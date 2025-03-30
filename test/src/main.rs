use alloy::{
    eips::BlockId,
    providers::{Provider, ProviderBuilder},
    rpc::types::BlockTransactionsKind,
};
use alloy_primitives::{address, keccak256, Address, Bytes, B256};
use alloy_rlp::Encodable;
use alloy_trie::{self, Nibbles, TrieAccount};
use anyhow::{Context, Result};
use std::env;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();

    // Convert u64 to storage slot representations needed for EVM interactions
    fn storage_slot_from_u64(slot: u64) -> (alloy_primitives::U256, B256) {
        let u256_slot = alloy_primitives::U256::from(slot);

        // Create right-aligned 32-byte representation for B256
        let mut b256_bytes = [0u8; 32];
        let slot_bytes = slot.to_be_bytes();
        b256_bytes[32 - slot_bytes.len()..].copy_from_slice(&slot_bytes);

        (u256_slot, B256::from(b256_bytes))
    }

    // Testing HubPool->owner() which is at storage slot 1
    // Expected owner: 0xB524735356985D2f267FA010D681f061DfF03715
    let contract_address = address!("0xc186fA914353c44b2E33eBE05f21846F1048bEda");
    let expected_owner = address!("0xB524735356985D2f267FA010D681f061DfF03715");
    let slot_index = 1u64;
    let (u256_slot, storage_slot) = storage_slot_from_u64(slot_index);

    println!("Storage slot index: {}", slot_index);
    println!("As U256: {:?}", u256_slot);
    println!("As B256: {:?}", storage_slot);

    // Setup execution RPC client
    let execution_rpc = env::var("SOURCE_EXECUTION_RPC_URL")
        .context("SOURCE_EXECUTION_RPC_URL environment variable not set")?;
    let provider = ProviderBuilder::new().on_http(execution_rpc.parse()?);

    // Get latest block and its state root
    let block = provider
        .get_block(BlockId::latest(), BlockTransactionsKind::Hashes)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Block not found"))?;
    let state_root = block.header.state_root;
    let block_number = block.header.number;
    println!("Using block number: {}", block_number);
    println!("State root: {:?}", state_root);

    // Get the proof using eth_getProof
    let proof = provider
        .get_proof(contract_address, vec![storage_slot])
        .block_id(BlockId::number(block_number))
        .await
        .context("Failed to retrieve storage proof")?;

    println!("Retrieved proof for contract: {:?}", contract_address);
    println!("Account proof nodes: {}", proof.account_proof.len());
    println!(
        "Storage proof nodes: {}",
        proof.storage_proof[0].proof.len()
    );
    println!("Contract balance: {:?}", proof.balance);
    println!("Contract state root: {:?}", proof.storage_hash);
    println!("Contract code hash: {:?}", proof.code_hash);

    // STEP 1: Verify the account proof in the global state trie
    // -------------------------------------------------------------
    // Prepare the address key (keccak256 hash of address)
    let address_hash = keccak256(contract_address.as_slice());
    let address_nibbles = Nibbles::unpack(Bytes::copy_from_slice(address_hash.as_ref()));

    // RLP-encode the account data (this is what's stored in the state trie)
    let trie_acc = TrieAccount {
        nonce: proof.nonce,
        balance: proof.balance,
        storage_root: proof.storage_hash,
        code_hash: proof.code_hash,
    };
    let mut rlp_encoded_trie_account = Vec::new();
    trie_acc.encode(&mut rlp_encoded_trie_account);

    // Verify account exists in global state trie
    if let Err(e) = alloy_trie::proof::verify_proof(
        state_root,
        address_nibbles,
        Some(rlp_encoded_trie_account),
        &proof.account_proof,
    ) {
        panic!(
            "Could not verify the contract's `TrieAccount` in the global MPT for address {}: {}",
            contract_address, e
        );
    }
    println!("VERIFIED ACCOUNT PROOF!");

    // STEP 2: Verify the storage proof in the contract's storage trie
    // -------------------------------------------------------------
    // Get the actual storage value using RPC
    let slot_val = provider
        .get_storage_at(contract_address, u256_slot)
        .block_id(BlockId::number(block_number))
        .await?;

    // Extract address from the 32-byte storage value (addresses are right-padded)
    let bts = slot_val.to_be_bytes::<32>();
    let hubpool_owner = Address::from_slice(&bts[12..]);

    // Assert that the retrieved owner equals the expected owner
    assert_eq!(expected_owner, hubpool_owner, "HubPool owner mismatch");
    println!("hubpool owner: {:?}", hubpool_owner);

    // Prepare the storage key (keccak256 hash of storage slot)
    let storage_key_hash = keccak256(storage_slot.as_slice());
    let storage_key_nibbles = Nibbles::unpack(Bytes::copy_from_slice(storage_key_hash.as_ref()));

    // RLP-encode the owner address for verification
    let mut rlp_owner = Vec::new();
    hubpool_owner.encode(&mut rlp_owner);

    println!("Verifying storage proof for slot: {:?}", storage_slot);
    println!("Expected owner: {:?}", hubpool_owner);

    // Verify storage value exists in contract's storage trie
    if let Err(e) = alloy_trie::proof::verify_proof(
        proof.storage_hash,
        storage_key_nibbles,
        Some(rlp_owner),
        &proof.storage_proof[0].proof,
    ) {
        panic!(
            "Could not verify the storage proof for slot {}: {}",
            u256_slot, e
        );
    }
    println!("VERIFIED STORAGE PROOF!");

    Ok(())
}
