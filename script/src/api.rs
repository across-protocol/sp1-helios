use crate::proof_service::{ProofId, ProofService, ProofServiceError, ProofStatus};
use alloy_primitives::{Address, B256};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    serve, Json, Router,
};
use log::{error, info};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::str::FromStr;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Status of a proof generation request
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProofStatusResponse {
    /// Proof generation is in progress
    Pending,
    /// Proof generation was successful
    Success,
    /// Proof generation encountered an error
    Errored,
}

impl From<ProofStatus> for ProofStatusResponse {
    fn from(status: ProofStatus) -> Self {
        match status {
            ProofStatus::Initiated | ProofStatus::WaitingForFinality | ProofStatus::Generating => {
                ProofStatusResponse::Pending
            }
            ProofStatus::Success => ProofStatusResponse::Success,
            ProofStatus::Errored => ProofStatusResponse::Errored,
        }
    }
}

// todo? make requesting possible by `proof_id` as well
/// Request for generating a storage slot proof
#[derive(Debug, Clone, Serialize, Deserialize, RlpEncodable, RlpDecodable)]
pub struct ProofRequest {
    /// Contract address to prove a storage slot for
    pub contract_address: Address,
    /// Storage slot key to prove
    pub storage_slot: B256,
    /// Block number on the source chain to prove against
    pub block_number: u64,
}

// todo: might want to return old_status, new_status
/// Response for a proof request operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofRequestResponse {
    /// The unique identifier for the requested proof, as a hex string
    pub proof_id: ProofId,
    /// Current status of the proof generation
    pub status: ProofStatusResponse,
}

/// Response to a proof generation request (used for API output)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofStateResponse {
    /// Unique ID for the proof request (hex-encoded keccak256 hash of request data)
    pub proof_id: ProofId,
    /// Status of the proof generation
    pub status: ProofStatusResponse,
    /// Calldata for the update function (only present when status is Success)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_calldata: Option<ProofData>,
    /// Error message (only present when status is Errored)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// Data needed to call `SP1Helios.update`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofData {
    /// ZK proof bytes to pass to the update function
    pub proof: Vec<u8>,
    /// Public values bytes to pass to the update function. Encoded `ProofOutputs`
    pub public_values: Vec<u8>,
    /// Beacon slot to pass to the update function
    pub head: u64,
}

// --- API Handlers & Router ---

// Helper to convert ProofServiceError to API Response
impl IntoResponse for ProofServiceError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            ProofServiceError::RedisError(e) => {
                error!("Redis error: {}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".to_string(),
                )
            }
            ProofServiceError::LockContention(id) => (
                StatusCode::CONFLICT, // Use CONFLICT (409) for lock issues
                format!(
                    "Proof request {} is already being processed",
                    id.to_hex_string()
                ),
            ),
            ProofServiceError::NotFound(id) => (
                StatusCode::NOT_FOUND,
                format!("Proof request {} not found", id.to_hex_string()),
            ),
            ProofServiceError::SerializationError(e) => {
                error!("Serialization error: {}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".to_string(),
                )
            }
            ProofServiceError::ProofGenerationFailed(id, msg) => {
                // This state is reflected in ProofState, usually return OK with Errored status
                // But if the error happens during the *request* phase itself:
                error!(
                    "Proof generation failed for {}: {}",
                    id.to_hex_string(),
                    msg
                );
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Proof generation failed".to_string(),
                )
            }
            ProofServiceError::Internal(msg) => {
                error!("Internal service error: {}", msg);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".to_string(),
                )
            }
        };
        (status, Json(serde_json::json!({ "error": error_message }))).into_response()
    }
}

/// Create and configure the API router
fn create_api_router(proof_service: ProofService) -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .route("/api/proofs", post(request_proof_handler))
        // GET /api/proofs/{proof_id_hex} - Get status/result by ProofId (hex string)
        .route("/api/proofs/{id}", get(get_proof_handler))
        .with_state(proof_service) // Provide ProofService as state
}

/// Health check endpoint
async fn health_handler() -> &'static str {
    "OK"
}

/// Handler function for new request_proof requests
async fn request_proof_handler(
    State(service): State<ProofService>,
    Json(request): Json<ProofRequest>,
) -> Result<impl IntoResponse, ProofServiceError> {
    let (proof_id, status) = service.request_proof(request).await?;

    let response = ProofRequestResponse {
        proof_id,
        status: status.into(),
    };
    Ok((StatusCode::ACCEPTED, Json(response)))
}

/// Handler function for new get_proof requests
async fn get_proof_handler(
    State(service): State<ProofService>,
    Path(proof_id_hex): Path<String>,
) -> Result<impl IntoResponse, ProofServiceError> {
    // Parse the hex ID into B256
    let proof_id_bytes = B256::from_str(&proof_id_hex).map_err(|_| {
        // Return a more specific API error for bad input format
        ProofServiceError::Internal(format!("Invalid proof ID format: {}", proof_id_hex))
        // Consider adding a dedicated BadInput variant to ProofServiceError
    })?;
    // Convert B256 to ProofId using the From trait
    let proof_id: ProofId = proof_id_bytes.into();

    // Fetch the full internal state
    let stored_state = service.get_proof_state(proof_id).await?;

    // Construct the API response structure (ProofState)
    let response_state = ProofStateResponse {
        proof_id,                                  // todo: will serialize to a hex string?
        status: stored_state.status.into(),        // Get status from stored state
        update_calldata: stored_state.proof_data, // Get data from stored state (will be None if not Success)
        error_message: stored_state.error_message, // Get error message from stored state (will be None if not Errored)
    };

    Ok((StatusCode::OK, Json(response_state)))
}

/// Start the API server
pub async fn start_api_server(port: u16, proof_service: ProofService) -> JoinHandle<()> {
    // Create the API router
    let app = create_api_router(proof_service);

    // Create socket address
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    // Create TCP listener
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind API server to {}: {}", addr, e);
            panic!("Failed to bind API server: {}", e);
        }
    };

    // Log the startup
    info!("Starting API server on {}", addr);

    // Spawn the server task
    tokio::spawn(async move {
        serve(listener, app.into_make_service())
            .await
            .unwrap_or_else(|e| {
                error!("API server error: {}", e);
            });
    })
}
