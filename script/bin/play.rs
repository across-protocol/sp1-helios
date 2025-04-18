// Playground

use alloy::hex;
use log::info;
use sp1_helios_script::{get_checkpoint, get_client};
use std::env;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env::set_var("RUST_LOG", "info");
    dotenv::dotenv().ok();
    env_logger::init();

    let slot: u64 = 11467072;
    let checkpoint = get_checkpoint(slot).await;
    info!("checkpoint from slot: {}", hex::encode(checkpoint));
    // let checkpoint = b256!("0xa4c300c4ad14d5b6c507836be6989c25cd04d62fe1a21e2c62ea6b776406eed9");
    let client = get_client(checkpoint).await;
    info!(
        "finalized slot from checkpoint: {}",
        client.store.finalized_header.beacon().slot
    );

    Ok(())
}
