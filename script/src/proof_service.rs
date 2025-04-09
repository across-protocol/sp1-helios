use crate::{
    api::ProofRequest,
    get_checkpoint, get_client, get_latest_checkpoint, get_updates,
    types::{ProofId, ProofRequestState, ProofRequestStatus, ProofServiceError},
};
use alloy::{
    network::Ethereum,
    providers::{ProviderBuilder, RootProvider},
    transports::http::Http,
};
use anyhow::{anyhow, Context, Result};
use helios_consensus_core::{
    consensus_spec::MainnetConsensusSpec,
    types::{FinalityUpdate, LightClientHeader},
};
use helios_ethereum::rpc::ConsensusRpc;
use log::{debug, error, info, warn};
use redis::{aio::ConnectionManager, AsyncCommands};
use reqwest::{Client, Url};
use sp1_sdk::SP1Stdin;
use std::env;
use std::time::Duration;

/// Service responsible for managing the lifecycle of ZK proof generation requests.
///
/// It uses Redis for state management and locking, and interacts with an
/// external asynchronous function to trigger the actual proof computation.
#[derive(Clone)]
pub struct ProofService {
    redis_lock_duration: Duration,
    redis_key_prefix: String,
    redis_conn_manager: ConnectionManager,
    header_check_interval_secs: u64,
    // required for storage slot merkle proving
    source_chain_provider: RootProvider<Http<Client>>,
}

impl ProofService {
    /// Initialize a new ProofService with configuration from environment variables
    pub async fn new() -> anyhow::Result<Self> {
        // Ensure environment variables are loaded
        dotenv::dotenv().ok();

        let source_execution_rpc_url: Url = env::var("SOURCE_EXECUTION_RPC_URL")
            .expect("SOURCE_EXECUTION_RPC_URL not set")
            .parse()
            .unwrap();

        let source_chain_provider = ProviderBuilder::new()
            .network::<Ethereum>()
            .on_http(source_execution_rpc_url);

        let redis_url = env::var("REDIS_URL").expect("REDIS_URL environment variable must be set");
        let redis_lock_duration_secs: u64 = env::var("REDIS_LOCK_DURATION_SECS")
            .expect("REDIS_LOCK_DURATION_SECS environment variable must be set")
            .parse()
            .expect("REDIS_LOCK_DURATION_SECS environment variable must be a number");
        let redis_key_prefix = env::var("REDIS_KEY_PREFIX")
            .expect("REDIS_KEY_PREFIX environment variable must be set");

        let header_check_interval_secs: u64 = match env::var("FINALIZED_HEADER_CHECK_INTERVAL_SECS")
        {
            Ok(val) => match val.parse() {
                Ok(num) if num > 0 => num,
                Ok(_) => {
                    panic!("FINALIZED_HEADER_CHECK_INTERVAL_SECS must be > 0");
                }
                Err(_) => {
                    panic!("FINALIZED_HEADER_CHECK_INTERVAL_SECS not a valid number");
                }
            },
            Err(_) => {
                panic!("FINALIZED_HEADER_CHECK_INTERVAL_SECS not set");
            }
        };

        info!("ProofService configuration:");
        info!(" - Redis URL: {}", redis_url);
        info!(" - Redis Lock Duration: {}s", redis_lock_duration_secs);
        info!(" - Redis Key Prefix: {}", redis_key_prefix);
        info!(
            " - Finalized Header Check Interval: {}s",
            header_check_interval_secs
        );

        let client =
            redis::Client::open(redis_url.as_str()).context("Failed to create Redis client")?;

        let redis_conn_manager = match tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            ConnectionManager::new(client),
        )
        .await
        {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => return Err(anyhow!("Failed to connect to Redis: {}", e)),
            Err(_) => return Err(anyhow!("Timed out connecting to Redis after 5 seconds")),
        };

        let service = Self {
            redis_conn_manager,
            redis_lock_duration: Duration::from_secs(redis_lock_duration_secs),
            redis_key_prefix,
            header_check_interval_secs,
            source_chain_provider,
        };

        info!("ProofService initialized successfully.");
        Ok(service)
    }

    /// Run the proof service, periodically checking for new finalized headers
    pub async fn run(self) -> anyhow::Result<()> {
        info!(
            "Starting finalized header polling loop with interval of {}s",
            self.header_check_interval_secs
        );

        loop {
            debug!("Checking for new finalized header...");

            match Self::get_latest_finality_update().await {
                Ok(finality_update) => {
                    // Retry up to 3 times with 1 second interval
                    let mut attempts = 0;
                    let max_attempts = 3;

                    while attempts < max_attempts {
                        match self
                            .try_update_finalized_header(finality_update.clone())
                            .await
                        {
                            Ok(_) => break,
                            Err(e) => {
                                attempts += 1;

                                if attempts >= max_attempts {
                                    error!(
                                        "Failed to update finalized header after {} attempts: {}",
                                        max_attempts, e
                                    );
                                } else {
                                    debug!("Attempt {}/{} to update finalized header failed, retrying in 1s", 
                                           attempts, max_attempts);
                                    tokio::time::sleep(Duration::from_secs(1)).await;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to fetch latest finalized header: {}", e);
                    // Continue the loop, will try again after sleeping
                }
            }

            tokio::time::sleep(Duration::from_secs(self.header_check_interval_secs)).await;
        }
    }

    async fn try_update_finalized_header(
        &self,
        new_finality_update: FinalityUpdate<MainnetConsensusSpec>,
    ) -> Result<bool, ProofServiceError> {
        let acquired = self.try_acquire_redis_lock().await?;
        if !acquired {
            // Should be a pretty rare occurence, so it's fine to just return error here and let the caller try again
            return Err(ProofServiceError::Internal(
                "Failed to acquire redis lock".to_string(),
            ));
        }

        let result = self
            .try_update_finalized_header_inner(new_finality_update)
            .await;

        // best effort to delete the lock
        let _: Result<(), _> = (self.redis_conn_manager.clone())
            .del::<_, ()>(self.get_redis_lock_key())
            .await;

        result
    }

    /// This function assumes redis lock is held
    async fn try_update_finalized_header_inner(
        &self,
        new_finality_update: FinalityUpdate<MainnetConsensusSpec>,
    ) -> Result<bool, ProofServiceError> {
        let finalized_header = self.get_finalized_header().await?;
        // todo: here, we're trusting that our light client object already checked this update for
        // validity, so we don't perform any validity checks here. Is this correct?
        let should_update_header: bool = match finalized_header {
            Some(header) => {
                header.beacon().slot < new_finality_update.finalized_header().beacon().slot
            }
            None => true,
        };

        if should_update_header {
            // - gather requests that are ::WaitingForFinality
            // - decide which require updating
            // - set new header and update all statuses all in the same call to redis. Meaning that if one reverts -- all change should revert

            let conn = &mut self.redis_conn_manager.clone();

            // 1. Gather requests that are ::WaitingForFinality
            let waiting_requests = self.get_proofs_waiting_for_finality().await?;

            // 2. Decide which requests require updating based on the new finalized header
            let finalized_header = new_finality_update.finalized_header();
            let finalized_block_number = match finalized_header.execution() {
                Ok(execution_header) => *execution_header.block_number(),
                Err(_) => {
                    return Err(ProofServiceError::Internal(
                        "Failed to get execution header from finality update".to_string(),
                    ));
                }
            };
            let finalized_slot = finalized_header.beacon().slot;

            // Prepare a transaction to update everything atomically
            let mut pipe = redis::pipe();
            pipe.atomic();

            // Add the new finalized header to the transaction
            pipe.set(
                self.get_redis_key_finalized_header(),
                serde_json::to_string(&finalized_header)?,
            );

            // Track which requests need to be processed further
            let mut requests_to_process = Vec::new();

            // Add updates for each eligible request
            for proof_state in waiting_requests {
                if proof_state.status == ProofRequestStatus::WaitingForFinality
                    && finalized_block_number >= proof_state.request.block_number
                    && finalized_slot > proof_state.request.valid_contract_head
                {
                    // Update status to Generating
                    let mut updated_state = proof_state.clone();

                    // TODO: Think about concurrency and fault-tolerance around this ::Generating thing. If we set redis key
                    // to generating and the server immediately crashes, what should we do? We should somehow know which tasks
                    // we currenly have in the system, and for each task that should be processing but is not, we should start
                    // processing it. Should have Some Self { Vec<Task> }. Task will have some metadata and an associated joinHandle.
                    // Then for every Request that has status ::Generating, we should have a task. Otherwise, spin up new task.
                    // However, what if there's another instance of ProofService doing that? We could have some sort of per-task redis lock
                    // that should be renewed by an active task that is handling ::Generating tasks logic. Then, we'd try to start a tokio thread
                    // for each task that has a status ::Generating and for the ones that are locked, we'd just back off.
                    updated_state.status = ProofRequestStatus::Generating;

                    let redis_key = self.get_redis_key_proof(&ProofId::new(&proof_state.request));
                    pipe.set(&redis_key, serde_json::to_string(&updated_state)?);

                    // Save for later processing after transaction completes
                    requests_to_process.push(proof_state.request.clone());
                }
            }
            // Execute the transaction
            pipe.query_async::<()>(conn).await?;

            // Now that the transaction has completed successfully, spawn tasks to generate proofs
            for request in requests_to_process {
                let source_chain_provider = self.source_chain_provider.clone();
                tokio::spawn(async move {
                    let _ = Self::generate_and_store_proof(request, source_chain_provider).await;
                });
            }

            // todo: better lock deletion logic
            let _: Result<(), _> = conn.del::<_, ()>(self.get_redis_lock_key()).await;
            return Ok(true);
        }

        // todo: better lock deletion logic
        let _: Result<(), _> = (self.redis_conn_manager.clone())
            .del::<_, ()>(self.get_redis_lock_key())
            .await;

        // did not update header
        Ok(false)
    }

    pub async fn request_proof(
        &self,
        request: ProofRequest,
    ) -> Result<(ProofId, ProofRequestStatus), ProofServiceError> {
        let proof_id = ProofId::new(&request);
        let lock_key = self.get_redis_lock_key();
        let acquired = self.try_acquire_redis_lock().await?;
        // TODO: at every point after we made this call, if we exit due to any error, we want to delete the lock value. How to do that? Some kind of defer would be nice
        // Otherwise, if this function returns an error, we might not unlock redis, which is sad

        let proof_request_state = self.get_proof_request_state(&proof_id).await?;

        if acquired {
            // --- Lock Acquired ---
            log::info!("Lock acquired for new proof request: {:?}", proof_id);

            let finalized_header = self.get_finalized_header().await?.ok_or_else(|| {
                ProofServiceError::Internal(
                    "No finalized header available in redis. Wait and try again".to_string(),
                )
            })?;

            let finalized_block_number = *finalized_header
                .execution()
                .map_err(|_| {
                    ProofServiceError::Internal("Failed to get execution header.".to_string())
                })?
                .block_number();

            let conn = &mut self.redis_conn_manager.clone();
            match proof_request_state {
                Some(state) => match state.status {
                    // state exists and proof generation has errored before. We start a proof cycle anew here
                    ProofRequestStatus::Errored => {
                        let proof_status = self
                            .create_new_request_entry(
                                conn,
                                finalized_block_number,
                                request,
                                self.get_redis_key_proof(&proof_id),
                                lock_key,
                            )
                            .await?;
                        Ok((proof_id, proof_status))
                    }
                    // state exists and generation has not errored. Return current status
                    _ => Ok((proof_id, state.status)),
                },
                None => {
                    let proof_status = self
                        .create_new_request_entry(
                            conn,
                            finalized_block_number,
                            request,
                            self.get_redis_key_proof(&proof_id),
                            lock_key,
                        )
                        .await?;

                    Ok((proof_id, proof_status))
                }
            }
        } else {
            // --- Lock Not Acquired ---
            match proof_request_state {
                Some(state) => Ok((proof_id, state.status)),
                // Lock exists, but state doesn't. This is unusual.
                // Could be transient issue, or the lock holder crashed before writing state.
                None => Err(ProofServiceError::LockContention(proof_id)),
            }
        }
    }

    /// This function assumes that the corresponding `redis_lock_key` is set for the duration.
    /// Called after checking the proper conditions for starting a proof generation sequence.
    async fn create_new_request_entry(
        &self,
        conn: &mut ConnectionManager,
        latest_finalized_block: u64,
        request: ProofRequest,
        redis_state_key: String,
        redis_lock_key: String,
    ) -> Result<ProofRequestStatus, ProofServiceError> {
        let mut proof_state = ProofRequestState::new(request.clone());

        if latest_finalized_block >= request.block_number {
            // finality condition for request is already satisfied. Start proof generation right away
            // Change status to ::Generating
            proof_state.status = ProofRequestStatus::Generating;
            conn.set::<_, _, ()>(&redis_state_key, serde_json::to_string(&proof_state)?)
                .await?;

            let source_chain_provider = self.source_chain_provider.clone();
            tokio::spawn(async move {
                let _ = Self::generate_and_store_proof(request, source_chain_provider).await;
            });
        } else {
            proof_state.status = ProofRequestStatus::WaitingForFinality;
            conn.set::<_, _, ()>(&redis_state_key, serde_json::to_string(&proof_state)?)
                .await?;
        }

        // todo: we want caller to delete the lock ideally through some sort of defer mechanism
        // todo: will this error if the lock is not present? Maybe we want that, cause a race might have happened
        conn.del::<_, ()>(&redis_lock_key).await?;
        Ok(proof_state.status)
    }

    /// Retrieves all proof requests that are in the WaitingForFinality status from Redis
    ///
    /// This function scans Redis for all keys with the configured prefix that store
    /// proof request states, deserializes them, and filters for those with
    /// WaitingForFinality status.
    async fn get_proofs_waiting_for_finality(
        &self,
    ) -> Result<Vec<ProofRequestState>, ProofServiceError> {
        let mut conn = self.redis_conn_manager.clone();
        let pattern = format!("{}:state:*", self.redis_key_prefix);

        // Use SCAN to efficiently iterate through keys matching our pattern
        let mut cursor = 0;
        let mut waiting_proofs = Vec::new();

        loop {
            // SCAN returns (next_cursor, [keys])
            let (next_cursor, keys): (i64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(100) // Process in batches of 100 keys
                .query_async(&mut conn)
                .await
                .map_err(ProofServiceError::RedisError)?;

            // Use MGET to fetch all values in a single round trip
            if !keys.is_empty() {
                let state_jsons: Vec<Option<String>> = conn
                    .get(keys.clone())
                    .await
                    .map_err(ProofServiceError::RedisError)?;

                // Process the results
                for (i, maybe_json) in state_jsons.into_iter().enumerate() {
                    if let Some(state_json) = maybe_json {
                        match serde_json::from_str::<ProofRequestState>(&state_json) {
                            Ok(state) if state.status == ProofRequestStatus::WaitingForFinality => {
                                waiting_proofs.push(state);
                            }
                            Ok(_) => {
                                // Not in WaitingForFinality status, skip
                            }
                            Err(e) => {
                                // Log deserialization error but continue processing other keys
                                warn!(
                                    "Failed to deserialize proof request state from {}: {}",
                                    keys[i], e
                                );
                            }
                        }
                    }
                }
            }

            // If cursor is 0, we've completed the scan
            cursor = next_cursor;
            if cursor == 0 {
                break;
            }
        }

        debug!(
            "Found {} proof requests waiting for finality",
            waiting_proofs.len()
        );
        Ok(waiting_proofs)
    }

    /// request proof generation from a ZKVM
    async fn generate_and_store_proof(
        request: ProofRequest,
        source_chain_provider: RootProvider<Http<Client>>,
    ) -> anyhow::Result<()> {
        // This function is limited to ONE instance per proof request per all possible machines
        // AND the current status in Redis is ::Generating. This function has full control over
        // this proof request entry and shouldn't need any locking

        // Fetch the checkpoint at that slot
        let checkpoint = get_checkpoint(request.valid_contract_head).await;

        // Get the client from the checkpoint
        let client = get_client(checkpoint).await;

        let mut stdin = SP1Stdin::new();

        // Setup client.
        let mut sync_committee_updates = get_updates(&client).await;
        let finality_update: FinalityUpdate<MainnetConsensusSpec> =
            client.rpc.get_finality_update().await.unwrap();

        let latest_finalized_header = finality_update.finalized_header();

        let latest_finalized_execution_header = latest_finalized_header
            .execution()
            .map_err(|_e| anyhow::anyhow!("Failed to get execution payload header"))?;

        Ok(())
    }

    async fn try_acquire_redis_lock(&self) -> Result<bool, ProofServiceError> {
        let lock_key = self.get_redis_lock_key();
        let conn = &mut self.redis_conn_manager.clone();
        // Attempt to acquire distributed lock using SET NX EX
        let acquired: bool = redis::cmd("SET")
            .arg(&lock_key)
            .arg("locked") // Value doesn't matter much, existence does
            .arg("NX") // Set only if key does not exist
            .arg("PX") // Set expiry in milliseconds
            .arg(self.redis_lock_duration.as_millis() as u64)
            .query_async(conn)
            .await?;

        Ok(acquired)
    }

    pub async fn get_proof_request_state(
        &self,
        id: &ProofId,
    ) -> Result<Option<ProofRequestState>, ProofServiceError> {
        let conn = &mut self.redis_conn_manager.clone();
        Self::get_value_from_redis(conn, &self.get_redis_key_proof(id)).await
    }

    async fn get_finalized_header(&self) -> Result<Option<LightClientHeader>, ProofServiceError> {
        let conn = &mut self.redis_conn_manager.clone();
        Self::get_value_from_redis(conn, &self.get_redis_key_finalized_header()).await
    }

    /// Internal helper to fetch and deserialize the state from Redis.
    async fn get_value_from_redis<T>(
        conn: &mut ConnectionManager,
        redis_state_key: &String,
    ) -> Result<Option<T>, ProofServiceError>
    where
        T: for<'de> serde::Deserialize<'de>,
    {
        let state_json: Option<String> = conn.get(redis_state_key).await?;

        match state_json {
            Some(json) => {
                let state: T = serde_json::from_str(&json)?;
                Ok(Some(state))
            }
            None => Ok(None),
        }
    }

    /// Generates the Redis key for storing the state of a specific proof request.
    fn get_redis_key_proof(&self, id: &ProofId) -> String {
        // Use hex representation of the B256 hash for the key
        format!("{}:state:{}", self.redis_key_prefix, id.to_hex_string())
    }

    /// Generates the Redis key for storing the latest finalized header.
    fn get_redis_key_finalized_header(&self) -> String {
        format!("{}:state:finalized_header", self.redis_key_prefix)
    }

    /// Generates a global redis lock for ProofService prefix.
    fn get_redis_lock_key(&self) -> String {
        format!("{}:lock", self.redis_key_prefix)
    }

    // ! todo: I think, in this function, I should do something like:
    // ! 1. have light_client: Inner<MainnetConsensusSpec, HttpRpc> as a member var of ProofService
    // ! 2. fetch all the finality updates since my client's latest recorded data in client.store.
    // ! 3. verify and apply all updates consecutively. In the code above, work with client.store values, rather than querying anything from RPC.
    /// Fetches the latest finalized LightClientHeader for use with ProofService
    async fn get_latest_finality_update() -> anyhow::Result<FinalityUpdate<MainnetConsensusSpec>> {
        // TODO: just use bootstrap slot from .env or something else ... IDK yet
        // Yeah, maybe some recent finalized slot?
        // get_latest_checkpoint seems to be returning rather old data
        let checkpoint = get_latest_checkpoint().await;
        let client = get_client(checkpoint).await;

        client
            .rpc
            .get_finality_update()
            .await
            .map_err(|e| anyhow!("get_latest_finality_update error: {}", e))
    }
}
