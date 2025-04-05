use log::error;
use std::env;

use sp1_helios_script::api::start_api_server;
use sp1_helios_script::proof_service::ProofService;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env::set_var("RUST_LOG", "info");
    dotenv::dotenv().ok();
    env_logger::init();

    let proof_service = ProofService::new().await?;

    // Start API server with a clone of the proof service. Notice that cloning is fine here
    // becauer ProofService object is mostly stateless. The only state it has is under an
    // Arc<Mutex<>> which is fine to share via a clone
    let _api_task_handle = start_api_server(proof_service.clone()).await;

    loop {
        if let Err(e) = ProofService::run_header_loop(proof_service.clone()).await {
            error!("Error running proof service: {}", e);
        }
    }
}
