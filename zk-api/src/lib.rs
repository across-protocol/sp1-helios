use alloy::hex;
use alloy_primitives::{keccak256, Bytes, FixedBytes, B256};
use alloy_rlp::Encodable;
use alloy_trie::{proof, Nibbles};
use anyhow::anyhow;
use futures::future::FutureExt;
use std::time::Duration;
use tracing::warn;

use helios_ethereum::config::{checkpoints, networks::Network};
use sp1_helios_primitives::types::{ContractStorage, VerifiedStorageSlot};

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

use rpc_proxies::multiplex;

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

/// Upper bound on a single checkpoint request, covering connection, headers and body decoding.
/// Without it a primary RPC that accepts the connection but never answers would block the
/// sequential fallback loop and keep healthy backups from ever being tried.
pub const CHECKPOINT_RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// Fetch checkpoint from a slot number. This function only works for slots that are the 1st slot in their epoch
pub async fn try_get_checkpoint(slot: u64) -> anyhow::Result<B256> {
    let rpcs =
        std::env::var("CONSENSUS_RPCS_LIST").map_err(|_| anyhow!("CONSENSUS_RPCS_LIST not set"))?;
    let rpcs: Vec<String> = rpcs
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    try_get_checkpoint_from(&rpcs, slot, CHECKPOINT_RPC_TIMEOUT).await
}

/// Fetch the beacon block root at `slot`, trying the primary RPC first and falling back to
/// the remaining RPCs concurrently (through the shared [`multiplex`] helper) if it fails.
/// Each request, including its response body, is bounded by `timeout` so a stalled
/// endpoint cannot block the fallback or hang the call.
pub async fn try_get_checkpoint_from(
    rpcs: &[String],
    slot: u64,
    timeout: Duration,
) -> anyhow::Result<B256> {
    let (primary, backups) = rpcs
        .split_first()
        .ok_or_else(|| anyhow!("no consensus RPCs configured in CONSENSUS_RPCS_LIST"))?;

    let client = reqwest::Client::builder().timeout(timeout).build()?;

    // 1) Try the primary RPC first
    match fetch_block_root(&client, primary, slot).await {
        Ok(root) => return Ok(root),
        Err(e) => warn!(
            target: "checkpoint",
            "Primary RPC failed; falling back to backups for slot {}: {}", slot, e
        ),
    }

    // 2) Race the backups and take the first that answers
    multiplex(
        |rpc: String| {
            let client = client.clone();
            async move { fetch_block_root(&client, &rpc, slot).await }.boxed()
        },
        backups,
    )
    .await
    .map_err(|e| anyhow!("All RPCs failed to fetch checkpoint for slot {slot}: {e}"))
}

/// Fetch the beacon block root at `slot` from a single beacon API endpoint.
async fn fetch_block_root(client: &reqwest::Client, rpc: &str, slot: u64) -> anyhow::Result<B256> {
    // The beacon block root at `slot` *is* the checkpoint. Query the standard beacon API
    // directly: the helios `ConsensusRpc` trait no longer exposes block fetching.
    #[derive(serde::Deserialize)]
    struct HeaderResponse {
        data: HeaderData,
    }
    #[derive(serde::Deserialize)]
    struct HeaderData {
        root: B256,
    }

    let url = format!(
        "{}/eth/v1/beacon/headers/{}",
        rpc.trim_end_matches('/'),
        slot
    );
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow!("request to {rpc} failed: {e}"))?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "{rpc} returned status {} for slot {slot}",
            response.status()
        ));
    }
    let header = response
        .json::<HeaderResponse>()
        .await
        .map_err(|e| anyhow!("failed to parse header response from {rpc}: {e}"))?;
    Ok(header.data.root)
}

/**
 * @dev this function is copied over from `program/src/main.rs` and used to mimic the Merkle tree proof part of the ZK program
 * It is used to confirm the validity of the Merkle proof we receive from execution RPCs. This function is modified to return an error instaed of panicking
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
