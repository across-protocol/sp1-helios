use alloy::hex;
use alloy_primitives::{keccak256, Bytes, FixedBytes, B256};
use alloy_rlp::Encodable;
use alloy_trie::{proof, Nibbles};
use anyhow::anyhow;
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
use sp1_helios_primitives::types::{ContractStorage, VerifiedStorageSlot};
use tracing::info;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use tree_hash::TreeHash;

use std::sync::Arc;
use tokio::sync::{mpsc::channel, watch};
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

/**
 * @dev this function is copied over from `program/src/main.rs` and used to mimic the Merkle tree proof part of the ZK program
 * It is used to confirm the validity of the Merkle proof we receive from execution RPCs. This function is modified to return an error instead of panicking
 */
pub fn verify_storage_slot_proofs(
    execution_state_root: FixedBytes<32>,
    contract_storage: ContractStorage,
) -> anyhow::Result<Vec<VerifiedStorageSlot>> {
    // Convert the contract address into nibbles for the global MPT proof
    // We need to keccak256 the address before converting to nibbles for the MPT proof
    let address_hash = keccak256(contract_storage.address.as_slice());
    let address_nibbles = Nibbles::unpack(Bytes::copy_from_slice(address_hash.as_ref()));
    // RLP-encode the `TrieAccount`. This is what's actually stored in the global MPT
    let mut rlp_encoded_trie_account = Vec::new();
    contract_storage
        .expected_value
        .encode(&mut rlp_encoded_trie_account);

    // 1) Verify the contract's account node in the global MPT:
    //    We expect to find `rlp_encoded_trie_account` as the trie value for this address.
    if let Err(e) = proof::verify_proof(
        execution_state_root,
        address_nibbles,
        Some(rlp_encoded_trie_account),
        &contract_storage.mpt_proof,
    ) {
        return Err(anyhow!(
            "Could not verify the contract's `TrieAccount` in the global MPT for address {}: {:#?}",
            hex::encode(contract_storage.address),
            e
        ));
    }

    // 2) Now that we've verified the contract's `TrieAccount`, use it to verify each storage slot proof
    let mut verified_slots = Vec::with_capacity(contract_storage.storage_slots.len());
    for slot in contract_storage.storage_slots {
        let key = slot.key;
        let value = slot.expected_value;
        // We need to keccak256 the slot key before converting to nibbles for the MPT proof
        let key_hash = keccak256(key.as_slice());
        let key_nibbles = Nibbles::unpack(Bytes::copy_from_slice(key_hash.as_ref()));
        // RLP-encode expected value. This is what's actually stored in the contract MPT
        let mut rlp_encoded_value = Vec::new();
        value.encode(&mut rlp_encoded_value);

        // Verify the storage proof under the *contract's* storage root
        if let Err(e) = proof::verify_proof(
            contract_storage.expected_value.storage_root,
            key_nibbles,
            Some(rlp_encoded_value),
            &slot.mpt_proof,
        ) {
            return Err(anyhow!(
                "Storage proof invalid for slot {}: {:#?}",
                hex::encode(key),
                e
            ));
        }

        verified_slots.push(VerifiedStorageSlot {
            key,
            value: FixedBytes(value.to_be_bytes()),
            contractAddress: contract_storage.address,
        });
    }

    Ok(verified_slots)
}
