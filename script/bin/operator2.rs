use dotenv::dotenv;
use helios_consensus_core::types::LightClientHeader;
use helios_consensus_core::{consensus_spec::MainnetConsensusSpec, types::FinalityUpdate};
use helios_ethereum::rpc::ConsensusRpc;
use log::{error, info};
use sp1_helios_script::{
    // Assuming your crate name is sp1_helios_script based on path
    api::start_api_server,
    get_client,
    get_latest_checkpoint,
    proof_service::ProofService,
};
use std::env;
use std::time::Duration;
use tokio::time;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load environment variables from .env file
    dotenv().ok();
    // Initialize logger
    env_logger::init();

    info!("Starting operator...");

    // --- Configuration ---
    let api_port: u16 = env::var("API_PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()?;
    let redis_url = env::var("REDIS_URL").expect("REDIS_URL must be set");
    let redis_lock_duration_secs: u64 = env::var("REDIS_LOCK_DURATION_SECS")
        .unwrap_or_else(|_| "2".to_string()) // Default to 2 seconds
        .parse()?;
    let redis_key_prefix =
        env::var("REDIS_KEY_PREFIX").unwrap_or_else(|_| "proof_service".to_string());
    let finalized_header_check_interval_secs: u64 =
        env::var("FINALIZED_HEADER_CHECK_INTERVAL_SECS")
            .unwrap_or_else(|_| "30".to_string()) // Default to 30 seconds
            .parse()?;

    info!("Configuration loaded:");
    info!(" - API Port: {}", api_port);
    info!(" - Redis URL: {}", redis_url); // Be cautious logging sensitive URLs in production
    info!(" - Redis Lock Duration: {}s", redis_lock_duration_secs);
    info!(" - Redis Key Prefix: {}", redis_key_prefix);
    info!(
        " - Finalized Header Check Interval: {}s",
        finalized_header_check_interval_secs
    );

    // --- Get Latest Finalized Header ---
    info!("Fetching latest finalized header...");
    let latest_finalized_header = get_latest_finalized_header().await?;
    info!("Latest finalized header retrieved successfully.");

    // --- Service Initialization ---
    let proof_service = match ProofService::new(
        &redis_url,
        redis_lock_duration_secs,
        redis_key_prefix,
        latest_finalized_header.clone(),
    )
    .await
    {
        Ok(service) => {
            info!("ProofService initialized successfully.");
            service
        }
        Err(e) => {
            error!("Failed to initialize ProofService: {}", e);
            return Err(e.into()); // Propagate the error
        }
    };

    // --- Start API Server ---
    let _api_server_handle = start_api_server(api_port, proof_service.clone()).await;
    info!("API server started.");

    // --- Finalized Header Polling Loop ---
    info!("Starting finalized header polling loop...");
    let mut interval = time::interval(Duration::from_secs(finalized_header_check_interval_secs));
    let mut current_header = latest_finalized_header;

    loop {
        interval.tick().await;

        match get_latest_finalized_header().await {
            Ok(new_header) => {
                // todo: compare anything else here besides the beacon slot number?
                // Check if the header is different from current one
                if current_header.beacon().slot != new_header.beacon().slot {
                    info!("New finalized header detected. Updating ProofService...");

                    // Update the header in ProofService
                    match proof_service
                        .update_finalized_header(new_header.clone())
                        .await
                    {
                        Ok(_) => {
                            info!("ProofService finalized header updated successfully.");
                            current_header = new_header;
                        }
                        Err(e) => {
                            error!("Failed to update finalized header in ProofService: {}", e);
                            // Continue with the same header, will try again next interval
                        }
                    }
                } else {
                    info!("No change in finalized header detected.");
                }
            }
            Err(e) => {
                error!("Failed to fetch latest finalized header: {}", e);
                // Continue with the same header, will try again next interval
            }
        }
    }
}

/// Fetches the latest finalized LightClientHeader for use with ProofService
async fn get_latest_finalized_header() -> Result<LightClientHeader, Box<dyn std::error::Error>> {
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
