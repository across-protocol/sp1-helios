use anyhow::Context;
use sp1_helios_script::init_tracing;
use sp1_helios_script::proof_backends::sp1::SP1Backend;
use std::time::Duration;
use tokio::time::sleep;
use tracing::error;

use sp1_helios_script::api::start_api_server;
use sp1_helios_script::proof_service::ProofService;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    init_tracing().context("failed to set up tracing")?;

    let proof_backend = SP1Backend::from_env()?;
    let proof_service = ProofService::new(proof_backend).await?;

    let _api_task_handle = start_api_server(proof_service.clone()).await;

    if let Err(e) = sp1_helios_script::proof_service::run(proof_service).await {
        error!("Error running proof service: {:#?}", e);
        // todo: temporary "graceful shutdown" hack
        sleep(Duration::from_secs(5)).await;
        return Err(e);
    }

    // todo: temporary "graceful shutdown" hack
    sleep(Duration::from_secs(5)).await;

    Err(anyhow::anyhow!(
        "proof_service.run exited unexpectedly without returning an error"
    ))
}
