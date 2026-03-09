//! Manual dev utilities for inspecting live chain data and Redis state.
//! These are `#[ignore]`d tests meant to be run individually with `--nocapture`.
//!
//! # Setup
//!   Requires `.env` at the workspace root (see `.env.example`).
//!   Redis tests need a running Redis instance (`REDIS_URL`).
//!   RPC tests need valid `SOURCE_EXECUTION_RPC_URL` (comma-separated list OK, first is used).
//!
//! # Usage
//! ```sh
//! # Run a single test with output:
//! cargo test -p zk-api --test manual read_storage_slot -- --ignored --nocapture
//!
//! # Run all manual tests:
//! cargo test -p zk-api --test manual -- --ignored --nocapture
//! ```

use alloy::{
    eips::BlockId,
    hex,
    providers::{Provider, ProviderBuilder},
};
use alloy_primitives::{address, keccak256, Address, Bytes, B256, U256};
use anyhow::{Context, Result};
use sp1_helios_api::redis_store::RedisStore;
use std::env;
use tree_hash::TreeHash;

fn load_env() {
    dotenv::dotenv().ok();
}

/// Returns the first URL from the comma-separated `SOURCE_EXECUTION_RPC_URL`.
fn execution_rpc_url() -> String {
    let raw = env::var("SOURCE_EXECUTION_RPC_URL").expect("SOURCE_EXECUTION_RPC_URL not set");
    raw.split(',')
        .next()
        .expect("SOURCE_EXECUTION_RPC_URL is empty")
        .trim()
        .to_string()
}

// ---- Redis tests ----

#[tokio::test]
#[ignore]
async fn read_finalized_header_from_redis() {
    load_env();

    let mut redis_store = RedisStore::<String>::new()
        .await
        .expect("failed to connect to Redis");

    match redis_store.read_finalized_header().await {
        Ok(Some(header)) => {
            let checkpoint = header.beacon().state_root.tree_hash_root();
            println!(
                "Finalized header — slot: {}, checkpoint: {}",
                header.beacon().slot,
                hex::encode(checkpoint)
            );
        }
        Ok(None) => println!("No finalized header in Redis"),
        Err(e) => panic!("Error reading finalized header: {e}"),
    }
}

// ---- Storage proof tests ----

/// Calculates the storage slot for a Solidity mapping entry.
/// For `mapping(uint => uint)` at slot `p`, key `k`: `keccak256(pad32(k) ++ pad32(p))`
fn calculate_mapping_slot(mapping_slot: u64, key: u64) -> B256 {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&U256::from(key).to_be_bytes::<32>());
    buf.extend_from_slice(&U256::from(mapping_slot).to_be_bytes::<32>());
    keccak256(&buf)
}

async fn get_storage_at(contract: Address, slot: B256) -> Result<Bytes> {
    let rpc = execution_rpc_url();
    let provider = ProviderBuilder::new().connect_http(rpc.parse()?);
    let value = provider
        .get_storage_at(contract, U256::from_be_bytes(slot.into()))
        .block_id(BlockId::latest())
        .await
        .with_context(|| format!("get_storage_at failed for {contract:?} slot {slot:?}"))?;
    Ok(Bytes::from(value.to_be_bytes::<32>()))
}

#[tokio::test]
#[ignore]
async fn read_storage_slot() {
    load_env();

    let slot = calculate_mapping_slot(0, 0);
    println!("Computed slot: {slot:?}");

    let contract = address!("0xdD6Fa55b12aA2a937BA053d610D76f20cC235c09");
    let value = get_storage_at(contract, slot)
        .await
        .expect("failed to read storage");
    println!("Storage value: {value:?}");
}

#[tokio::test]
#[ignore]
async fn get_storage_proof() {
    load_env();

    let rpc = execution_rpc_url();
    let provider = ProviderBuilder::new().connect_http(rpc.parse().unwrap());

    let contract = address!("0xdD6Fa55b12aA2a937BA053d610D76f20cC235c09");
    let storage_key = calculate_mapping_slot(0, 0);

    let proof = provider
        .get_proof(contract, vec![storage_key])
        .block_id(BlockId::latest())
        .await
        .expect("get_proof failed");

    let value = B256::from(proof.storage_proof[0].value.to_be_bytes());
    println!("Proof value (B256): {value:?}");
}
