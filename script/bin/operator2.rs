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

    let proof_service = ProofService::<SP1Backend>::new().await?;

    // proof_service is a wrapper around DB connection + external zk proof generation service.
    // It's stateless in this way, so it's fine to clone. Redis handles all state
    let _api_task_handle = start_api_server(proof_service.clone()).await;

    /*
    todo:
    we might want to remove this loop and just let the program crash. How to handle all lingering green threads?
    Maybe they're not a problem. They contain self-contained logic that will run it's course
     */
    loop {
        if let Err(e) = ProofService::run(proof_service.clone()).await {
            error!("Error running proof service: {}", e);
        }
    }
}
