#![allow(unused_imports)]
// Playground
use alloy::hex;
use anyhow::Context;
use sp1_helios_script::{get_checkpoint, get_client, init_tracing, redis_store::RedisStore};
use std::env;
use tracing::info;
use tree_hash::TreeHash;

// target for slot 11509824: 0x492d9843f2e4c789e5bf239591d0e8e2549f45eb50870a4725c62a74331fac97
// tree hash root of the beacon: f000097e8527266d317e24abb05a1413919b48494de8648375d78c2dae11c284
// hash of the beacon body root: 1ca8a0b3b95216625370d6ec5ff8058f0d1a2f87988f700d8eab45b1ecb39e2c
// hash of the state: 6977be6a9568e912494dcf01f1db2db70c2ba76d321ecb2b815db64595a27d2e
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env::set_var("RUST_LOG", "info");
    dotenv::dotenv().ok();
    init_tracing().context("failed to set up tracing")?;

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

    Ok(())
}
