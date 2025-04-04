use crate::api::{ProofData, ProofRequest};
use alloy_primitives::B256; // Use alloy_primitives
use alloy_rlp::Encodable;
// Import Encodable for ProofRequest
use redis::{aio::ConnectionManager, AsyncCommands};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use thiserror::Error;
use tokio::sync::Mutex;

// --- Core Types ---

/// Unique identifier for a proof request, derived from the Keccak256 hash of its RLP-encoded content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)] // B256 derives these traits
pub struct ProofId(B256);

impl ProofId {
    /// Creates a new ProofId by RLP encoding the ProofRequest and hashing it with Keccak256.
    pub fn new(request: &ProofRequest) -> Self {
        let mut buf = Vec::new();
        // ProofRequest needs RlpEncodable from api.rs
        request.encode(&mut buf);
        ProofId(alloy_primitives::keccak256(&buf))
    }

    /// Returns the underlying B256 hash of the ProofId.
    pub fn hash(&self) -> B256 {
        self.0
    }

    /// Returns the hash as a hex string prefixed with "0x".
    pub fn to_hex_string(&self) -> String {
        format!("{:x}", self.0)
    }
}

// Implement From<B256> for ProofId
impl From<B256> for ProofId {
    fn from(hash: B256) -> Self {
        ProofId(hash)
    }
}

// todo: consider using this in enum below
pub enum ProofRequestType {
    Network(String),
    CPU,
    CUDA,
    Mock,
}

// todo: consider converting this into a stateful enum
/// Status of a proof generation request
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProofStatus {
    /// the proof was requested, but we haven't checked if it can be moved to generating right away, or wait for finality
    Initiated,
    /// the block for which the proof is requested is not yet part of a finalized chain
    WaitingForFinality,
    /// the ZK proof is being generated
    Generating,
    /// the proof generated succesfully. Ready for consumption
    Success,
    // todo: any other reasons for Errored status?
    /// proof generation failed
    Errored,
}

/// Represents the state of a proof request stored in Redis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofState {
    pub status: ProofStatus,
    /// The original request that initiated this proof generation.
    request: ProofRequest,
    /// Transaction hash or identifier from the external proof network (e.g., SP1).
    proof_network_tx_id: Option<String>,
    /// Final proof data, available only on success.
    pub proof_data: Option<ProofData>,
    /// Error message if proof generation failed.
    pub error_message: Option<String>,
}

/// Errors that can occur within the ProofService.
#[derive(Error, Debug)]
pub enum ProofServiceError {
    #[error("Redis error: {0}")]
    RedisError(#[from] redis::RedisError),
    #[error("Failed to acquire lock for proof request ID {0:?}, likely already processing.")]
    LockContention(ProofId),
    #[error("Proof request not found: {0:?}")]
    NotFound(ProofId),
    #[error("Failed to serialize/deserialize state: {0}")]
    SerializationError(#[from] serde_json::Error), // Added variant for serde_json errors
    #[error("Proof generation failed for ID {0:?}: {1}")]
    ProofGenerationFailed(ProofId, String),
    #[error("Internal service error: {0}")]
    Internal(String),
}

// --- Proof Service ---

/// Service responsible for managing the lifecycle of ZK proof generation requests.
///
/// It uses Redis for state management and locking, and interacts with an
/// external asynchronous function to trigger the actual proof computation.
#[derive(Clone)] // Clone is required for Axum state
pub struct ProofService {
    // Use ConnectionManager for efficient connection handling.
    // Arc allows sharing the manager across clones of ProofService.
    redis_conn_manager: Arc<Mutex<ConnectionManager>>,
    redis_lock_duration: Duration,
    redis_key_prefix: String,
}

impl ProofService {
    /// Creates a new ProofService instance.
    ///
    /// # Arguments
    ///
    /// * `redis_url` - Connection string for the Redis instance.
    /// * `redis_lock_duration_secs` - Duration (in seconds) for Redis locks used during requests.
    /// * `redis_key_prefix` - Prefix for all keys stored in Redis by this service.
    ///
    /// # Errors
    ///
    /// Returns `ProofServiceError::RedisError` if connecting to Redis fails.
    pub async fn new(
        redis_url: &str,
        redis_lock_duration_secs: u64,
        redis_key_prefix: String,
    ) -> Result<Self, ProofServiceError> {
        let client = redis::Client::open(redis_url)?;
        let redis_conn_manager = Arc::new(Mutex::new(ConnectionManager::new(client).await?));
        Ok(Self {
            redis_conn_manager,
            redis_lock_duration: Duration::from_secs(redis_lock_duration_secs),
            redis_key_prefix,
        })
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

    /// Requests a proof for the given parameters.
    ///
    /// If a request for the same parameters already exists (completed or pending),
    /// its `ProofId` is returned immediately. Otherwise, a new proof generation
    /// process is initiated:
    /// 1. A lock is acquired in Redis.
    /// 2. Initial state (`Requested`) is stored in Redis.
    /// 3. An asynchronous task is spawned to handle the actual proof generation steps.
    /// 4. The lock is released (automatically via expiry or explicitly).
    ///
    /// # Arguments
    ///
    /// * `request` - The `ProofRequest` containing details for the proof.
    ///
    /// # Returns
    ///
    /// * `Ok(ProofId)` - The ID of the requested proof. Only returns Ok if the proof was just created
    /// * `Err(ProofServiceError)` - If locking fails or an internal error occurs.
    pub async fn request_proof(
        &self,
        request: ProofRequest,
    ) -> Result<(ProofId, ProofStatus), ProofServiceError> {
        let proof_id = ProofId::new(&request);
        let state_key = self.get_redis_state_key(proof_id);
        let lock_key = self.get_redis_lock_key(proof_id);

        let mut conn_guard = self.redis_conn_manager.lock().await;
        let conn = &mut *conn_guard; // Get mutable reference to ConnectionManager

        // Attempt to acquire distributed lock using SET NX EX
        let acquired: bool = redis::cmd("SET")
            .arg(&lock_key)
            .arg("locked") // Value doesn't matter much, existence does
            .arg("NX") // Set only if key does not exist
            .arg("PX") // Set expiry in milliseconds
            .arg(self.redis_lock_duration.as_millis() as u64)
            .query_async(conn)
            .await?;

        let current_state = self.get_stored_state(conn, proof_id).await?;

        if acquired {
            // --- Lock Acquired ---
            match current_state {
                Some(state) => match state.status {
                    // state exists and proof generation has errored before. We start a proof cycle anew here
                    ProofStatus::Errored => {
                        // todo: this branch and new request branch are identical. Reduce redundant code. Idk if it's possible to do nicely
                        // Previous proof errored for this ID. Request a new proof. Pretend that this is a new request
                        log::info!("Lock acquired for new proof request: {:?}", proof_id);
                        let initial_state = ProofState {
                            status: ProofStatus::Initiated,
                            request: request.clone(),
                            proof_network_tx_id: None,
                            proof_data: None,
                            error_message: None,
                        };
                        let serialized_state = serde_json::to_string(&initial_state)?; // Uses ProofServiceError::SerializationError

                        conn.set::<_, _, ()>(&state_key, serialized_state).await?;

                        // --- Spawn the background proof generation task ---
                        // Clone necessary resources for the task
                        let task_conn_manager = Arc::clone(&self.redis_conn_manager);
                        let task_state_key = state_key.clone();
                        let task_proof_id = proof_id;
                        let task_request = request.clone(); // Pass request data to task
                        let task_prefix = self.redis_key_prefix.clone(); // Pass prefix if needed in task

                        tokio::spawn(async move {
                            log::info!(
                                "Spawning proof generation task for ID: {:?}",
                                task_proof_id
                            );
                            // Pass cloned resources to the actual logic function
                            Self::run_proof_generation_flow(
                                task_conn_manager,
                                task_prefix,
                                task_proof_id,
                                task_state_key,
                                task_request,
                            )
                            .await;
                        });
                        // -------------------------------------------------

                        // todo: will this error if the lock is not present? Maybe we want that, cause a race might have happened
                        conn.del::<_, ()>(&lock_key).await?;
                        Ok((proof_id, initial_state.status))
                    }
                    // state exists and generation has not errored. Just return current status
                    _ => Ok((proof_id, state.status)),
                },
                None => {
                    // State does not exist, create and store initial state
                    log::info!("Lock acquired for new proof request: {:?}", proof_id);
                    let initial_state = ProofState {
                        status: ProofStatus::Initiated,
                        request: request.clone(),
                        proof_network_tx_id: None,
                        proof_data: None,
                        error_message: None,
                    };
                    let serialized_state = serde_json::to_string(&initial_state)?; // Uses ProofServiceError::SerializationError

                    conn.set::<_, _, ()>(&state_key, serialized_state).await?;

                    // --- Spawn the background proof generation task ---
                    // Clone necessary resources for the task
                    let task_conn_manager = Arc::clone(&self.redis_conn_manager);
                    let task_state_key = state_key.clone();
                    let task_proof_id = proof_id;
                    let task_request = request.clone(); // Pass request data to task
                    let task_prefix = self.redis_key_prefix.clone(); // Pass prefix if needed in task

                    tokio::spawn(async move {
                        log::info!("Spawning proof generation task for ID: {:?}", task_proof_id);
                        // Pass cloned resources to the actual logic function
                        Self::run_proof_generation_flow(
                            task_conn_manager,
                            task_prefix,
                            task_proof_id,
                            task_state_key,
                            task_request,
                        )
                        .await;
                    });
                    // -------------------------------------------------

                    // Optional: Explicitly delete lock - useful if task might finish quickly
                    // todo: will this error if the lock is not present? Maybe we want that actually, cause the race might have happened
                    conn.del::<_, ()>(&lock_key).await?;
                    Ok((proof_id, initial_state.status))
                }
            }
        } else {
            // --- Lock Not Acquired ---
            match current_state {
                Some(state) => Ok((proof_id, state.status)),
                // todo: not sure if we need this branch. It shouldn't be a crash. The lock is short-lived
                // Lock exists, but state doesn't. This is unusual.
                // Could be transient issue, or the lock holder crashed before writing state.
                None => Err(ProofServiceError::LockContention(proof_id)),
            }
        }
    }

    /// Retrieves the full internal state of a proof request.
    /// Returns NotFound error if the request ID does not exist.
    pub async fn get_proof_state(&self, id: ProofId) -> Result<ProofState, ProofServiceError> {
        let mut conn_guard = self.redis_conn_manager.lock().await;
        let conn = &mut *conn_guard;

        match self.get_stored_state(conn, id).await {
            Ok(state) => match state {
                Some(state) => Ok(state),
                None => Err(ProofServiceError::NotFound(id)),
            },
            Err(e) => Err(e),
        }
    }

    /// Internal helper to fetch and deserialize the state from Redis.
    async fn get_stored_state(
        &self,
        conn: &mut ConnectionManager,
        id: ProofId,
    ) -> Result<Option<ProofState>, ProofServiceError> {
        let state_key = self.get_redis_state_key(id);
        let state_json: Option<String> = conn.get(&state_key).await?;

        match state_json {
            Some(json) => {
                let state: ProofState = serde_json::from_str(&json)?; // Uses From trait for ProofServiceError::SerializationError
                Ok(Some(state))
            }
            None => Ok(None),
        }
    }

    // --- Placeholder for Async Proof Generation Logic ---

    /// The core logic for generating a proof, intended to be run in a background task.
    /// This function assumes it's the only active flow for this `proof_id` due to
    /// the locking mechanism in `request_proof`.
    /// It directly sets the state in Redis.
    async fn run_proof_generation_flow(
        conn_manager: Arc<Mutex<ConnectionManager>>,
        _redis_prefix: String, // Available if needed
        proof_id: ProofId,
        state_key: String,
        request: ProofRequest, // Contains data needed for the proof network call
    ) {
        // ! Todo: this function needs to have actuall proof generation depending on the
        // `SP1_PROVER` env var: mock, network, cpu or cuda. Take this logic from `operator.rs`
        // Right now it's only a simulation

        log::info!("Task {:?}: Starting proof generation flow.", proof_id);

        // --- Phase 1: Update status to Generating ---
        // We optimistically set to Generating. If the external call fails immediately,
        // we'll update to Errored later.
        let pending_state = ProofState {
            status: ProofStatus::Generating,
            request: request.clone(),
            proof_network_tx_id: None, // Will be set after simulated external call
            proof_data: None,
            error_message: None,
        };
        match serde_json::to_string(&pending_state) {
            Ok(serialized_state) => {
                let mut conn_guard = conn_manager.lock().await;
                let conn = &mut *conn_guard;
                if let Err(e) = conn.set::<_, _, ()>(&state_key, serialized_state).await {
                    log::error!(
                        "Task {:?}: Failed to set initial Pending state in Redis: {:?}",
                        proof_id,
                        e
                    );
                    // Decide if we should abort here. For now, let's log and continue,
                    // as the state might still be 'Requested'.
                }
            }
            Err(e) => {
                log::error!(
                    "Task {:?}: Failed to serialize Pending state: {:?}",
                    proof_id,
                    e
                );
                return; // Cannot proceed without serialization
            }
        }

        // --- Phase 2: Simulate External Call & Work ---
        log::info!(
            "Task {:?}: Simulating external proof request and work...",
            proof_id
        );
        tokio::time::sleep(Duration::from_secs(10)).await; // Simulate network delay + work

        // Simulate a simple outcome: Success or Error based on ProofId
        let sim_u64 = u64::from_be_bytes(proof_id.hash().0[..8].try_into().unwrap_or_default());
        let final_result: Result<ProofData, String> = if sim_u64 % 4 == 0 {
            Err("Simulated proof generation failure".to_string())
        } else {
            Ok(ProofData {
                proof: proof_id.hash().to_vec(),              // Dummy proof data
                public_values: request.storage_slot.to_vec(), // Dummy PVs
                head: request.block_number.saturating_add(1), // Dummy head
            })
        };

        // --- Phase 3: Set Final State ---
        let final_state = match final_result {
            Ok(proof_data) => {
                log::info!(
                    "Task {:?}: Proof generation successful. Status -> Success.",
                    proof_id
                );
                ProofState {
                    status: ProofStatus::Success,
                    request, // Take ownership of original request
                    proof_network_tx_id: Some(format!("simulated_tx_{}", proof_id.to_hex_string())),
                    proof_data: Some(proof_data),
                    error_message: None,
                }
            }
            Err(err_msg) => {
                log::error!(
                    "Task {:?}: Proof generation failed: {}. Status -> Errored.",
                    proof_id,
                    err_msg
                );
                ProofState {
                    status: ProofStatus::Errored,
                    request,                   // Take ownership of original request
                    proof_network_tx_id: None, // No Tx ID if it failed
                    proof_data: None,
                    error_message: Some(format!("Proof generation error: {}", err_msg)),
                }
            }
        };

        // Serialize and set the final state directly
        match serde_json::to_string(&final_state) {
            Ok(serialized_state) => {
                let mut conn_guard = conn_manager.lock().await;
                let conn = &mut *conn_guard;
                if let Err(e) = conn.set::<_, _, ()>(&state_key, serialized_state).await {
                    log::error!(
                        "Task {:?}: Failed to set final state in Redis: {:?}",
                        proof_id,
                        e
                    );
                }
            }
            Err(e) => {
                log::error!(
                    "Task {:?}: Failed to serialize final state: {:?}",
                    proof_id,
                    e
                );
            }
        }

        log::info!("Task {:?}: Proof generation flow finished.", proof_id);
    }
}
