use crate::{
    proof_backends::ProofBackend,
    proof_service::ProofService,
    types::{
        ProofId, ProofRequestState, ProofRequestStatus, ProofServiceError, SP1HeliosProofData,
    },
};
use alloy_primitives::{hex, Address, B256};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use async_trait::async_trait;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    serve, Json, Router,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::{env, net::SocketAddr};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::{error, info};
use utoipa::{OpenApi, ToSchema};
use utoipa_swagger_ui::{SwaggerUi, Url};

/// Status of a proof generation request
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProofStatusResponse {
    Pending,
    Success,
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

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ApiProofRequest {
    /// Contract address to prove a storage slot for (hex string with 0x prefix)
    pub src_chain_contract_address: String,
    /// Storage slot keys to prove (list of hex strings with 0x prefix)
    pub src_chain_storage_slots: Vec<String>,
    /// Minumum block number required for the storage to Be present (e.g. the block number of TX that set the storage slot above)
    pub src_chain_block_number: u64,
    /// A stored head on destination chain Helios contract
    pub dst_chain_contract_from_head: u64,
    /// A corresponding stored header on destination chain Helios contract
    pub dst_chain_contract_from_header: String,
}

// Internal represenation of `ApiProofRequest`
#[derive(Debug, Clone, Serialize, Deserialize, RlpEncodable, RlpDecodable)]
pub struct ProofRequest {
    /// Contract address to prove a storage slot for
    pub src_chain_contract_address: Address,
    /// Storage slot keys to prove
    pub src_chain_storage_slots: Vec<B256>,
    /// Minumum block number required for the storage to Be present (e.g. the block number of TX that set the storage slot above)
    pub src_chain_block_number: u64,
    /// A stored head on destination chain Helios contract
    pub dst_chain_contract_from_head: u64,
    /// A corresponding stored header on destination chain Helios contract
    pub dst_chain_contract_from_header: B256,
}

impl TryFrom<ApiProofRequest> for ProofRequest {
    type Error = ProofServiceError;

    fn try_from(req: ApiProofRequest) -> Result<Self, Self::Error> {
        let src_chain_contract_address = Address::from_str(&req.src_chain_contract_address)
            .map_err(|_| {
                ProofServiceError::Internal("Invalid contract address format".to_string())
            })?;

        // Iterate over the input vector of strings, parse each to B256.
        // collect() stops on the first error, otherwise collects into a Vec<B256>.
        let src_chain_storage_slots: Vec<B256> = req
            .src_chain_storage_slots
            .iter()
            .map(|s| {
                B256::from_str(s).map_err(|_| {
                    ProofServiceError::Internal("Invalid storage slot format".to_string())
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let dst_chain_contract_from_header = B256::from_str(&req.dst_chain_contract_from_header)
            .map_err(|_| {
                // Use a more specific error message for the header
                ProofServiceError::Internal("Invalid header format".to_string())
            })?;

        Ok(ProofRequest {
            src_chain_contract_address,
            src_chain_storage_slots,
            src_chain_block_number: req.src_chain_block_number,
            dst_chain_contract_from_head: req.dst_chain_contract_from_head,
            dst_chain_contract_from_header,
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
#[serde(bound = "ProofOutput: DeserializeOwned")]
pub struct ProofStateResponse<ProofOutput>
where
    ProofOutput: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    /// Unique ID for the proof request (hex-encoded keccak256 hash of request data)
    pub proof_id: String,
    /// Status of the proof generation
    pub status: ProofStatusResponse,
    /// Calldata for the update function (only present when status is Success)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_calldata: Option<ProofOutput>,
    /// Error message (only present when status is Errored)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// Response containing the latest finalized header details
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FinalizedHeaderResponse {
    /// The slot number of the latest finalized header
    pub slot: u64,
    /// The checkpoint (tree_hash_root) of the latest finalized header, as a hex string
    pub checkpoint: String,
}

// --- API Documentation ---

#[derive(OpenApi)]
#[openapi(
    paths(
        health_handler,
        request_proof_handler,
        get_proof_handler,
        get_finalized_header_handler,
        get_vkey_handler
    ),
    components(
        schemas(
            ApiProofRequest,
            ProofStatusResponse,
            ProofRequestResponse,
            FinalizedHeaderResponse,
            ProofStateResponse<SP1HeliosProofData>,
            VkeyResponse
        )
    ),
    tags( (name = "helios-proof-service", description = "Helios Proof Service API") ),
    servers( (url = "/v1") )
)]
pub struct ApiDocV1;

// --- API Handlers & Router ---

// Helper to convert ProofServiceError to API Response
impl IntoResponse for ProofServiceError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            ProofServiceError::RedisError(e) => {
                error!("Redis error: {:#?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Redis error: {}", e),
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
                error!("Serialization error: {:#?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Serialization error: {}", e),
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
                    format!("Proof generation failed: {}", msg),
                )
            }
            ProofServiceError::Internal(msg) => {
                error!("Internal service error: {}", msg);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Internal error: {}", msg),
                )
            }
        };
        (status, Json(serde_json::json!({ "error": error_message }))).into_response()
    }
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
async fn request_proof_handler<B>(
    State(mut service): State<ProofService<B>>,
    Json(api_request): Json<ApiProofRequest>,
) -> Result<impl IntoResponse, ProofServiceError>
where
    B: ProofBackend + Clone + Send + Sync + 'static,
{
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
        // todo: the 200 response body currently shows only SP1Helios variant. If we add new backends, like R0VM, this is not 100% accurate doc. But also not an urgent fix
        (status = 200, description = "Proof information retrieved", body = ProofStateResponse<SP1HeliosProofData>),
        (status = 404, description = "Proof not found"),
        (status = 400, description = "Invalid proof ID format"),
        (status = 500, description = "Internal server error")
    )
)]
async fn get_proof_handler<B>(
    State(mut service): State<ProofService<B>>,
    Path(proof_id_hex): Path<String>,
) -> Result<impl IntoResponse, ProofServiceError>
where
    B: ProofBackend + Clone + Send + Sync + 'static,
{
    let proof_id_bytes = B256::from_str(&proof_id_hex).map_err(|_| {
        ProofServiceError::Internal(format!("Invalid proof ID format: {}", proof_id_hex))
    })?;
    let proof_id: ProofId = proof_id_bytes.into();

    let stored_state = match service.get_proof(&proof_id).await? {
        Some(state) => state,
        None => return Err(ProofServiceError::NotFound(proof_id)),
    };

    let response_state = ProofStateResponse {
        proof_id: proof_id.to_hex_string(),
        status: stored_state.status.into(),
        update_calldata: stored_state.proof_data,
        error_message: stored_state.error_message,
    };

    Ok((StatusCode::OK, Json(response_state)))
}

/// Handler function for retrieving the latest finalized header details
#[utoipa::path(
    get,
    path = "/api/finalized-header",
    tag = "helios-proof-service",
    responses(
        (status = 200, description = "Latest finalized header details retrieved", body = FinalizedHeaderResponse),
        (status = 404, description = "Finalized header not found in storage"),
        (status = 500, description = "Internal server error")
    )
)]
async fn get_finalized_header_handler<B>(
    State(mut service): State<ProofService<B>>,
) -> Result<impl IntoResponse, ProofServiceError>
where
    B: ProofBackend + Clone + Send + Sync + 'static,
{
    match service.get_finalized_header_details().await? {
        Some(header_details) => Ok((StatusCode::OK, Json(header_details))),
        None => Err(ProofServiceError::Internal(
            "Finalized header not found in Redis.".to_string(),
        )),
    }
}

/// Response containing the verifying key digest for the embedded ZK program
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct VkeyResponse {
    /// Hex string (0x-prefixed) of the vkey digest
    pub vkey: String,
}

#[utoipa::path(
    get,
    path = "/api/vkey",
    tag = "helios-proof-service",
    responses(
        (status = 200, description = "Verifying key digest retrieved", body = VkeyResponse),
        (status = 500, description = "Internal server error")
    )
)]
async fn get_vkey_handler<B>(
    State(service): State<ProofService<B>>,
) -> Result<impl IntoResponse, ProofServiceError>
where
    B: ProofBackend + Clone + Send + Sync + 'static,
{
    let digest = service.vkey_digest_bytes();
    let hex_str = format!("0x{}", hex::encode(digest));
    Ok((StatusCode::OK, Json(VkeyResponse { vkey: hex_str })))
}

// --- API Service Trait --- //

/// Defines the interface for the proof service exposed via the API.
/// This allows the API layer to be decoupled from the concrete ProofService implementation.
#[async_trait]
pub trait ApiProofService: Clone + Send + Sync + 'static {
    /// Associated type for the specific proof output format produced by the backend.
    /// This type must be serializable/deserializable, thread-safe, and cloneable.
    /// It determines the structure of the `update_calldata` field when successful.
    type ProofOutput: Clone + Serialize + DeserializeOwned + Send + Sync + 'static;

    /// Retrieves the current state of a proof request by its ID.
    async fn get_proof(
        &self,
        id: &ProofId,
    ) -> Result<Option<ProofRequestState<Self::ProofOutput>>, ProofServiceError>;

    /// Submits a new request for proof generation.
    /// Returns the ProofId and the initial status (e.g., Pending).
    async fn request_proof(
        &self,
        request: ProofRequest,
    ) -> Result<(ProofId, ProofRequestStatus), ProofServiceError>;
}

/// Create and configure the API router
fn create_api_router<B>(proof_service: ProofService<B>) -> Router
where
    B: ProofBackend + Clone + Send + Sync + 'static,
{
    let v1_router = Router::new()
        .route("/health", get(health_handler))
        .route("/api/proofs", post(request_proof_handler::<B>))
        .route("/api/proofs/{id}", get(get_proof_handler))
        .route("/api/finalized-header", get(get_finalized_header_handler))
        .route("/api/vkey", get(get_vkey_handler::<B>));

    // Serve v1 as primary API, add next versions to this vec! as needed
    let swagger = SwaggerUi::new("/").urls(vec![(
        Url::with_primary("v1", "/v1/api-docs/openapi.json", true),
        ApiDocV1::openapi(),
    )]);

    Router::new()
        .nest("/v1", v1_router.with_state(proof_service.clone()))
        .merge(swagger)
}

/// Start the API server
pub async fn start_api_server<B>(proof_service: ProofService<B>) -> JoinHandle<()>
where
    B: ProofBackend + Clone + Send + Sync + 'static,
{
    // Ensure environment variables are loaded
    dotenv::dotenv().ok();

    let api_port = env::var("PORT")
        .expect("PORT environment variable must be set")
        .parse::<u16>()
        .expect("PORT must be a valid number");

    info!("Starting API server on port {}", api_port);

    let app = create_api_router(proof_service);

    let socket_addr = SocketAddr::from(([0, 0, 0, 0], api_port));

    let listener = match TcpListener::bind(socket_addr).await {
        Ok(l) => l,
        Err(e) => {
            panic!("Failed to bind API server: {}", e);
        }
    };

    info!("Starting API server on {}", socket_addr);

    tokio::spawn(async move {
        serve(listener, app.into_make_service())
            .await
            .unwrap_or_else(|e| {
                error!("API server error: {:#?}", e);
            });
    })
}
