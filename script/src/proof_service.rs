use crate::{
    api::ProofRequest,
    get_client, get_latest_checkpoint,
    types::{ProofId, ProofRequestState, ProofRequestStatus, ProofServiceError},
};
use anyhow::{anyhow, Context, Result};
use helios_consensus_core::{
    consensus_spec::MainnetConsensusSpec,
    types::{FinalityUpdate, LightClientHeader},
};
use helios_ethereum::rpc::ConsensusRpc;
use log::{debug, error, info, warn};
use std::env;
// Import Encodable for ProofRequest
use redis::{aio::ConnectionManager, AsyncCommands};
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

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
    latest_finalized_header: Arc<Mutex<LightClientHeader>>,
}

impl ProofService {
    /// Initialize a new ProofService with configuration from environment variables
    pub async fn new() -> anyhow::Result<Self> {
        // Ensure environment variables are loaded
        dotenv::dotenv().ok();

        // Read Redis configuration from environment variables with defaults
        let redis_url =
            env::var("REDIS_URL").context("REDIS_URL environment variable must be set")?;

        let redis_lock_duration_secs: u64 = match env::var("REDIS_LOCK_DURATION_SECS") {
            Ok(val) => match val.parse() {
                Ok(num) if num > 0 => num,
                Ok(_) => {
                    warn!("REDIS_LOCK_DURATION_SECS must be > 0, using default of 2");
                    2
                }
                Err(_) => {
                    warn!("REDIS_LOCK_DURATION_SECS not a valid number, using default of 2");
                    2
                }
            },
            Err(_) => {
                info!("REDIS_LOCK_DURATION_SECS not set, using default of 2");
                2
            }
        };

        let redis_key_prefix = match env::var("REDIS_KEY_PREFIX") {
            Ok(prefix) if !prefix.trim().is_empty() => prefix,
            _ => {
                info!("REDIS_KEY_PREFIX not set or empty, using default 'proof_service'");
                "proof_service".to_string()
            }
        };

        // Read polling interval configuration
        let header_check_interval_secs: u64 = match env::var("FINALIZED_HEADER_CHECK_INTERVAL_SECS")
        {
            Ok(val) => match val.parse() {
                Ok(num) if num > 0 => num,
                Ok(_) => {
                    warn!("FINALIZED_HEADER_CHECK_INTERVAL_SECS must be > 0, using default of 30");
                    30
                }
                Err(_) => {
                    warn!("FINALIZED_HEADER_CHECK_INTERVAL_SECS not a valid number, using default of 30");
                    30
                }
            },
            Err(_) => {
                info!("FINALIZED_HEADER_CHECK_INTERVAL_SECS not set, using default of 30");
                30
            }
        };

        // Log configuration
        info!("ProofService configuration:");
        info!(" - Redis URL: {}", redis_url);
        info!(" - Redis Lock Duration: {}s", redis_lock_duration_secs);
        info!(" - Redis Key Prefix: {}", redis_key_prefix);
        info!(
            " - Finalized Header Check Interval: {}s",
            header_check_interval_secs
        );

        // Fetch the latest finalized header with retries
        info!("Fetching latest finalized header...");

        const MAX_RETRIES: usize = 3;
        let mut retries = 0;
        let latest_finalized_header = loop {
            match Self::get_latest_finalized_header().await {
                Ok(header) => break header,
                Err(e) if retries < MAX_RETRIES => {
                    retries += 1;
                    warn!(
                        "Failed to get latest finalized header (attempt {}/{}): {}",
                        retries, MAX_RETRIES, e
                    );
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                }
                Err(e) => {
                    return Err(anyhow!(
                        "Failed to get latest finalized header after multiple attempts: {}",
                        e
                    ));
                }
            }
        };

        info!(
            "Latest finalized header retrieved successfully (slot: {}).",
            latest_finalized_header.beacon().slot
        );

        // Initialize Redis connection
        let client =
            redis::Client::open(redis_url.as_str()).context("Failed to create Redis client")?;

        let redis_conn_manager = match tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            ConnectionManager::new(client),
        )
        .await
        {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => return Err(anyhow!("Failed to connect to Redis: {}", e).into()),
            Err(_) => return Err(anyhow!("Timed out connecting to Redis after 5 seconds").into()),
        };

        // Create the service instance
        let service = Self {
            redis_conn_manager,
            latest_finalized_header: Arc::new(Mutex::new(latest_finalized_header)),
            redis_lock_duration: Duration::from_secs(redis_lock_duration_secs),
            redis_key_prefix,
            header_check_interval_secs,
        };

        info!("ProofService initialized successfully.");
        Ok(service)
    }

    /// Run the proof service, periodically checking for new finalized headers
    pub async fn run(&mut self) -> anyhow::Result<()> {
        info!(
            "Starting finalized header polling loop with interval of {}s",
            self.header_check_interval_secs
        );

        // Create the ticker for our polling interval
        let mut ticker =
            tokio::time::interval(Duration::from_secs(self.header_check_interval_secs));

        // Run the polling loop
        loop {
            // Wait for the next tick
            ticker.tick().await;

            debug!("Checking for new finalized header...");

            // Attempt to get the latest finalized header
            match Self::get_latest_finalized_header().await {
                Ok(new_header) => {
                    // Get the current header's slot number for comparison
                    let current_slot = {
                        let current_header = self.latest_finalized_header.lock().await;
                        current_header.beacon().slot
                    };

                    // Compare the new header with the current one
                    if new_header.beacon().slot > current_slot {
                        info!(
                            "New finalized header detected. Slot: {} -> {}",
                            current_slot,
                            new_header.beacon().slot
                        );

                        // Update the header
                        match self.update_finalized_header(new_header).await {
                            Ok(_) => info!("Updated finalized header successfully"),
                            Err(e) => error!("Failed to update finalized header: {}", e),
                        }
                    } else if new_header.beacon().slot < current_slot {
                        warn!(
                            "Received older finalized header. Current: {}, Received: {}",
                            current_slot,
                            new_header.beacon().slot
                        );
                    } else {
                        debug!("No change in finalized header (slot: {})", current_slot);
                    }
                }
                Err(e) => {
                    error!("Failed to fetch latest finalized header: {}", e);
                    // Continue the loop, will try again at next interval
                }
            }
        }
    }

    pub async fn request_proof(
        &self,
        request: ProofRequest,
    ) -> Result<(ProofId, ProofRequestStatus), ProofServiceError> {
        // in this function, we hold 2 locks:
        //  - redis lock for a specific proofId key
        //  - latest_finalized_header lock
        // These 2 entities are related parts of our state and we aim to avoid races between writing them
        let proof_id = ProofId::new(&request);
        let state_key = self.get_redis_state_key(proof_id);
        let lock_key = self.get_redis_lock_key(proof_id);

        // we want to hold this lock until this function exits because we can't update to the new
        // finalized head until we updated redis with this new request correctly
        let header = self.latest_finalized_header.lock().await;

        let finalized_execution_header = header.execution().map_err(|_| {
            ProofServiceError::Internal("Failed to get execution payload header".to_string())
        })?;
        let latest_finalized_block_number = *finalized_execution_header.block_number();

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

        let redis_state_key = self.get_redis_state_key(proof_id);
        let current_state = Self::get_stored_state(conn, &redis_state_key).await?;

        if acquired {
            // --- Lock Acquired ---
            log::info!("Lock acquired for new proof request: {:?}", proof_id);
            match current_state {
                Some(state) => match state.status {
                    // state exists and proof generation has errored before. We start a proof cycle anew here
                    ProofRequestStatus::Errored => {
                        let proof_status = self
                            .create_new_request_entry(
                                conn,
                                latest_finalized_block_number,
                                request,
                                state_key,
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
                            latest_finalized_block_number,
                            request,
                            state_key,
                            lock_key,
                        )
                        .await?;

                    Ok((proof_id, proof_status))
                }
            }
        } else {
            // --- Lock Not Acquired ---
            match current_state {
                Some(state) => Ok((proof_id, state.status)),
                // Lock exists, but state doesn't. This is unusual.
                // Could be transient issue, or the lock holder crashed before writing state.
                None => Err(ProofServiceError::LockContention(proof_id)),
            }
        }
    }

    /// This function assumes that the corresponding `redis_lock_key` is locked.
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
            // we can request a proof immediately and should do that here. It will continue in the backround and wait for proof to complete
            // Change status to ::Generating
            proof_state.status = ProofRequestStatus::Generating;
            conn.set::<_, _, ()>(&redis_state_key, serde_json::to_string(&proof_state)?)
                .await?;

            tokio::spawn(async move {
                Self::request_proof_zkvm().await;
            });
        } else {
            proof_state.status = ProofRequestStatus::WaitingForFinality;
            conn.set::<_, _, ()>(&redis_state_key, serde_json::to_string(&proof_state)?)
                .await?;
        }

        // todo: will this error if the lock is not present? Maybe we want that, cause a race might have happened
        conn.del::<_, ()>(&redis_lock_key).await?;
        Ok(proof_state.status)
    }

    /// Read all ProofRequestStates from redis with status `::WaitingForFinality`.
    /// For each, try to start ZK proof generation and update status to `::Generating`
    pub async fn update_finalized_header(
        &self,
        new_latest_finalized_header: LightClientHeader,
    ) -> Result<(), ProofServiceError> {
        // in this function, we hold 2 types of locks:
        //  - self.inner lock for a finalized header throught the whole function
        //  - redis locks for a specific proofId keys
        // These entities are related parts of our state and we aim to avoid races between writing them

        let mut latest_finalized_header = self.latest_finalized_header.lock().await;
        *latest_finalized_header = new_latest_finalized_header;
        let latest_finalized_block_number = *latest_finalized_header
            .execution()
            .map_err(|_| {
                ProofServiceError::Internal("failed to get execution payload header".to_string())
            })?
            .block_number();

        let requests_waiting_for_finality: Vec<ProofRequestState> =
            self.get_proofs_waiting_for_finality().await;

        // Store handles to join them later
        let mut handles = Vec::new();

        for proof_state in requests_waiting_for_finality {
            let proof_id = ProofId::new(&proof_state.request);
            let state_key = self.get_redis_state_key(proof_id);
            let lock_key = self.get_redis_lock_key(proof_id);
            let redis_lock_duration = self.redis_lock_duration;

            let mut conn = self.redis_conn_manager.clone();
            let handle = tokio::spawn(async move {
                Self::try_start_proof_generation(
                    &mut conn,
                    proof_state,
                    proof_id,
                    state_key,
                    lock_key,
                    redis_lock_duration,
                    latest_finalized_block_number,
                )
                .await
            });

            handles.push(handle);
        }

        // Wait for all individual Proof Requests to try to update before exiting the function and
        // releasing header lock
        for handle in handles {
            // todo: think about how we want to retry here if we catch an error. Maybe schedule
            // another `update_finalized_head` call in a couple seconds from the caller?
            let _ = handle.await;
        }

        Ok(())
    }

    // todo: think about how we want to retry here
    /// best-case effort to move exising ProofRequestState from ::WaitingForFinality to ::Generating
    async fn try_start_proof_generation(
        conn: &mut ConnectionManager,
        _proof_state: ProofRequestState,
        _proof_id: ProofId,
        redis_state_key: String,
        redis_lock_key: String,
        redis_lock_duration: Duration,
        latest_finalized_block_number: u64,
    ) {
        // Attempt to acquire distributed lock using SET NX EX
        let acquired: bool = redis::cmd("SET")
            .arg(&redis_lock_key)
            .arg("locked") // Value doesn't matter much, existence does
            .arg("NX") // Set only if key does not exist
            .arg("PX") // Set expiry in milliseconds
            .arg(redis_lock_duration.as_millis() as u64)
            .query_async(conn)
            .await
            .unwrap();

        if acquired {
            // Fetch current state to verify it's still waiting for finality
            if let Ok(Some(current_state)) = Self::get_stored_state(conn, &redis_state_key).await {
                if current_state.status == ProofRequestStatus::WaitingForFinality
                    && latest_finalized_block_number >= current_state.request.block_number
                {
                    // Update status to Generating
                    let mut updated_state = current_state;
                    updated_state.status = ProofRequestStatus::Generating;

                    conn.set::<_, _, ()>(
                        &redis_state_key,
                        serde_json::to_string(&updated_state).unwrap(),
                    )
                    .await
                    .unwrap();

                    // Spawn background task to generate proof
                    tokio::spawn(async move {
                        Self::request_proof_zkvm().await;
                    });
                }
            }

            // Release the lock in all cases
            let _: Result<(), _> = conn.del::<_, ()>(&redis_lock_key).await;
        } else {
            // skip this one. The state was present and now the lock is held. Someone else is
            // trying to update header and will request the proof for this entry
        }
    }

    // from redis
    async fn get_proofs_waiting_for_finality(&self) -> Vec<ProofRequestState> {
        // ! todo
        Vec::new()
    }

    /// request proof generation from a ZKVM
    async fn request_proof_zkvm() {
        // request proof with proper inputs and store result to redis
        // todo!();
    }

    /// Retrieves the full internal state of a proof request.
    /// Returns NotFound error if the request ID does not exist.
    pub async fn get_proof_state(
        &self,
        id: ProofId,
    ) -> Result<ProofRequestState, ProofServiceError> {
        let conn = &mut self.redis_conn_manager.clone();

        let redis_state_key = self.get_redis_state_key(id);
        match Self::get_stored_state(conn, &redis_state_key).await {
            Ok(state) => match state {
                Some(state) => Ok(state),
                None => Err(ProofServiceError::NotFound(id)),
            },
            Err(e) => Err(e),
        }
    }

    /// Internal helper to fetch and deserialize the state from Redis.
    async fn get_stored_state(
        conn: &mut ConnectionManager,
        redis_state_key: &String,
    ) -> Result<Option<ProofRequestState>, ProofServiceError> {
        let state_json: Option<String> = conn.get(redis_state_key).await?;

        match state_json {
            Some(json) => {
                let state: ProofRequestState = serde_json::from_str(&json)?;
                Ok(Some(state))
            }
            None => Ok(None),
        }
    }

    /// Generates the Redis key for storing the state of a specific proof request.
    fn get_redis_state_key(&self, id: ProofId) -> String {
        // Use hex representation of the B256 hash for the key
        format!("{}:state:{}", self.redis_key_prefix, id.to_hex_string())
    }

    /// Generates the Redis key used for locking a specific proof request during processing.
    fn get_redis_lock_key(&self, id: ProofId) -> String {
        // Use hex representation of the B256 hash for the key
        format!("{}:lock:{}", self.redis_key_prefix, id.to_hex_string())
    }

    /// Fetches the latest finalized LightClientHeader for use with ProofService
    async fn get_latest_finalized_header() -> Result<LightClientHeader, Box<dyn std::error::Error>>
    {
        // Get the latest checkpoint
        let checkpoint = get_latest_checkpoint().await;

        // Create a client from the checkpoint
        let client = get_client(checkpoint).await;

        // Get the finality update
        let finality_update: FinalityUpdate<MainnetConsensusSpec> =
            client.rpc.get_finality_update().await?;

        // Extract the finalized header
        let latest_finalized_header = finality_update.finalized_header();

        Ok(latest_finalized_header.clone())
    }
}
