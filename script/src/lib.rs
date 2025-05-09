use alloy_primitives::B256;
use helios_consensus_core::{
    calc_sync_period,
    consensus_spec::{ConsensusSpec, MainnetConsensusSpec},
    types::{BeaconBlock, Update},
};
use helios_ethereum::{
    config::{checkpoints, networks::Network, Config},
    consensus::Inner,
};
use helios_ethereum::{consensus::ConsensusClient, database::ConfigDB, rpc::ConsensusRpc};
use rpc_proxies::consensus::ConsensusRpcProxy;
use tracing::info;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use std::sync::Arc;
use tokio::sync::{mpsc::channel, watch};
use tree_hash::TreeHash;
pub mod api;
pub mod consensus_client;
pub mod proof_backends;
pub mod proof_service;
pub mod redis_store;
pub mod rpc_proxies;
pub mod slack_layer;
pub mod types;
pub mod util;

use slack_layer::SlackLayer;
use std::str::FromStr;
use tracing::Level;

pub const MAX_REQUEST_LIGHT_CLIENT_UPDATES: u8 = 128;
pub const CONSENSUS_RPC_ENV_VAR: &str = "SOURCE_CONSENSUS_RPC_URL";

/// Fetch updates for client
pub async fn get_updates(
    client: &Inner<MainnetConsensusSpec, ConsensusRpcProxy>,
) -> Vec<Update<MainnetConsensusSpec>> {
    try_get_updates(client).await.unwrap()
}

/// Fetch updates for client
pub async fn try_get_updates<S: ConsensusSpec, R: ConsensusRpc<S>>(
    client: &Inner<S, R>,
) -> anyhow::Result<Vec<Update<S>>> {
    let period = calc_sync_period::<S>(client.store.finalized_header.beacon().slot);

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
        consensus_rpc: String::new(), // don't think it's used
        execution_rpc: None,
        chain: base_config.chain,
        forks: base_config.forks,
        strict_checkpoint_age: false,
        ..Default::default()
    };

    let (block_send, _) = channel(256);
    let (finalized_block_send, _) = watch::channel(None);
    let (channel_send, _) = watch::channel(None);
    let client = Inner::<MainnetConsensusSpec, ConsensusRpcProxy>::new(
        CONSENSUS_RPC_ENV_VAR,
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
pub async fn get_client<S: ConsensusSpec, R: ConsensusRpc<S>>(checkpoint: B256) -> Inner<S, R> {
    try_get_client(checkpoint).await.unwrap()
}

/// Setup a client from a checkpoint.
pub async fn try_get_client<S: ConsensusSpec, R: ConsensusRpc<S>>(
    checkpoint: B256,
) -> anyhow::Result<Inner<S, R>> {
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
        consensus_rpc: String::new(), // I don't think it's used
        execution_rpc: None,
        chain: base_config.chain,
        forks: base_config.forks,
        strict_checkpoint_age: false,
        ..Default::default()
    };

    let (block_send, _) = channel(256);
    let (finalized_block_send, _) = watch::channel(None);
    let (channel_send, _) = watch::channel(None);

    let mut client = Inner::new(
        CONSENSUS_RPC_ENV_VAR,
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
) -> ConsensusClient<MainnetConsensusSpec, ConsensusRpcProxy, ConfigDB> {
    let consensus_rpc = std::env::var("SOURCE_CONSENSUS_RPC_URL").unwrap();
    let chain_id = std::env::var("SOURCE_CHAIN_ID").unwrap();
    let network = Network::from_chain_id(chain_id.parse().unwrap()).unwrap();
    let base_config = network.to_base_config();

    let config = Config {
        consensus_rpc: consensus_rpc.to_string(),
        execution_rpc: None,
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

pub fn init_tracing() -> anyhow::Result<()> {
    // 1) Read RUST_LOG or default to "info" for console/default filtering
    let default_filter =
        EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new("info"))?;

    // 2) Prepare base subscriber registry with console output
    let subscriber = tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stdout))
        .with(default_filter); // Apply default filtering globally first

    // 3) Conditionally add Slack layer
    match std::env::var("SLACK_WEBHOOK_URL") {
        Ok(webhook_url) if !webhook_url.is_empty() => {
            // Determine Slack log level threshold
            let slack_level_str =
                std::env::var("SLACK_LOG_LEVEL").unwrap_or_else(|_| "WARN".to_string());
            let slack_threshold =
                Level::from_str(&slack_level_str.to_uppercase()).unwrap_or(Level::WARN); // Default to WARN if parsing fails

            println!(
                "Initializing Slack tracing layer with webhook URL and level >= {}",
                slack_threshold
            ); // Use println! as tracing might not be fully init yet

            let slack_layer = SlackLayer::new(webhook_url, slack_threshold);

            // Add the Slack layer to the subscriber
            subscriber.with(slack_layer).init();
        }
        _ => {
            // No Slack webhook URL found, just init with console
            println!("No SLACK_WEBHOOK_URL found, skipping Slack layer initialization.");
            subscriber.init();
        }
    }

    Ok(())
}
