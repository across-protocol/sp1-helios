use log::error;
use sp1_helios_script::proof_backends::sp1::SP1Backend;
use std::env;

use sp1_helios_script::api::start_api_server;
use sp1_helios_script::proof_service::ProofService;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env::set_var("RUST_LOG", "info");
    dotenv::dotenv().ok();
    env_logger::init();

    let proof_backend = SP1Backend::from_env()?;
    let proof_service = ProofService::new(proof_backend).await?;

    let _api_task_handle = start_api_server(proof_service.clone()).await;

    if let Err(e) = proof_service.run().await {
        error!("Error running proof service: {}", e);
        return Err(e);
    }

    Err(anyhow::anyhow!(
        "proof_service.run exited unexpectedly without returning an error"
    ))
}
