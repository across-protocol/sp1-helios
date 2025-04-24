use crate::api::ProofRequest;
use alloy_primitives::B256;
use alloy_rlp::Encodable;
use tracing::error;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;

/// Unique identifier for a proof request, derived from the Keccak256 hash of its RLP-encoded content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProofId(B256);

impl ProofId {
    /// Creates a new ProofId by RLP encoding the ProofRequest and hashing it with Keccak256.
    pub fn new(request: &ProofRequest) -> Self {
        let mut buf = Vec::new();
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

impl From<B256> for ProofId {
    fn from(hash: B256) -> Self {
        ProofId(hash)
    }
}

// todo: consider converting this into a stateful enum
/// Status of a proof generation request
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProofRequestStatus {
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
#[serde(bound = "ProofOutput: DeserializeOwned")]
pub struct ProofRequestState<ProofOutput>
where
    ProofOutput: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    pub status: ProofRequestStatus,
    /// The original request that initiated this proof generation.
    pub request: ProofRequest,
    /// Transaction hash or identifier from the external proof network (e.g., SP1).
    pub proof_network_tx_id: Option<String>,
    /// Final proof data, available only on success.
    pub proof_data: Option<ProofOutput>,
    /// Error message if proof generation failed.
    pub error_message: Option<String>,
}

impl<ProofOutput> ProofRequestState<ProofOutput>
where
    ProofOutput: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    /// Creates a new ProofState with Initiated status and default values.
    pub fn new(request: ProofRequest) -> Self {
        ProofRequestState {
            status: ProofRequestStatus::Initiated,
            request,
            proof_network_tx_id: None,
            proof_data: None,
            error_message: None,
        }
    }
}

/// Required calldata for `SP1Helios.update`
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SP1HeliosProofData {
    /// Hex string of ZK proof bytes to pass to the update function
    pub proof: String,
    /// Hex string of public values bytes to pass to the update function. Encoded `ProofOutputs`
    pub public_values: String,
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
    SerializationError(#[from] serde_json::Error),
    #[error("Proof generation failed for ID {0:?}: {1}")]
    ProofGenerationFailed(ProofId, String),
    #[error("Internal service error: {0}")]
    Internal(String),
}
