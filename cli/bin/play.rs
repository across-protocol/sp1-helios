#![allow(unused_imports)]
// Playground
use alloy::hex;
use anyhow::Context;
use sp1_helios_api::{init_tracing, redis_store::RedisStore};
use std::env;
use tracing::info;
use tree_hash::TreeHash;

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
