use dotenv::dotenv;
use log::{error, info};
use sp1_helios_script::{
    // Assuming your crate name is sp1_helios_script based on path
    api::start_api_server,
    proof_service::ProofService,
};
use std::env;
use tokio::signal;

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

    info!("Configuration loaded:");
    info!(" - API Port: {}", api_port);
    info!(" - Redis URL: {}", redis_url); // Be cautious logging sensitive URLs in production
    info!(" - Redis Lock Duration: {}s", redis_lock_duration_secs);
    info!(" - Redis Key Prefix: {}", redis_key_prefix);

    // --- Service Initialization ---
    let proof_service =
        match ProofService::new(&redis_url, redis_lock_duration_secs, redis_key_prefix).await {
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
    let api_server_handle = start_api_server(api_port, proof_service.clone()).await;
    info!("API server started.");

    // --- Wait for Shutdown Signal ---
    info!("Operator running. Press Ctrl+C to shut down.");
    match signal::ctrl_c().await {
        Ok(()) => {
            info!("Received shutdown signal. Shutting down ungracefully...");
            api_server_handle.abort(); // Ungraceful shutdown: abort the API server immediately
        }
        Err(err) => {
            error!("Failed to listen for shutdown signal: {}", err);
            // Attempt to shut down anyway
            api_server_handle.abort();
        }
    }

    info!("Operator shut down complete.");
    Ok(())
}
