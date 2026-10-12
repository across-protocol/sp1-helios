//! Types and verification logic shared by the SP1 Helios guest program and its hosts.

use alloy_primitives::{keccak256, Bytes, FixedBytes, B256};
use alloy_rlp::Encodable;
use alloy_trie::{proof, Nibbles};
use anyhow::{bail, Result};

use crate::types::{ContractStorage, VerifiedStorageSlot};

pub mod execution;
pub mod types;

/// Verify a contract's storage slot proofs against an execution state root.
///
/// This checks that:
/// - the contract's [`alloy_trie::TrieAccount`] is included in the global state trie rooted at
///   `execution_state_root`, and
/// - each requested storage slot is included in the contract's storage trie rooted at that
///   account's `storage_root`.
///
/// The same function runs inside the ZK program (where a failure aborts the proof) and on the host
/// (where a failure rejects an RPC response before any proof is requested), so both sides apply
/// exactly the same rules.
pub fn verify_storage_slot_proofs(
    execution_state_root: FixedBytes<32>,
    contract_storage: &ContractStorage,
) -> Result<Vec<VerifiedStorageSlot>> {
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
        bail!(
            "Could not verify the contract's `TrieAccount` in the global MPT for address {:#x}: {e}",
            contract_storage.address
        );
    }

    // 2) Now that we've verified the contract's `TrieAccount`, use it to verify each storage slot proof
    let mut verified_slots = Vec::with_capacity(contract_storage.storage_slots.len());
    for slot in &contract_storage.storage_slots {
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
            bail!("Storage proof invalid for slot {key:#x}: {e}");
        }

        verified_slots.push(VerifiedStorageSlot {
            key,
            value: B256::from(value.to_be_bytes::<32>()),
            contractAddress: contract_storage.address,
        });
    }

    Ok(verified_slots)
}
