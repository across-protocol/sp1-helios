use alloy_primitives::B256;

use helios_ethereum::config::{checkpoints, networks::Network};

pub mod api;
pub mod consensus_client;
pub mod proof_backends;
pub mod proof_service;
pub mod redis_store;
pub mod rpc_proxies;
pub mod types;
pub mod util;
// Expose tracing setup without shadowing the `tracing` crate
#[path = "tracing/mod.rs"]
pub mod tracing_setup;
pub use tracing_setup::init_tracing;

/// Fetch latest checkpoint from chain to bootstrap client to the latest state.
/// Panics if any step fails.
pub async fn get_latest_checkpoint() -> B256 {
    try_get_latest_checkpoint().await.unwrap()
}

/// Fetch latest checkpoint from chain to bootstrap client to the latest state.
/// Returns an error if any step fails.
pub async fn try_get_latest_checkpoint() -> anyhow::Result<B256> {
    let cf = checkpoints::CheckpointFallback::new()
        .build()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to build checkpoint fallback: {}", e))?;

    let chain_id =
        std::env::var("SOURCE_CHAIN_ID").map_err(|_| anyhow::anyhow!("SOURCE_CHAIN_ID not set"))?;
    let network = Network::from_chain_id(
        chain_id
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid SOURCE_CHAIN_ID: {}", e))?,
    )
    .map_err(|e| anyhow::anyhow!("Unknown network for chain ID: {}", e))?;

    cf.fetch_latest_checkpoint(&network)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch latest checkpoint: {}", e))
}

/// Fetch checkpoint from a slot number. This function only works for slots that are the 1st slot in their epoch
pub async fn get_checkpoint(slot: u64) -> B256 {
    try_get_checkpoint(slot).await.unwrap()
}

/// Fetch checkpoint from a slot number. This function only works for slots that are the 1st slot in their epoch.
/// Delegates to [`consensus_client::checkpoint::fetch_checkpoint`], which owns the beacon API
/// request, its timeout and the primary-then-backups fallback policy.
pub async fn try_get_checkpoint(slot: u64) -> anyhow::Result<B256> {
    consensus_client::checkpoint::fetch_checkpoint(slot).await
}
