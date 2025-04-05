use crate::{
    proof_service::ProofService,
    types::{ProofData, ProofId, ProofRequestStatus, ProofServiceError},
};
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
use std::str::FromStr;
use std::{env, net::SocketAddr};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use utoipa::{OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

/// Status of a proof generation request
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProofStatusResponse {
    /// Proof generation is in progress
    Pending,
    /// Proof generation was successful
    Success,
    /// Proof generation encountered an error
    Errored,
}

impl From<ProofRequestStatus> for ProofStatusResponse {
    fn from(status: ProofRequestStatus) -> Self {
        match status {
            ProofRequestStatus::Initiated
            | ProofRequestStatus::WaitingForFinality
            | ProofRequestStatus::Generating => ProofStatusResponse::Pending,
            ProofRequestStatus::Success => ProofStatusResponse::Success,
            ProofRequestStatus::Errored => ProofStatusResponse::Errored,
        }
    }
}

// Create API-friendly version of ProofRequest with strings
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ApiProofRequest {
    /// Contract address to prove a storage slot for (hex string with 0x prefix)
    pub contract_address: String,
    /// Storage slot key to prove (hex string with 0x prefix)
    pub storage_slot: String,
    /// Block number on the source chain to prove against
    pub block_number: u64,
    /// The caller must pass a valid head stored on associated destination chain contract
    pub valid_contract_head: u64,
}

// Keep original RLP structure for internal use
#[derive(Debug, Clone, Serialize, Deserialize, RlpEncodable, RlpDecodable)]
pub struct ProofRequest {
    /// Contract address to prove a storage slot for
    pub contract_address: Address,
    /// Storage slot key to prove
    pub storage_slot: B256,
    /// Block number on the source chain to prove against
    pub block_number: u64,
    /// The caller must pass a valid head stored on associated destination chain contract
    pub valid_contract_head: u64,
}

// Convert API request to internal request
impl TryFrom<ApiProofRequest> for ProofRequest {
    type Error = ProofServiceError;

    fn try_from(req: ApiProofRequest) -> Result<Self, Self::Error> {
        Ok(ProofRequest {
            contract_address: Address::from_str(&req.contract_address).map_err(|_| {
                ProofServiceError::Internal("Invalid contract address format".to_string())
            })?,
            storage_slot: B256::from_str(&req.storage_slot).map_err(|_| {
                ProofServiceError::Internal("Invalid storage slot format".to_string())
            })?,
            block_number: req.block_number,
            valid_contract_head: req.valid_contract_head,
        })
    }
}

/// Response for a proof request operation
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ProofRequestResponse {
    /// The unique identifier for the requested proof, as a hex string
    pub proof_id: String,
    /// Current status of the proof generation
    pub status: ProofStatusResponse,
}

/// Response to a proof generation request (used for API output)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ProofStateResponse {
    /// Unique ID for the proof request (hex-encoded keccak256 hash of request data)
    pub proof_id: String,
    /// Status of the proof generation
    pub status: ProofStatusResponse,
    /// Calldata for the update function (only present when status is Success)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_calldata: Option<ProofData>,
    /// Error message (only present when status is Errored)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

// --- API Documentation ---

#[derive(OpenApi)]
#[openapi(
    paths(health_handler, request_proof_handler, get_proof_handler),
    components(
        schemas(
            ApiProofRequest,
            ProofStatusResponse,
            ProofRequestResponse,
            ProofStateResponse
        )
    ),
    tags(
        (name = "helios-proof-service", description = "Helios Proof Service API")
    )
)]
pub struct ApiDoc;

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
        .route("/api/proofs/{id}", get(get_proof_handler))
        // Add Swagger UI
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .with_state(proof_service)
}

/// Health check endpoint
#[utoipa::path(
    get,
    path = "/health",
    tag = "helios-proof-service",
    responses(
        (status = 200, description = "Service is healthy", body = String)
    )
)]
async fn health_handler() -> &'static str {
    "OK"
}

/// Handler function for new request_proof requests
#[utoipa::path(
    post,
    path = "/api/proofs",
    tag = "helios-proof-service",
    request_body = ApiProofRequest,
    responses(
        (status = 202, description = "Proof request accepted", body = ProofRequestResponse),
        (status = 400, description = "Invalid request data"),
        (status = 409, description = "Request already being processed"),
        (status = 500, description = "Internal server error")
    )
)]
async fn request_proof_handler(
    State(service): State<ProofService>,
    Json(api_request): Json<ApiProofRequest>,
) -> Result<impl IntoResponse, ProofServiceError> {
    // Convert API request to internal request type
    let request = ProofRequest::try_from(api_request)?;

    let (proof_id, status) = service.request_proof(request).await?;

    let response = ProofRequestResponse {
        proof_id: proof_id.to_hex_string(),
        status: status.into(),
    };
    Ok((StatusCode::ACCEPTED, Json(response)))
}

/// Handler function for new get_proof requests
#[utoipa::path(
    get,
    path = "/api/proofs/{id}",
    tag = "helios-proof-service",
    params(
        ("id" = String, Path, description = "Proof ID (hex string)")
    ),
    responses(
        (status = 200, description = "Proof information retrieved", body = ProofStateResponse),
        (status = 404, description = "Proof not found"),
        (status = 400, description = "Invalid proof ID format"),
        (status = 500, description = "Internal server error")
    )
)]
async fn get_proof_handler(
    State(service): State<ProofService>,
    Path(proof_id_hex): Path<String>,
) -> Result<impl IntoResponse, ProofServiceError> {
    // Parse the hex ID into B256
    let proof_id_bytes = B256::from_str(&proof_id_hex).map_err(|_| {
        // Return a more specific API error for bad input format
        ProofServiceError::Internal(format!("Invalid proof ID format: {}", proof_id_hex))
    })?;
    // Convert B256 to ProofId using the From trait
    let proof_id: ProofId = proof_id_bytes.into();

    // Fetch the full internal state
    let stored_state = service.get_proof_state(proof_id).await?;

    // Construct the API response structure (ProofState)
    let response_state = ProofStateResponse {
        proof_id: proof_id.to_hex_string(),
        status: stored_state.status.into(),
        update_calldata: stored_state.proof_data,
        error_message: stored_state.error_message,
    };

    Ok((StatusCode::OK, Json(response_state)))
}

/// Start the API server
pub async fn start_api_server(proof_service: ProofService) -> JoinHandle<()> {
    // Ensure environment variables are loaded
    dotenv::dotenv().ok();

    // Get API port from environment variable (required)
    let api_port = match env::var("API_PORT") {
        Ok(port_str) => match port_str.parse::<u16>() {
            Ok(port) => port,
            Err(e) => {
                panic!("API_PORT must be a valid port number: {}", e);
            }
        },
        Err(e) => {
            panic!("API_PORT environment variable must be set: {}", e);
        }
    };

    info!("Starting API server on port {}", api_port);

    // Create the API router
    let app = create_api_router(proof_service);

    // Create socket address
    let addr = SocketAddr::from(([0, 0, 0, 0], api_port));

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
