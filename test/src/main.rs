use alloy::{
    eips::BlockId,
    hex,
    providers::{Provider, ProviderBuilder},
    rpc::{self, types::BlockTransactionsKind},
};
use alloy_primitives::{address, b256, keccak256, ruint::aliases::U256, Address, Bytes, B256};
use alloy_rlp::Encodable;
use alloy_trie::{self, Nibbles, TrieAccount};
use anyhow::{Context, Result};
use std::{env, hash::Hash, io::Read, sync::Arc};

// Imports needed for the new function
use alloy_primitives::FixedBytes;
use tree_hash::TreeHash; // Alias for B256 if needed

// Imports for using Helios client logic
use helios_consensus_core::consensus_spec::MainnetConsensusSpec;
// Remove Bootstrap, HttpRpc, ConsensusRpc imports as we use the higher-level client
// use helios_consensus_core::types::Bootstrap;
// use helios_ethereum::rpc::http_rpc::HttpRpc;
// use helios_ethereum::rpc::ConsensusRpc;

// Import functions from sp1_helios_script
// use sp1_helios_script::{get_client, get_latest_checkpoint};

// --- Remove Structs for deserializing Beacon API responses ---
// #[derive(Deserialize, Debug)]
// struct GenesisDataResponse { ... }
// #[derive(Deserialize, Debug)]
// struct GenesisDetails { ... }
// #[derive(Deserialize, Debug)]
// struct SpecDataResponse { ... }
// #[derive(Deserialize, Debug)]
// struct ChainSpec { ... }
// #[derive(Deserialize, Debug)]
// struct HeaderDataResponse { ... }
// #[derive(Deserialize, Debug)]
// struct HeaderMessage { ... }
// #[derive(Deserialize, Debug)]
// struct SignedHeaderContainer { ... }
// #[derive(Deserialize, Debug)]
// struct BeaconBlockHeaderData { ... }

// Remove Custom serde helper
// mod serde_string { ... }

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();

    // 0xc830c96d418b17d3cfcf4d1b59ed0db631bd6cd21ab6b98922700bb6b606a9a3
    // print_keccak256("message-to-prove");

    fetch_latest_header_info().await?;
    return Ok(());

    // 0xad3228b676f7d3cd4284a5443f17f1962b36e491b30a40b2405849e597ba5fb5
    let slot = calculate_mapping_slot(0u64, 0u64);
    println!("{:?}", slot);

    // addr = 0xdD6Fa55b12aA2a937BA053d610D76f20cC235c09
    // bts = 0xe955e8270bae4ffaee6178c273b4041cb56abb9b5f4dc6a0aff5bfd232d2a6ea
    let bts =
        get_bytes_from_storage_slot(address!("0xdD6Fa55b12aA2a937BA053d610D76f20cC235c09"), slot)
            .await?;
    println!("{:?}", bts);

    let hub_pool_store_addr = address!("0xdD6Fa55b12aA2a937BA053d610D76f20cC235c09");
    let storage_key = b256!("ad3228b676f7d3cd4284a5443f17f1962b36e491b30a40b2405849e597ba5fb5");
    // let storage_keys: Vec<StorageKey> = vec![storage_key];

    // Setup execution RPC client
    let execution_rpc = env::var("SOURCE_EXECUTION_RPC_URL")
        .context("SOURCE_EXECUTION_RPC_URL environment variable not set")?;
    let provider = ProviderBuilder::new().on_http(execution_rpc.parse()?);

    let proof = provider
        .get_proof(hub_pool_store_addr, vec![storage_key])
        .block_id(BlockId::latest())
        .await?;

    // Convert the U256 value to B256 for consistent printing
    let value_u256 = proof.storage_proof[0].value;
    let value_b256 = B256::from(value_u256.to_be_bytes());

    println!("get_proof val (B256): {:?}", value_b256);

    Ok(())
}

/// Fetches the latest finalized header, header hash, and execution state root from the Ethereum network.
///
/// This function connects to the Ethereum execution layer to retrieve the latest
/// finalized block information.
///
/// # Returns
/// A tuple containing:
/// - The slot number (block number) of the latest finalized header
/// - The hash of the latest finalized header (as B256)
/// - The execution state root (as B256)
///
/// # Errors
/// Returns an error if:
/// - Environment variable for SOURCE_EXECUTION_RPC_URL is not set
/// - Connection to the execution layer fails
/// - Required data cannot be retrieved (e.g., finalized block not found)
async fn fetch_latest_header_info() -> Result<(u64, B256, B256)> {
    // Setup execution RPC client
    let execution_rpc = env::var("SOURCE_EXECUTION_RPC_URL")
        .context("SOURCE_EXECUTION_RPC_URL environment variable not set")?;
    let provider = ProviderBuilder::new().on_http(execution_rpc.parse()?);

    // Get the latest finalized block directly from the execution client
    let finalized_block = provider
        .get_block(BlockId::finalized(), BlockTransactionsKind::Hashes)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Finalized block not found"))?;

    let header = finalized_block.header;
    let slot = header.number; // Block number serves as the slot here
    let header_hash = header.hash; // Get the block hash
    let execution_state_root = header.state_root;

    println!("Latest finalized slot/block number: {}", slot);
    println!("Header hash: 0x{:x}", header_hash);
    println!("Execution state root: 0x{:x}", execution_state_root);
    println!("Timestamp: {}", header.timestamp);

    Ok((slot, header_hash, execution_state_root))
}

fn print_keccak256(input: &str) {
    // Convert string to bytes and calculate hash
    let hash = keccak256(input.as_bytes());

    // Print the resulting hash
    println!("keccak256 of '{}': {}", input, hash);
}

/// Calculates the storage slot for an entry in a mapping.
///
/// In Solidity, for a mapping at slot `p`, the slot for key `k` is:
/// keccak256(concat(pad32(k), pad32(p)))
///
/// # Arguments
/// * `mapping_slot` - The storage slot of the mapping itself (e.g., 0)
/// * `key` - The key in the mapping
///
/// # Returns
/// The calculated storage slot as a U256
fn calculate_mapping_slot(mapping_slot: u64, key: u64) -> alloy_primitives::B256 {
    // Convert to U256
    let mapping_slot = alloy_primitives::U256::from(mapping_slot);
    let key = alloy_primitives::U256::from(key);

    // Prepare buffer for concatenating key and slot
    let mut buffer = Vec::with_capacity(64);

    // Add key as 32-byte big-endian value
    buffer.extend_from_slice(&key.to_be_bytes::<32>());

    // Add mapping slot as 32-byte big-endian value
    buffer.extend_from_slice(&mapping_slot.to_be_bytes::<32>());

    keccak256(&buffer)
}

/// Retrieves raw bytes from a contract's storage at a specific slot.
///
/// # Arguments
/// * `contract_address` - The address of the contract
/// * `storage_slot` - The storage slot to read from
///
/// # Returns
/// The value at the storage slot as raw bytes, or an error
async fn get_bytes_from_storage_slot(
    contract_address: Address,
    storage_slot: alloy_primitives::B256,
) -> Result<Bytes> {
    // Validate inputs
    assert!(
        contract_address != Address::ZERO,
        "Contract address cannot be zero"
    );

    // Setup execution RPC client
    let execution_rpc = env::var("SOURCE_EXECUTION_RPC_URL")
        .context("SOURCE_EXECUTION_RPC_URL environment variable not set")?;
    let provider = ProviderBuilder::new().on_http(execution_rpc.parse()?);

    // Get storage at slot
    let storage_value = provider
        .get_storage_at(
            contract_address,
            alloy_primitives::U256::from_be_bytes(storage_slot.into()),
        )
        .block_id(BlockId::latest())
        .await
        .with_context(|| {
            format!(
                "Failed to get storage at address {:?}, slot {:?}",
                contract_address, storage_slot
            )
        })?;

    // Convert storage value to hex bytes (32-byte big-endian representation)
    let bytes_value = Bytes::from(storage_value.to_be_bytes::<32>());
    Ok(bytes_value)
}
