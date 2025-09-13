use crate::{
    api::{FinalizedHeaderResponse, ProofRequest},
    consensus_client::{self, ConfigExt},
    proof_backends::ProofBackend,
    redis_store::RedisStore,
    rpc_proxies::execution::{self},
    types::{
        ContractStorageBuilder, ProofId, ProofRequestState, ProofRequestStatus, ProofServiceError,
    },
    util::CancellationTokenGuard,
};
use alloy::transports::BoxFuture;
use alloy_primitives::B256;
use anyhow::{anyhow, Context, Result};
use helios_consensus_core::{
    consensus_spec::{ConsensusSpec, MainnetConsensusSpec},
    types::LightClientHeader,
};
use helios_ethereum::rpc::{http_rpc::HttpRpc, ConsensusRpc};
use serde::{de::DeserializeOwned, Serialize};
use sp1_helios_primitives::types::ProofInputs;
use tree_hash::TreeHash;

use std::str::FromStr;
use std::time::Duration;
use std::{env, sync::Arc};
use tokio::time::{interval_at, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

const FINALIZED_HEADER_AGE_BUFFER_SECS: u64 = 180; // 3 minutes
const MAX_FINALIZED_HEADER_AGE: std::time::Duration =
    Duration::from_secs(12 * 32 * 3 + FINALIZED_HEADER_AGE_BUFFER_SECS); // max legit finalized header age: 3 epochs + buffer to allow for an update to happen
const INITIAL_PROOF_GENERATION_LOCK_DURATION_MS: u64 = 1000;
const MAX_GET_PROOF_INPUTS_ATTEMPTS: i32 = 3;

/// Service responsible for managing the lifecycle of ZK proof generation requests.
///
/// It uses Redis for state management and locking, and interacts with an
/// external asynchronous function to trigger the actual proof computation.
#[derive(Clone)]
pub struct ProofService<B>
where
    B: ProofBackend + Clone + Send + Sync + 'static,
{
    execution_rpc_proxy: execution::Proxy,
    proof_backend: B,
    redis_store: RedisStore<B::ProofOutput>,
}

// --- API-facing functionality of ProofService ---
impl<B> ProofService<B>
where
    B: ProofBackend + Clone + Send + Sync,
{
    pub fn vkey_digest_bytes(&self) -> Vec<u8> {
        self.proof_backend.vkey_digest()
    }

    pub async fn get_proof(
        &mut self,
        id: &ProofId,
    ) -> Result<Option<ProofRequestState<B::ProofOutput>>, ProofServiceError> {
        self.redis_store.get_proof_state(id).await
    }

    /// Retrieves the latest finalized header details from Redis.
    pub async fn get_finalized_header_details(
        &mut self,
    ) -> Result<Option<FinalizedHeaderResponse>, ProofServiceError> {
        match self.redis_store.read_finalized_header().await? {
            Some(header) => {
                let slot = header.beacon().slot;
                let checkpoint = header.beacon().tree_hash_root();
                let response = FinalizedHeaderResponse {
                    slot,
                    checkpoint: checkpoint.to_string(), // Convert B256 to hex string
                };
                Ok(Some(response))
            }
            None => Ok(None),
        }
    }

    pub async fn request_proof(
        &mut self,
        request: ProofRequest,
    ) -> Result<(ProofId, ProofRequestStatus), ProofServiceError> {
        self.with_global_lock(request, |proof_service, request, lock_acquired| {
            Box::pin(async move {
                proof_service
                    .process_request_locked(request, lock_acquired)
                    .await
            })
        })
        .await
    }

    async fn process_request_locked(
        &mut self,
        request: ProofRequest,
        redis_lock_acquired: bool,
    ) -> Result<(ProofId, ProofRequestStatus), ProofServiceError> {
        let proof_id = ProofId::new(&request);
        let proof_request_state = self.get_proof(&proof_id).await?;

        if redis_lock_acquired {
            tracing::debug!(target: "proof_service::api", "Global lock acquired for new proof request ID: {}", proof_id.to_hex_string());

            let finalized_header =
                self.redis_store
                    .read_finalized_header()
                    .await?
                    .ok_or_else(|| {
                        ProofServiceError::Internal(
                            "No finalized header available in redis. Wait and try again"
                                .to_string(),
                        )
                    })?;

            let finalized_block_number = *finalized_header
                .execution()
                .map_err(|_| {
                    ProofServiceError::Internal("Failed to get execution header.".to_string())
                })?
                .block_number();

            match proof_request_state {
                Some(state) => match state.status {
                    ProofRequestStatus::Errored => {
                        let proof_status = self
                            .initialize_request_locked(
                                finalized_header.beacon().slot,
                                finalized_block_number,
                                request,
                            )
                            .await?;
                        Ok((proof_id, proof_status))
                    }
                    _ => Ok((proof_id, state.status)),
                },
                None => {
                    let proof_status = self
                        .initialize_request_locked(
                            finalized_header.beacon().slot,
                            finalized_block_number,
                            request,
                        )
                        .await?;
                    Ok((proof_id, proof_status))
                }
            }
        } else {
            // --- Lock Not Acquired ---
            match proof_request_state {
                Some(state) => Ok((proof_id, state.status)),
                // Lock exists, but state doesn't. This is either a race with other worker, or the
                // other worker crashed before releasing the lock and the lock has not expired
                None => Err(ProofServiceError::LockContention(proof_id)),
            }
        }
    }

    /// This function assumes that the corresponding global lock is held.
    /// Called after checking the proper conditions for starting a proof generation sequence.
    async fn initialize_request_locked(
        &mut self,
        latest_finalized_slot: u64,
        latest_finalized_block: u64,
        request: ProofRequest,
    ) -> Result<ProofRequestStatus, ProofServiceError> {
        let proof_id = ProofId::new(&request);
        let mut proof_state = ProofRequestState::new(request.clone());

        if latest_finalized_block >= request.src_chain_block_number
            && latest_finalized_slot > request.dst_chain_contract_from_head
        {
            // Finality condition met, set status to Generating
            proof_state.status = ProofRequestStatus::Generating;
            self.redis_store
                .set_proof_state(&proof_id, &proof_state)
                .await?;

            let proof_service_clone = self.clone();
            // Use redis_store to acquire the proof generation lock
            match self
                .redis_store
                .acquire_proof_generation_lock(&proof_id, INITIAL_PROOF_GENERATION_LOCK_DURATION_MS)
                .await
            {
                Ok(true) => {
                    // Lock acquired, spawn generation task.
                    tokio::spawn(async move {
                        Self::generate_and_store_proof(request, proof_service_clone, proof_id)
                            .await;
                    });
                }
                Ok(false) => {
                    // Lock already exists
                    debug!(
                        target: "proof_service::api",
                        "Skipping proof generation spawn for ID: {}, lock already held.", proof_id.to_hex_string()
                    );
                }
                Err(e) => {
                    // Error acquiring lock
                    warn!(
                        target: "proof_service::api",
                        "Failed to acquire lock for proof generation ID: {}: {:#?}", proof_id.to_hex_string(), e
                    );
                }
            }
        } else {
            // Finality condition not met, set status to WaitingForFinality
            proof_state.status = ProofRequestStatus::WaitingForFinality;
            self.redis_store
                .set_proof_state(&proof_id, &proof_state)
                .await?;
        }

        Ok(proof_state.status)
    }

    /// Tries to apply `latest_finalized_header` to redis state.
    async fn process_new_finalized_header(
        &mut self,
        latest_finalized_header: LightClientHeader,
    ) -> Result<bool, ProofServiceError> {
        self.with_global_lock(
            latest_finalized_header,
            |this, latest_finalized_header, lock_acquired: bool| {
                Box::pin(async move {
                    if lock_acquired {
                        this.process_new_finalized_header_locked(latest_finalized_header)
                            .await?;
                        // If the above function did not error, stored state in redis is equal or newer than the proposed latest_finalized_header
                        Ok(true)
                    } else {
                        debug!(target: "proof_service::run", "Could not acquire global lock to process new finalized header. Another worker might be processing.");
                        // If we failed to acquire the lock, we're not sure what the redis state is and if it's equal or newer to latest_finalized_header. Catiously return false
                        Ok(false)
                    }
                })
            },
        )
        .await
    }

    /// Same as `process_new_finalized_header`, but assumes the redis lock is held
    async fn process_new_finalized_header_locked(
        &mut self,
        latest_finalized_header: LightClientHeader,
    ) -> Result<(), ProofServiceError> {
        let stored_finalized_header = self.redis_store.read_finalized_header().await?;
        let should_update_header: bool = match stored_finalized_header {
            Some(redis_header) => {
                redis_header.beacon().slot < latest_finalized_header.beacon().slot
            }
            None => true,
        };

        if should_update_header {
            let waiting_requests = self
                .redis_store
                .find_requests_by_status(ProofRequestStatus::WaitingForFinality)
                .await?;

            let finalized_block_number = match latest_finalized_header.execution() {
                Ok(execution_header) => *execution_header.block_number(),
                Err(_) => {
                    return Err(ProofServiceError::Internal(
                        "Failed to get execution header from finality update".to_string(),
                    ));
                }
            };
            let finalized_slot = latest_finalized_header.beacon().slot;

            let mut updated_states = Vec::new();
            let mut requests_to_process_further = Vec::new();

            for proof_state in waiting_requests {
                if proof_state.status == ProofRequestStatus::WaitingForFinality
                    && finalized_block_number >= proof_state.request.src_chain_block_number
                    && finalized_slot > proof_state.request.dst_chain_contract_from_head
                {
                    let mut updated_state = proof_state.clone();
                    updated_state.status = ProofRequestStatus::Generating;
                    let proof_id = ProofId::new(&updated_state.request);
                    updated_states.push((proof_id, updated_state));
                    requests_to_process_further.push(proof_state.request.clone());
                }
            }

            // Use redis_store to update header and states atomically
            self.redis_store
                .update_finalized_header_and_proof_states(&latest_finalized_header, updated_states)
                .await?;

            // Spawn tasks for requests that moved to Generating
            for request in requests_to_process_further {
                let proof_id = ProofId::new(&request);
                let proof_service_clone = self.clone();
                match self
                    .redis_store
                    .acquire_proof_generation_lock(
                        &proof_id,
                        INITIAL_PROOF_GENERATION_LOCK_DURATION_MS,
                    )
                    .await
                {
                    Ok(true) => {
                        tokio::spawn(async move {
                            Self::generate_and_store_proof(
                                request,
                                proof_service_clone,
                                proof_id, // Pass proof_id
                            )
                            .await;
                        });
                    }
                    Ok(false) => {
                        // Lock was not acquired
                        debug!(
                            target: "proof_service::state",
                            "Skipping proof generation spawn for ID: {}, lock already held.",
                            proof_id.to_hex_string()
                        );
                    }
                    Err(e) => {
                        // Redis returned an error while trying to acquire lock
                        warn!(
                            target: "proof_service::state",
                            "Failed to acquire lock for proof generation spawn ID: {}: {:#?}",
                            proof_id.to_hex_string(),
                            e
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /*
    Strategy for getting genuine checkpoint:
    1. Try get finalized header from redis. If exists, use header.beacon.tree_hash_root()
    2. If not in redis, try get INIT_CHECKPOINT env var.
    3. If neither is present, fail.
    */
    async fn get_initial_checkpoint(&mut self) -> anyhow::Result<B256> {
        // 1. Try Redis
        match self.redis_store.read_finalized_header().await {
            Ok(Some(header)) => {
                let checkpoint = header.beacon().tree_hash_root();
                info!(target: "proof_service::init", "Using checkpoint calculated from Redis finalized header (Slot {}): {}", header.beacon().slot, checkpoint);
                return Ok(checkpoint);
            }
            Ok(None) => {
                info!(target: "proof_service::init", "Finalized header not found in Redis. Falling back to env var.");
            }
            Err(e) => {
                warn!(target: "proof_service::init", "Error reading finalized header from Redis: {:#?}. Falling back to env var.", e);
                return Err(anyhow!("{:#?}", e));
            }
        }

        // 2. Try Env Var
        match env::var("INIT_CHECKPOINT") {
            Ok(checkpoint_hex) => {
                info!(target: "proof_service::init", "Using checkpoint from env var INIT_CHECKPOINT: {}", checkpoint_hex);
                let checkpoint = B256::from_str(&checkpoint_hex).map_err(|e| {
                    anyhow!(
                        "Invalid INIT_CHECKPOINT format: {} - Error: {}",
                        checkpoint_hex,
                        e
                    )
                })?;
                Ok(checkpoint)
            }
            Err(_) => {
                info!(target: "proof_service::init", "Checkpoint not found in env var INIT_CHECKPOINT.");
                // 3. Fail
                Err(anyhow::anyhow!(
                     "Failed to get initial checkpoint. Not found in Redis and INIT_CHECKPOINT env var is not set."
                 ))
            }
        }
    }
}

// --- Runtime logic required for proof generation ---
impl<B> ProofService<B>
where
    B: ProofBackend + Clone + Send + Sync,
{
    pub async fn new(proof_backend: B) -> anyhow::Result<Self> {
        let redis_store = RedisStore::new().await?;
        let execution_rpc_proxy = execution::Proxy::try_from_env()?;

        let service = Self {
            execution_rpc_proxy,
            proof_backend,
            redis_store,
        };

        info!(target: "proof_service::init", "ProofService initialized successfully.");
        Ok(service)
    }

    async fn get_proof_inputs(&self, request: &ProofRequest) -> anyhow::Result<ProofInputs> {
        let mut attempt = 0;
        loop {
            match self.get_proof_inputs_once(request).await {
                Ok(inputs) => return Ok(inputs),
                Err(e) => {
                    attempt += 1;
                    let proof_id_hex = ProofId::new(request).to_hex_string();
                    if attempt >= MAX_GET_PROOF_INPUTS_ATTEMPTS {
                        warn!(target: "proof_service::proof_inputs", "All {} attempts exhausted for proof id {}. Last error: {:#?}", MAX_GET_PROOF_INPUTS_ATTEMPTS, proof_id_hex, e);
                        return Err(anyhow!(
                            "Failed to get proof inputs after {} attempts for proof id {}: {:#?}",
                            MAX_GET_PROOF_INPUTS_ATTEMPTS,
                            proof_id_hex,
                            e
                        ));
                    } else {
                        warn!(target: "proof_service::proof_inputs", "Attempt {} failed for proof id {}: {:#?}. Retrying in 12s...", attempt, proof_id_hex, e);
                        tokio::time::sleep(Duration::from_secs(12)).await;
                    }
                }
            }
        }
    }

    async fn get_proof_inputs_once(&self, request: &ProofRequest) -> anyhow::Result<ProofInputs> {
        let consensus_proof_inputs =
            consensus_client::get_proof_inputs::<MainnetConsensusSpec>(request).await?;

        let latest_finalized_header = consensus_proof_inputs.finality_update.finalized_header();
        let expected_current_slot = consensus_proof_inputs.expected_current_slot;

        // Fetch execution layer storage proof
        let latest_finalized_execution_header = latest_finalized_header
            .execution()
            .map_err(|_| anyhow!("No execution header in finality update".to_string()))?;

        let block_id = (*latest_finalized_execution_header.block_number()).into();
        debug!(target: "proof_service::input", "Fetching storage proof for address {} slot {:?} at block ID {:?}", request.src_chain_contract_address, request.src_chain_storage_slots, block_id);

        // Get execution part of ProofInputs
        let proof = self
            .execution_rpc_proxy
            .get_proof(
                request.src_chain_contract_address,
                &request.src_chain_storage_slots,
                block_id,
                *latest_finalized_execution_header.state_root(),
            )
            .await
            .context("Failed to get storage proof using execution provider proxy")?;

        let contract_storage =
            ContractStorageBuilder::build(&request.src_chain_storage_slots, proof)?;

        let inputs = ProofInputs {
            sync_committee_updates: consensus_proof_inputs.sync_committee_updates,
            finality_update: consensus_proof_inputs.finality_update,
            expected_current_slot,
            store: consensus_proof_inputs.initial_store,
            genesis_root: consensus_proof_inputs.config.chain.genesis_root,
            forks: consensus_proof_inputs.config.forks.clone(),
            contract_storage_slots: contract_storage,
        };

        Ok(inputs)
    }

    async fn generate_and_store_proof(
        request: ProofRequest,
        mut proof_service: ProofService<B>,
        proof_id: ProofId,
    ) {
        info!(
            target: "proof_service::generate",
            "[ProofID: {}] Starting proof generation", proof_id.to_hex_string()
        );

        // At this point, this function has exclusive control over this proof_id
        // via the Redis lock acquired before spawning.
        let cancellation_token = CancellationToken::new();
        let _cancellation_token_guard = CancellationTokenGuard::new(cancellation_token.clone());

        // Clone redis_store for the lock extension task
        let mut redis_store_clone = proof_service.redis_store.clone();

        // Spawn lock extension task
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        // Release lock on cancellation
                        redis_store_clone.release_proof_generation_lock(&proof_id).await;
                        debug!(target: "proof_service::generate", "[ProofID: {}] Lock extension task cancelled, released lock.", proof_id.to_hex_string());
                        break;
                    }
                    _ = ticker.tick() => {
                        // Use redis_store to extend the lock
                        match redis_store_clone.extend_proof_generation_lock(&proof_id, 2000).await {
                            Ok(true) => {
                                trace!(target: "proof_service::generate", "[ProofID: {}] Extended worker lock.", proof_id.to_hex_string());
                            }
                            Ok(false) => {
                                warn!(target: "proof_service::generate", "[ProofID: {}] Failed to extend worker lock: Lock key does not exist or expired.", proof_id.to_hex_string());
                                // If extending fails, maybe the main task finished/crashed, or Redis issue.
                                // Stop trying to extend.
                                break;
                            }
                            Err(e) => {
                                warn!(target: "proof_service::generate", "[ProofID: {}] Error extending worker lock: {:#?}. Releasing lock.", proof_id.to_hex_string(), e);
                                // Release lock on error to prevent dangling locks
                                redis_store_clone.release_proof_generation_lock(&proof_id).await;
                                break; // Stop trying on error
                            }
                        }
                    }
                }
            }
        });

        let inputs = proof_service.get_proof_inputs(&request).await;
        if inputs.is_ok() {
            info!(
                target: "proof_service::generate",
                "[ProofID: {}] Successfully generated proof inputs", proof_id.to_hex_string()
            );
        }

        let proof_output_result = match inputs {
            Ok(inputs) => {
                /*
                TODO: Not sure this is a perfect place, nor a perfect implementation for this % 32 error
                TODO: handling. It's the easiest fast solution. Let the caller re-request a bit later when
                TODO: we have a *proper* finalized slot
                 */
                /*
                It is important for a slot to be divisible by 32. To understand why, you need to understand
                bootstrapping. When we get some slot to start a proof from, we first need to bootsrtap
                a client to that *starting state* (see consensus_client::get_proof_inputs). For that, we
                need that starting slot to be a *checkpoint*. It's only a checkpoint and stored long-term by
                consensus nodes if it's divisible by 32. So if a finalizer bot (ZK API client) ever
                updates a smart contract to have a non-divisible latest head, we might be unable to
                generate further proofs to advance that contract because of this *checkpoint* constraint.
                 */
                let slot = inputs.finality_update.finalized_header().beacon().slot;
                if slot % 32 == 0 {
                    proof_service.proof_backend.generate_proof(inputs).await
                } else {
                    // Error here and let caller re-request this proof at a later time. In practice, this should almost never happen
                    Err(anyhow!(
                        "Finalized beacon chain slot {} is not divisible by 32. This could lead to an \
                        accidental bricking of the SP1Helios contract. This problem is intermittent \
                        and will go away at the next epoch most likely.",
                        slot
                    ))
                }
            }
            // If inputs generation errored, just pass it along as proof generation error.
            Err(e) => Err(e),
        };

        let updated_proof_state = match proof_output_result {
            Ok(proof_output) => {
                let mut proof_state = ProofRequestState::new(request.clone());
                proof_state.status = ProofRequestStatus::Success;
                proof_state.proof_data = Some(proof_output);
                proof_state
            }
            Err(e) => {
                warn!(
                    target: "proof_service::generate",
                    "[ProofID: {}] Error generating proof: {:#?}",
                    proof_id.to_hex_string(),
                    e
                );
                let mut proof_state = ProofRequestState::new(request.clone());
                proof_state.status = ProofRequestStatus::Errored;
                proof_state.error_message = Some(format!("{:#?}", e));
                proof_state
            }
        };

        // Retry loop for updating Redis state
        let mut retry_count = 0;
        loop {
            // Use redis_store to set the final proof state
            match proof_service
                .redis_store
                .set_proof_state(&proof_id, &updated_proof_state)
                .await
            {
                Ok(_) => {
                    info!(target: "proof_service::generate", "[ProofID: {}] Successfully stored final proof state in Redis.", proof_id.to_hex_string());
                    break;
                }
                Err(e) => {
                    retry_count += 1;
                    warn!(
                        target: "proof_service::generate",
                        "[ProofID: {}] Failed to store proof state in Redis (attempt {}): {:#?}. Retrying in 1s...",
                        proof_id.to_hex_string(),
                        retry_count,
                        e
                    );
                    // todo? Consider adding a max retry limit. But then do what? Abandon this task? Because it's either abandon
                    // and leave at ::Generating in Redis, which is bad, or retry forever
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }

        // Lock is released implicitly by the lock extension task upon cancellation/completion/error,
        // or by letting the lock expire if the extension task fails silently.
        // We could add an explicit release here, but the extension task handles it.
        // proof_service.redis_store.release_proof_generation_lock(&proof_id).await;
    }

    /// A utility for calling functions that require the global redis lock.
    /// Acquires the lock, executes the provided async function, and releases the lock.
    async fn with_global_lock<F, R, A>(&mut self, args: A, f: F) -> Result<R, ProofServiceError>
    where
        F: FnOnce(&mut Self, A, bool) -> BoxFuture<'_, Result<R, ProofServiceError>> + Send,
    {
        // Use redis_store to acquire lock
        let acquired = self.redis_store.acquire_global_lock().await?;
        debug!(target: "proof_service::lock", "Global lock acquire attempt result: {}", acquired);

        let result = f(self, args, acquired).await;

        // Use redis_store to release lock
        self.redis_store.release_global_lock().await; // release_global_lock in store handles logging

        result
    }

    /// Attempts to find and restart orphaned proof generation tasks.
    async fn restart_orphaned_proofs(&mut self) -> Result<(), ProofServiceError> {
        debug!(target: "proof_service::pickup", "Checking for orphaned proof generation tasks...");

        // Use redis_store to find requests
        let generating_requests = self
            .redis_store
            .find_requests_by_status(ProofRequestStatus::Generating)
            .await?;

        if generating_requests.is_empty() {
            debug!(target: "proof_service::pickup", "No requests found in ::Generating state.");
            return Ok(());
        }

        debug!(target: "proof_service::pickup", "Found {} requests in ::Generating state. Checking for locks...", generating_requests.len());

        let mut picked_up_count = 0;
        for proof_state in generating_requests {
            let proof_id = ProofId::new(&proof_state.request);

            // Use redis_store to try acquiring the lock
            match self
                .redis_store
                .acquire_proof_generation_lock(&proof_id, INITIAL_PROOF_GENERATION_LOCK_DURATION_MS)
                .await
            {
                Ok(true) => {
                    // Lock acquired! This task is orphaned.
                    let lock_key = self.redis_store.proof_generation_lock_key(&proof_id);
                    warn!(target: "proof_service::pickup", "Picking up orphaned proof generation task for ID: {}. Lock key acquired: {}", proof_id.to_hex_string(), lock_key);
                    picked_up_count += 1;

                    // Spawn a new worker for this task.
                    let proof_service_clone = self.clone();
                    tokio::spawn(async move {
                        Self::generate_and_store_proof(
                            proof_state.request,
                            proof_service_clone,
                            proof_id,
                        )
                        .await;
                    });
                }
                Ok(false) => {
                    // Lock is held by another worker.
                    debug!(target: "proof_service::pickup", "Proof ID {} is actively being processed (lock exists).", proof_id.to_hex_string());
                }
                Err(e) => {
                    // Error trying to acquire the lock.
                    error!(target: "proof_service::pickup", "Failed to check/acquire lock for proof ID {}: {:#?}. Skipping pickup.", proof_id.to_hex_string(), e);
                }
            }
        }

        if picked_up_count > 0 {
            info!(target: "proof_service::pickup", "Successfully picked up and restarted {} orphaned proof tasks.", picked_up_count);
        } else {
            debug!(target: "proof_service::pickup", "No orphaned tasks found requiring pickup.");
        }

        Ok(())
    }
}

/// Run stays in sync with the chain and periodically posts necessary updates to the proof_service
pub async fn run<B>(mut proof_service: ProofService<B>) -> anyhow::Result<()>
where
    B: ProofBackend + Clone + Send + Sync,
{
    let (mut light_client, next_slot_time) =
        init_light_client::<B, MainnetConsensusSpec, HttpRpc>(&mut proof_service).await?;

    let mut interval = interval_at(next_slot_time, std::time::Duration::from_secs(12));
    info!(
        target: "proof_service::run",
        "Starting main loop. Header polling interval: 12s"
    );

    let mut should_advance_redis_state = true;
    loop {
        if should_advance_redis_state {
            // Try to update redis with new finalized header
            update_redis_state(
                &mut proof_service,
                light_client.store.finalized_header.clone(),
                &mut should_advance_redis_state,
            )
            .await;
        }

        // Periodically check for and pick up orphaned proof generation tasks
        if let Err(e) = proof_service.restart_orphaned_proofs().await {
            warn!(target: "proof_service::run", "Error during orphaned proof pickup check: {:#?}", e);
        }

        // Periodically check that the finalized header stored in redis is no older than allowed MAX_FINALIZED_HEADER_AGE
        check_header_health(&mut proof_service.redis_store, light_client.config.clone()).await?;

        let _ = interval.tick().await;

        // Try to update light client to be in sync with chain tip
        advance_light_client(&mut light_client, &mut should_advance_redis_state).await;
    }
}

async fn init_light_client<B, S, R>(
    proof_service: &mut ProofService<B>,
) -> anyhow::Result<(consensus_client::Client<S, R>, Instant)>
where
    B: ProofBackend + Clone + Send + Sync + 'static,
    S: ConsensusSpec,
    R: ConsensusRpc<S> + std::fmt::Debug,
{
    let init_checkpoint = proof_service.get_initial_checkpoint().await?;

    info!(
        target: "proof_service::run",
        "Initializing light client with checkpoint: {}",
        init_checkpoint
    );

    // Create light_client. It is our main driver for finality updates / data integrity checks. This function aims to be updating state of this guy
    let mut light_client = consensus_client::Client::<S, R>::from_env()?;
    light_client.sync(init_checkpoint).await?;
    info!(
        target: "proof_service::run",
        "Initialized light client. Finalized slot: {}",
        light_client.store.finalized_header.beacon().slot
    );

    let next_slot_time = Instant::now()
        + light_client
            .config
            .duration_until_next_update()
            .to_std()
            .unwrap();

    Ok((light_client, next_slot_time))
}

/// Best effort attempt to advance redis state. Logs on errors
async fn update_redis_state<B>(
    proof_service: &mut ProofService<B>,
    finalized_header: LightClientHeader,
    should_advance_redis_state: &mut bool,
) where
    B: ProofBackend + Clone + Send + Sync + 'static,
{
    let res = proof_service
        .process_new_finalized_header(finalized_header.clone())
        .await;

    match res {
        Ok(header_equal_or_newer) => {
            if header_equal_or_newer {
                info!(target: "proof_service::run", "Redis state set to equal or newer than slot {}", finalized_header.beacon().slot);
                *should_advance_redis_state = false;
            } else {
                debug!(target: "proof_service::run", "Redis state advancement called but no update occurred for slot {}", finalized_header.beacon().slot);
            }
        }
        Err(e) => {
            warn!(target: "proof_service::run", "Failed to advance Redis state for finalized header slot {}. Error: {:#?}", finalized_header.beacon().slot, e);
        }
    }
}

/// Best effort attempt to advance Light Client. Logs on errors
async fn advance_light_client<S: ConsensusSpec, R: ConsensusRpc<S> + std::fmt::Debug>(
    light_client: &mut consensus_client::Client<S, R>,
    should_advance_redis_state: &mut bool,
) {
    let prev_finalized_slot = light_client.store.finalized_header.beacon().slot;
    let res = light_client.advance().await;
    if let Err(err) = res {
        warn!(target: "proof_service::run", "Helios light client advance error: {:#?}", err);
        return;
    }
    let new_finalized_slot = light_client.store.finalized_header.beacon().slot;
    if new_finalized_slot > prev_finalized_slot {
        info!(
            target: "proof_service::run",
            "Helios light client advanced. Finalized slot: {} -> {}",
            prev_finalized_slot, new_finalized_slot
        );
        *should_advance_redis_state = true;
    }
}

async fn check_header_health<ProofOutput>(
    redis_store: &mut RedisStore<ProofOutput>,
    config: Arc<helios_ethereum::config::Config>,
) -> Result<()>
where
    ProofOutput: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    match get_stored_header_age(redis_store, config).await {
        Ok(age) => {
            match age {
                Some(age) => {
                    let max_age = chrono::TimeDelta::from_std(MAX_FINALIZED_HEADER_AGE).unwrap();
                    if age > max_age {
                        error!(target: "proof_service::header_check", "stored header too old. Age {}s , max age {}s", age.num_seconds(), max_age.num_seconds());
                        Err(anyhow!(
                            "stored header too old, shutting down. Age {} , max age {}",
                            age,
                            max_age
                        ))
                    } else {
                        debug!(target: "proof_service::header_check", "Header is healthy. Age {}s", age.num_seconds());
                        Ok(())
                    }
                }
                None => {
                    // This check runs after we've already updated the header at least once. So it really should be present in redis
                    error!(
                        target: "proof_service::header_check", "No header set in redis. Was it removed manually during bot operation?"
                    );
                    Err(anyhow!(
                        "No header set in redis. Was it removed manually during bot operation?"
                    ))
                }
            }
        }
        Err(err) => {
            // Some redis error, will try again next cycle
            warn!(target: "proof_service::header_check", "error checking stored header age: {:#?}", err);
            // return Ok, consider this error non-critical, i.e. can be recovered from
            Ok(())
        }
    }
}

async fn get_stored_header_age<ProofOutput>(
    redis_store: &mut RedisStore<ProofOutput>,
    config: Arc<helios_ethereum::config::Config>,
) -> Result<Option<chrono::TimeDelta>>
where
    ProofOutput: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    match redis_store.read_finalized_header().await {
        Ok(header) => match header {
            Some(header) => Ok(Some(config.age(header.beacon().slot))),
            None => Ok(None),
        },
        Err(e) => Err(anyhow!("could not read finalized header from redis: {}", e)),
    }
}

/*
todo:
idea for test: request proof that should immediately go into ::Generating (old proof). Check that the generating lock is held
For this to work, I need to ensure that my prover mode is MOCK_PROVER: so set .env var, that's easy
 */
