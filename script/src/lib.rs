use alloy_primitives::B256;
use helios_consensus_core::{
    calc_sync_period,
    consensus_spec::MainnetConsensusSpec,
    types::{BeaconBlock, Update},
};
use helios_ethereum::{
    config::{checkpoints, networks::Network, Config},
    consensus::Inner,
    rpc::http_rpc::HttpRpc,
};
use helios_ethereum::{consensus::ConsensusClient, database::ConfigDB, rpc::ConsensusRpc};
use log::info;

use std::sync::Arc;
use tokio::sync::{mpsc::channel, watch};
use tree_hash::TreeHash;
pub mod api;
pub mod proof_backends;
pub mod proof_service;
pub mod redis_store;
pub mod rpc_proxies;
pub mod types;
pub mod util;

pub const MAX_REQUEST_LIGHT_CLIENT_UPDATES: u8 = 128;

/// Fetch updates for client
pub async fn get_updates(
    client: &Inner<MainnetConsensusSpec, HttpRpc>,
) -> Vec<Update<MainnetConsensusSpec>> {
    try_get_updates(client).await.unwrap()
}

/// Fetch updates for client
pub async fn try_get_updates(
    client: &Inner<MainnetConsensusSpec, HttpRpc>,
) -> anyhow::Result<Vec<Update<MainnetConsensusSpec>>> {
    let period =
        calc_sync_period::<MainnetConsensusSpec>(client.store.finalized_header.beacon().slot);

    let updates = client
        .rpc
        .get_updates(period, MAX_REQUEST_LIGHT_CLIENT_UPDATES)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    Ok(updates)
}

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

/// Fetch checkpoint from a slot number. This function only works for slots that are the 1st slot in their epoch
pub async fn try_get_checkpoint(slot: u64) -> anyhow::Result<B256> {
    let consensus_rpc = std::env::var("SOURCE_CONSENSUS_RPC_URL")
        .map_err(|e| anyhow::anyhow!("SOURCE_CONSENSUS_RPC_URL not set: {}", e))?;
    let chain_id = std::env::var("SOURCE_CHAIN_ID")
        .map_err(|e| anyhow::anyhow!("SOURCE_CHAIN_ID not set: {}", e))?;
    let network = Network::from_chain_id(
        chain_id
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid SOURCE_CHAIN_ID: {}", e))?,
    )
    .map_err(|e| anyhow::anyhow!("Unknown network for chain ID: {} . Error: {}", chain_id, e))?;
    let base_config = network.to_base_config();

    let config = Config {
        consensus_rpc: consensus_rpc.to_string(),
        execution_rpc: String::new(),
        chain: base_config.chain,
        forks: base_config.forks,
        strict_checkpoint_age: false,
        ..Default::default()
    };

    let (block_send, _) = channel(256);
    let (finalized_block_send, _) = watch::channel(None);
    let (channel_send, _) = watch::channel(None);
    let client = Inner::<MainnetConsensusSpec, HttpRpc>::new(
        &consensus_rpc,
        block_send,
        finalized_block_send,
        channel_send,
        Arc::new(config),
    );

    let block: BeaconBlock<MainnetConsensusSpec> = client
        .rpc
        .get_block(slot)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    Ok(B256::from_slice(block.tree_hash_root().as_ref()))
}

/// Setup a client from a checkpoint.
pub async fn get_client(checkpoint: B256) -> Inner<MainnetConsensusSpec, HttpRpc> {
    try_get_client(checkpoint).await.unwrap()
}

/// Setup a client from a checkpoint.
pub async fn try_get_client(
    checkpoint: B256,
) -> anyhow::Result<Inner<MainnetConsensusSpec, HttpRpc>> {
    let consensus_rpc = std::env::var("SOURCE_CONSENSUS_RPC_URL")
        .map_err(|e| anyhow::anyhow!("Failed to get SOURCE_CONSENSUS_RPC_URL: {}", e))?;
    let chain_id = std::env::var("SOURCE_CHAIN_ID")
        .map_err(|e| anyhow::anyhow!("Failed to get SOURCE_CHAIN_ID: {}", e))?;
    let network = Network::from_chain_id(
        chain_id
            .parse()
            .map_err(|e| anyhow::anyhow!("Failed to parse chain_id: {}", e))?,
    )
    .map_err(|_| anyhow::anyhow!("Failed to get network from chain_id: {}", chain_id))?;
    let base_config = network.to_base_config();

    let config = Config {
        consensus_rpc: consensus_rpc.to_string(),
        execution_rpc: String::new(),
        chain: base_config.chain,
        forks: base_config.forks,
        strict_checkpoint_age: false,
        ..Default::default()
    };

    let (block_send, _) = channel(256);
    let (finalized_block_send, _) = watch::channel(None);
    let (channel_send, _) = watch::channel(None);

    let mut client = Inner::new(
        &consensus_rpc,
        block_send,
        finalized_block_send,
        channel_send,
        Arc::new(config),
    );

    client
        .bootstrap(checkpoint)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bootstrap client with checkpoint: {}", e))?;

    Ok(client)
}

/// Creates a ConsensusClient that performs auto-updates every 12 seconds. Does all the required
/// verifications on the state applied and streams out finalized blocks
pub async fn create_streaming_client(
    checkpoint: B256,
) -> ConsensusClient<MainnetConsensusSpec, HttpRpc, ConfigDB> {
    let consensus_rpc = std::env::var("SOURCE_CONSENSUS_RPC_URL").unwrap();
    let chain_id = std::env::var("SOURCE_CHAIN_ID").unwrap();
    let network = Network::from_chain_id(chain_id.parse().unwrap()).unwrap();
    let base_config = network.to_base_config();

    let config = Config {
        consensus_rpc: consensus_rpc.to_string(),
        execution_rpc: String::new(),
        chain: base_config.chain,
        forks: base_config.forks,
        checkpoint: Some(checkpoint),
        strict_checkpoint_age: false,
        ..Default::default()
    };

    info!("CONFIG: {:?}", config);

    info!("config.max_checkpoint_age: {:?}", config.max_checkpoint_age);

    ConsensusClient::new(&consensus_rpc, Arc::new(config)).unwrap()
}
