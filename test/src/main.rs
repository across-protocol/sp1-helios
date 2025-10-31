use alloy::{
    eips::BlockId,
    providers::{Provider, ProviderBuilder},
};
use alloy_primitives::{address, b256, keccak256, Address, Bytes, B256};
use anyhow::{Context, Result};
use std::env;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();

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
    let provider = ProviderBuilder::new().connect_http(execution_rpc.parse()?);

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
    let provider = ProviderBuilder::new().connect_http(execution_rpc.parse()?);

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
