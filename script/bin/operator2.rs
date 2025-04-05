use log::error;
use std::env;

use sp1_helios_script::proof_service::ProofService;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env::set_var("RUST_LOG", "info");
    dotenv::dotenv().ok();
    env_logger::init();

    let mut proof_service = ProofService::new().await?;
    loop {
        if let Err(e) = proof_service.run().await {
            error!("Error running proof service: {}", e);
        }
    }
}
