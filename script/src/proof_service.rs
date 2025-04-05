use crate::api::{ProofData, ProofRequest};
use alloy_primitives::B256; // Use alloy_primitives
use alloy_rlp::Encodable;
use helios_consensus_core::types::LightClientHeader;
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
    // todo: consider removing this status
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

impl ProofState {
    /// Creates a new ProofState with Initiated status and default values.
    pub fn new(request: ProofRequest) -> Self {
        ProofState {
            status: ProofStatus::Initiated,
            request,
            proof_network_tx_id: None,
            proof_data: None,
            error_message: None,
        }
    }
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
    redis_lock_duration: Duration,
    redis_key_prefix: String,
    redis_conn_manager: ConnectionManager,

    // todo: do I use tokio mutex here, cause performace is not that important and I can avoid deadlocks easier?
    // OH ROFL, it's already tokio::Mutex. I should probably change it to std::Mutex for now
    inner: Arc<Mutex<ProofServiceInner>>,
    // todo: add receiver channel for
}

pub struct ProofServiceInner {
    latest_finalized_header: LightClientHeader,
}

impl ProofService {
    pub async fn new(
        redis_url: &str,
        redis_lock_duration_secs: u64,
        redis_key_prefix: String,
        // todo: decide where this gets created
        latest_finalized_header: LightClientHeader,
    ) -> Result<Self, ProofServiceError> {
        let client = redis::Client::open(redis_url)?;
        let redis_conn_manager = ConnectionManager::new(client).await?;

        Ok(Self {
            redis_conn_manager,
            inner: Arc::new(Mutex::new(ProofServiceInner {
                latest_finalized_header,
            })),
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

    // in this function, we hold 2 locks:
    //  - redis lock for a specific proofId key
    //  - self.inner lock for a finalized header
    // These 2 entities are related parts of our state and we aim to avoid races between writing them
    pub async fn request_proof(
        &self,
        request: ProofRequest,
    ) -> Result<(ProofId, ProofStatus), ProofServiceError> {
        let proof_id = ProofId::new(&request);
        let state_key = self.get_redis_state_key(proof_id);
        let lock_key = self.get_redis_lock_key(proof_id);

        // we want to hold this lock until this function exits because we can't update to the new
        // finalized head until we updated redis with this new request correctly
        let inner = self.inner.lock().await;

        // todo: remove unwrap here
        let finalized_execution_header = inner.latest_finalized_header.execution().unwrap();
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

        let current_state = self.get_stored_state(conn, proof_id).await?;

        if acquired {
            // --- Lock Acquired ---
            log::info!("Lock acquired for new proof request: {:?}", proof_id);
            match current_state {
                Some(state) => match state.status {
                    // state exists and proof generation has errored before. We start a proof cycle anew here
                    ProofStatus::Errored => {
                        let proof_status = self
                            .initiate_request_processing(
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
                        .initiate_request_processing(
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

    /// Retrieves the full internal state of a proof request.
    /// Returns NotFound error if the request ID does not exist.
    pub async fn get_proof_state(&self, id: ProofId) -> Result<ProofState, ProofServiceError> {
        let conn = &mut self.redis_conn_manager.clone();

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

    /// This function assumes that the corresponding `redis_lock_key` is locked.
    /// Called after checking the proper conditions for starting a proof generation sequence.
    async fn initiate_request_processing(
        &self,
        conn: &mut ConnectionManager,
        latest_finalized_block: u64,
        request: ProofRequest,
        redis_state_key: String,
        redis_lock_key: String,
    ) -> Result<ProofStatus, ProofServiceError> {
        let mut proof_state = ProofState::new(request.clone());

        if latest_finalized_block >= request.block_number {
            // we can request a proof immediately and should do that here. It will continue in the backround and wait for proof to complete
            // Change status to ::Generating
            proof_state.status = ProofStatus::Generating;
            conn.set::<_, _, ()>(&redis_state_key, serde_json::to_string(&proof_state)?)
                .await?;

            tokio::spawn(async move {
                Self::request_proof_zkvm().await;
            });
        } else {
            proof_state.status = ProofStatus::WaitingForFinality;
            conn.set::<_, _, ()>(&redis_state_key, serde_json::to_string(&proof_state)?)
                .await?;
        }

        // todo: will this error if the lock is not present? Maybe we want that, cause a race might have happened
        conn.del::<_, ()>(&redis_lock_key).await?;
        Ok(proof_state.status)
    }

    /// request proof generation from a ZKVM
    async fn request_proof_zkvm() {
        // request proof with proper inputs and store result to redis
        // todo!();
    }
}
