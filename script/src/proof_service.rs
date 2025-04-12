use crate::{
    api::ProofRequest,
    redis_store::RedisStore,
    try_get_checkpoint, try_get_client, try_get_latest_checkpoint, try_get_updates,
    types::{ProofData, ProofId, ProofRequestState, ProofRequestStatus, ProofServiceError},
    util::CancellationTokenGuard,
};
use alloy::{
    network::Ethereum,
    providers::{Provider, ProviderBuilder, RootProvider},
    transports::{http::Http, BoxFuture},
};
use alloy_primitives::hex;
use anyhow::{anyhow, Context, Result};
use helios_consensus_core::{
    consensus_spec::MainnetConsensusSpec,
    types::{FinalityUpdate, LightClientHeader},
};
use helios_ethereum::rpc::ConsensusRpc;
use log::{debug, error, info, warn};
use reqwest::{Client, Url};
use sp1_helios_primitives::types::{ContractStorage, ProofInputs, StorageSlot};
use sp1_sdk::SP1Stdin;
use std::time::Duration;
use std::{env, sync::Arc};
use tokio::time::{interval_at, Instant};
use tokio_util::sync::CancellationToken;

const ELF: &[u8] = include_bytes!("../../elf/sp1-helios-elf");
const ORPHANED_PROOF_LOCK_ACQUIRE_DURATION_MS: u64 = 1000;

/// Service responsible for managing the lifecycle of ZK proof generation requests.
///
/// It uses Redis for state management and locking, and interacts with an
/// external asynchronous function to trigger the actual proof computation.
#[derive(Clone)]
pub struct ProofService {
    prover_client: Arc<sp1_sdk::EnvProver>,
    proving_key: sp1_sdk::SP1ProvingKey,
    redis_store: RedisStore,
    // required for storage slot merkle proving
    source_chain_provider: RootProvider<Http<Client>>,
}

// --- API-facing functionality of ProofService ---
impl ProofService {
    pub async fn get_proof(
        &mut self,
        id: &ProofId,
    ) -> Result<Option<ProofRequestState>, ProofServiceError> {
        self.redis_store.get_proof_state(id).await
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
            log::debug!(target: "proof_service::api", "Global lock acquired for new proof request ID: {}", proof_id.to_hex_string());

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

        if latest_finalized_block >= request.block_number
            && latest_finalized_slot > request.stored_contract_head
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
                .acquire_proof_generation_lock(&proof_id, ORPHANED_PROOF_LOCK_ACQUIRE_DURATION_MS)
                .await
            {
                Ok(true) => {
                    // Lock acquired, spawn generation task.
                    // The spawned task now receives the proof_id, not the lock_key.
                    tokio::spawn(async move {
                        Self::execute_proof_generation(
                            request,
                            proof_service_clone,
                            proof_id, // Pass proof_id
                        )
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
                        "Failed to acquire lock for proof generation ID: {}: {}", proof_id.to_hex_string(), e
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
        // Uses with_global_lock which uses redis_store
        self.with_global_lock(
            latest_finalized_header,
            |this, latest_finalized_header, lock_acquired: bool| {
                Box::pin(async move {
                    if lock_acquired {
                        this.process_new_finalized_header_locked(latest_finalized_header)
                            .await
                    } else {
                        // Log instead of returning error if lock not acquired, as another worker might be processing
                        warn!(target: "proof_service::run", "Could not acquire global lock to process new finalized header. Another worker might be processing.");
                        Ok(false) // Indicate no update was made by this worker
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
    ) -> Result<bool, ProofServiceError> {
        // Use redis_store to read header
        let stored_finalized_header = self.redis_store.read_finalized_header().await?;
        let should_update_header: bool = match stored_finalized_header {
            Some(redis_header) => {
                redis_header.beacon().slot < latest_finalized_header.beacon().slot
            }
            None => true,
        };

        if should_update_header {
            // Use redis_store to find requests
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
                    && finalized_block_number >= proof_state.request.block_number
                    && finalized_slot > proof_state.request.stored_contract_head
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
                // Use redis_store to acquire lock
                match self
                    .redis_store
                    .acquire_proof_generation_lock(
                        &proof_id,
                        ORPHANED_PROOF_LOCK_ACQUIRE_DURATION_MS,
                    )
                    .await
                {
                    Ok(true) => {
                        tokio::spawn(async move {
                            Self::execute_proof_generation(
                                request,
                                proof_service_clone,
                                proof_id, // Pass proof_id
                            )
                            .await;
                        });
                    }
                    Ok(false) => {
                        debug!(
                            target: "proof_service::state",
                            "Skipping proof generation spawn for ID: {}, lock already held.",
                            proof_id.to_hex_string()
                        );
                    }
                    Err(e) => {
                        warn!(
                            target: "proof_service::state",
                            "Failed to acquire lock for proof generation spawn ID: {}: {}",
                            proof_id.to_hex_string(),
                            e
                        );
                    }
                }
            }

            return Ok(true);
        }

        Ok(false)
    }
}

// --- Runtime logic required for proof generation ---
impl ProofService {
    /// Initialize a new ProofService with configuration from environment variables
    pub async fn new() -> anyhow::Result<Self> {
        // Ensure environment variables are loaded
        dotenv::dotenv().ok();

        let source_execution_rpc_url: Url = env::var("SOURCE_EXECUTION_RPC_URL")
            .context("SOURCE_EXECUTION_RPC_URL not set")?
            .parse()
            .context("Failed to parse SOURCE_EXECUTION_RPC_URL")?;

        let source_chain_provider = ProviderBuilder::new()
            .network::<Ethereum>()
            .on_http(source_execution_rpc_url);

        // Initialize RedisStore
        let redis_store = RedisStore::new().await?;

        let prover_client = sp1_sdk::ProverClient::from_env();
        let (proving_key, _) = prover_client.setup(ELF);

        let service = Self {
            prover_client: Arc::new(prover_client),
            proving_key,
            redis_store,
            source_chain_provider,
        };

        info!(target: "proof_service::init", "ProofService initialized successfully.");
        Ok(service)
    }

    /// Run the proof service, periodically checking for new finalized headers
    pub async fn run(mut self) -> anyhow::Result<()> {
        // todo: we might want to get checkpoint from .env to be 100% sure it's genuine
        let checkpoint = try_get_latest_checkpoint().await?;
        info!(
            target: "proof_service::run",
            "Initializing light client with checkpoint: {:?}",
            checkpoint
        );

        // todo: can we somehow check via genesis params that the loaded state is genuine? Mb it's already done?
        let mut light_client = try_get_client(checkpoint).await?;
        info!(
            target: "proof_service::run",
            "Initialized light client. Finalized slot: {}",
            light_client.store.finalized_header.beacon().slot
        );

        let next_slot_time =
            Instant::now() + light_client.duration_until_next_update().to_std().unwrap();
        let mut interval = interval_at(next_slot_time, std::time::Duration::from_secs(12));

        info!(
            target: "proof_service::run",
            "Starting main loop. Header polling interval: 12s"
        );

        // We advance DB state on startup and whenever we see a new finalized slot
        let mut should_advance_db_state = true;
        loop {
            if should_advance_db_state {
                let res = self
                    .process_new_finalized_header(light_client.store.finalized_header.clone())
                    .await;

                match res {
                    Ok(updated) => {
                        if updated {
                            info!(target: "proof_service::run", "Advanced Redis state to finalized header slot: {}", light_client.store.finalized_header.beacon().slot)
                        } else {
                            warn!(target: "proof_service::run", "Redis state advancement called but no update occurred for slot: {}", light_client.store.finalized_header.beacon().slot)
                        }
                        should_advance_db_state = false;
                    }
                    Err(e) => {
                        warn!(target: "proof_service::run", "Failed to advance Redis state for finalized header slot {}. Error: {}", light_client.store.finalized_header.beacon().slot, e)
                    }
                }
            }

            // Periodically check for and pick up orphaned proof generation tasks
            if let Err(e) = self.restart_orphaned_proofs().await {
                warn!(target: "proof_service::run", "Error during orphaned proof pickup check: {}", e);
            }

            let _ = interval.tick().await;

            let prev_finalized_slot = light_client.store.finalized_header.beacon().slot;
            let res = light_client.advance().await;
            if let Err(err) = res {
                warn!(target: "proof_service::run", "Helios light client advance error: {}", err);
                continue;
            }
            let new_finalized_slot = light_client.store.finalized_header.beacon().slot;
            if new_finalized_slot > prev_finalized_slot {
                info!(
                    target: "proof_service::run",
                    "Helios light client advanced. Finalized slot: {} -> {}",
                    prev_finalized_slot, new_finalized_slot
                );
                should_advance_db_state = true;
            }
        }
    }

    /// Executes the ZK proof generation process for a given request.
    async fn execute_proof_generation(
        request: ProofRequest,
        mut proof_service: ProofService,
        proof_id: ProofId,
    ) {
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
                                debug!(target: "proof_service::generate", "[ProofID: {}] Extended worker lock.", proof_id.to_hex_string());
                            }
                            Ok(false) => {
                                warn!(target: "proof_service::generate", "[ProofID: {}] Failed to extend worker lock: Lock key does not exist or expired.", proof_id.to_hex_string());
                                // If extending fails, maybe the main task finished/crashed, or Redis issue.
                                // Stop trying to extend.
                                break;
                            }
                            Err(e) => {
                                warn!(target: "proof_service::generate", "[ProofID: {}] Error extending worker lock: {}. Releasing lock.", proof_id.to_hex_string(), e);
                                // Release lock on error to prevent dangling locks
                                redis_store_clone.release_proof_generation_lock(&proof_id).await;
                                break; // Stop trying on error
                            }
                        }
                    }
                }
            }
        });

        // todo? Move these to some kind of config file
        let max_tries_to_setup_input = 3;
        let sleep_duration_between_retries = 12; // one slot
        let mut attempt = 0;
        // todo: make a function out of this. There's too much code here
        let stdin: anyhow::Result<SP1Stdin> = loop {
            attempt += 1;
            match async {
                // Fetch the checkpoint at requested slot
                let checkpoint = try_get_checkpoint(request.stored_contract_head).await?;

                // Get the client from the checkpoint, will bootstrap a client with checkpoint
                // todo: does this type of bootstrapping guarantee valid RPC output? I.e., will the client
                // todo: have 100% correct data after this call?
                let client = try_get_client(checkpoint).await?;
                let sync_committee_updates = try_get_updates(&client).await?;
                let finality_update: FinalityUpdate<MainnetConsensusSpec> = client
                    .rpc
                    .get_finality_update()
                    .await
                    .map_err(|e| anyhow!("{}", e))?;
                let latest_finalized_header = finality_update.finalized_header();
                let expected_current_slot = client.expected_current_slot();
                let latest_finalized_execution_header = latest_finalized_header
                    .execution()
                    .map_err(|_| anyhow::anyhow!("No execution header in finality update"))?;
                let proof = proof_service
                    .source_chain_provider
                    .get_proof(request.hub_pool_address, vec![request.storage_slot])
                    .block_id((*latest_finalized_execution_header.block_number()).into())
                    .await?;
                let mut stdin = SP1Stdin::new();
                let storage_slot = StorageSlot {
                    key: request.storage_slot,
                    expected_value: proof.storage_proof[0].value,
                    mpt_proof: proof.storage_proof[0].proof.clone(),
                };
                let inputs = ProofInputs {
                    sync_committee_updates,
                    finality_update,
                    expected_current_slot,
                    store: client.store.clone(),
                    genesis_root: client.config.chain.genesis_root,
                    forks: client.config.forks.clone(),
                    contract_storage_slots: ContractStorage {
                        address: proof.address,
                        expected_value: alloy_trie::TrieAccount {
                            nonce: proof.nonce,
                            balance: proof.balance,
                            storage_root: proof.storage_hash,
                            code_hash: proof.code_hash,
                        },
                        mpt_proof: proof.account_proof,
                        storage_slots: vec![storage_slot],
                    },
                };
                let encoded_proof_inputs = serde_cbor::to_vec(&inputs)?;
                stdin.write_slice(&encoded_proof_inputs);
                anyhow::Ok(stdin)
            }
            .await
            {
                Ok(stdin) => break Ok(stdin),
                Err(e) => {
                    warn!(
                        target: "proof_service::generate",
                        "[ProofID: {}] Failed to setup proof inputs (attempt {}/{}): {}",
                        proof_id.to_hex_string(),
                        attempt + 1,
                        max_tries_to_setup_input,
                        e
                    );
                    // Sleep for 12 seconds before retrying
                    tokio::time::sleep(tokio::time::Duration::from_secs(
                        sleep_duration_between_retries,
                    ))
                    .await;
                    if attempt == max_tries_to_setup_input - 1 {
                        error!(target: "proof_service::generate", "[ProofID: {}] All {} attempts to setup proof inputs failed.", proof_id.to_hex_string(), max_tries_to_setup_input);
                        break Err(anyhow!(
                            "[ProofID: {}] Final attempt to setup proof inputs failed: {}",
                            proof_id.to_hex_string(),
                            e
                        ));
                    }
                }
            }
        };

        let zk_proof_result = match stdin {
            Ok(valid_stdin) => {
                let proving_key = proof_service.proving_key.clone();
                let prover_client = proof_service.prover_client.clone();
                tokio::task::spawn_blocking(move || {
                    prover_client
                        .prove(&proving_key, &valid_stdin)
                        .groth16()
                        .run()
                })
                .await
                .map_err(|join_err| anyhow!("Spawned proof generation task failed: {}", join_err))
                .and_then(|res| res) // Flatten Result<Result<_, _>, _> to Result<_, _>
            }
            Err(e) => Err(e), // Stdin setup failed
        };

        let updated_proof_state = match zk_proof_result {
            Ok(proof) => {
                let proof_hex_string = hex::encode(proof.bytes());
                let public_values_hex_string = hex::encode(proof.public_values.to_vec());
                info!(
                    target: "proof_service::generate",
                    "[ProofID: {}] Proof generated successfully. Storing in Redis.",
                    proof_id.to_hex_string()
                );
                let mut proof_state = ProofRequestState::new(request.clone());
                proof_state.status = ProofRequestStatus::Success;
                proof_state.proof_data = Some(ProofData {
                    proof: proof_hex_string,
                    public_values: public_values_hex_string,
                    from_head: request.stored_contract_head,
                });
                proof_state
            }
            Err(e) => {
                warn!(
                    target: "proof_service::generate",
                    "[ProofID: {}] Error generating proof: {}",
                    proof_id.to_hex_string(),
                    e.to_string()
                );
                let mut proof_state = ProofRequestState::new(request.clone());
                proof_state.status = ProofRequestStatus::Errored;
                proof_state.error_message = Some(e.to_string());
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
                        "[ProofID: {}] Failed to store proof state in Redis (attempt {}): {}. Retrying in 1s...",
                        proof_id.to_hex_string(),
                        retry_count,
                        e
                    );
                    // todo: Consider adding a max retry limit
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
                .acquire_proof_generation_lock(&proof_id, ORPHANED_PROOF_LOCK_ACQUIRE_DURATION_MS)
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
                        Self::execute_proof_generation(
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
                    error!(target: "proof_service::pickup", "Failed to check/acquire lock for proof ID {}: {}. Skipping pickup.", proof_id.to_hex_string(), e);
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

/*
todo:
idea for test: request proof that should immediately go into ::Generating (old proof). Check that the generating lock is held
For this to work, I need to ensure that my prover mode is MOCK_PROVER: so set .env var, that's easy
 */
