#![allow(unused_imports)]
// Playground
use alloy::hex;
use log::info;
use sp1_helios_script::{get_checkpoint, get_client, redis_store::RedisStore};
use std::env;
use tree_hash::TreeHash;

// target for slot 11509824: 0x492d9843f2e4c789e5bf239591d0e8e2549f45eb50870a4725c62a74331fac97
// tree hash root of the beacon: f000097e8527266d317e24abb05a1413919b48494de8648375d78c2dae11c284
// hash of the beacon body root: 1ca8a0b3b95216625370d6ec5ff8058f0d1a2f87988f700d8eab45b1ecb39e2c
// hash of the state: 6977be6a9568e912494dcf01f1db2db70c2ba76d321ecb2b815db64595a27d2e
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env::set_var("RUST_LOG", "info");
    dotenv::dotenv().ok();
    env_logger::init();

    // Fetch checkpoint from Redis
    let mut redis_store = RedisStore::<String>::new().await?;
    match redis_store.read_finalized_header().await {
        Ok(Some(header)) => {
            let checkpoint = header.beacon().state_root.tree_hash_root();
            info!(
                "Found finalized header in Redis. Slot: {}, Checkpoint (Beacon Root): {}",
                header.beacon().slot,
                hex::encode(checkpoint)
            );
        }
        Ok(None) => {
            info!("No finalized header found in Redis.");
        }
        Err(e) => {
            info!("Error reading finalized header from Redis: {}", e);
        }
    }

    /*
    let slot: u64 = 11467072;
    let checkpoint = get_checkpoint(slot).await;
    info!("checkpoint from slot: {}", hex::encode(checkpoint));
    // let checkpoint = b256!("0xa4c300c4ad14d5b6c507836be6989c25cd04d62fe1a21e2c62ea6b776406eed9");
    let client = get_client(checkpoint).await;
    info!(
        "finalized slot from checkpoint: {}",
        client.store.finalized_header.beacon().slot
    );
    */

    Ok(())
}
