#![no_main]
sp1_zkvm::entrypoint!(main);

use alloy_primitives::{B256, U256};
use alloy_sol_types::SolValue;
use helios_consensus_core::{
    apply_finality_update, apply_update, verify_finality_update, verify_update,
};
use sp1_helios_primitives::execution::verified_execution_fields;
use sp1_helios_primitives::types::{ProofInputs, ProofOutputs};
use sp1_helios_primitives::verify_storage_slot_proofs;
use tree_hash::TreeHash;

/// Program flow:
/// 1. Verify and apply sync committee updates, if any
/// 2. Verify and apply finality update
/// 3. Ensure finalized head was incremented, to confirm integrity of execution part of finalized header
/// 4. Authenticate the execution header (Gloas witness or embedded payload header)
/// 5. Verify storage slot proofs
/// 6. Commit new state root, header, and sync committee for usage in the on-chain contract
pub fn main() {
    let encoded_inputs = sp1_zkvm::io::read_vec();

    let ProofInputs {
        sync_committee_updates,
        finality_update,
        expected_current_slot,
        mut store,
        genesis_root,
        forks,
        contract_storage_slots,
        execution_header,
    } = serde_cbor::from_slice(&encoded_inputs).unwrap();

    // Don't allow `next_sync_committee`, a free input, to be propagated to ProofOutputs. It will only
    // be set if there's a valid sync committee update
    store.next_sync_committee = None;

    let start_sync_committee_hash = store.current_sync_committee.tree_hash_root();
    let prev_header: B256 = store.finalized_header.beacon().tree_hash_root();
    let prev_head = store.finalized_header.beacon().slot;

    // 1. Verify and apply sync committee updates, if any
    for (index, update) in sync_committee_updates.iter().enumerate() {
        println!(
            "Processing update {} of {}.",
            index + 1,
            sync_committee_updates.len()
        );
        let update_is_valid =
            verify_update(update, expected_current_slot, &store, genesis_root, &forks).is_ok();

        if !update_is_valid {
            panic!("Update {} is invalid!", index + 1);
        }
        println!("Update {} is valid.", index + 1);
        apply_update(&mut store, update);
    }

    // 2. Verify and apply finality update
    let finality_update_is_valid = verify_finality_update(
        &finality_update,
        expected_current_slot,
        &store,
        genesis_root,
        &forks,
    )
    .is_ok();
    if !finality_update_is_valid {
        panic!("Finality update is invalid!");
    }
    println!("Finality update is valid.");

    apply_finality_update(&mut store, &finality_update);

    // 3. Ensure finalized head was incremented, to confirm integrity of execution part of finalized header
    // This check confirms the execution_payload referenced by the finalized_header corresponds to
    // this newer slot, due to checks against beacon_body_root performed during update verification.
    assert!(store.finalized_header.beacon().slot > prev_head);

    // 4. Authenticate the execution header. Pre-Gloas, the finalized light-client header embeds
    // the execution payload header (proven against body_root during update verification).
    // From Gloas, it only commits to the builder bid's parent execution block hash, so the
    // supplied witness header must hash to that commitment before its fields are trusted.
    let execution = verified_execution_fields(&store.finalized_header, execution_header.as_ref())
        .expect("Execution header failed to verify.");
    let execution_state_root = execution.state_root;

    // 5. Verify storage slot proofs
    let verified_slots = verify_storage_slot_proofs(execution_state_root, &contract_storage_slots)
        .expect("Storage slot proofs failed to verify.");

    // 6. Commit new state root, header, and sync committee for usage in the on-chain contract
    let header: B256 = store.finalized_header.beacon().tree_hash_root();
    let sync_committee_hash: B256 = store.current_sync_committee.tree_hash_root();
    let next_sync_committee_hash: B256 = match &mut store.next_sync_committee {
        Some(next_sync_committee) => next_sync_committee.tree_hash_root(),
        None => B256::ZERO,
    };
    let head = store.finalized_header.beacon().slot;

    let proof_outputs = ProofOutputs {
        executionStateRoot: execution_state_root,
        newHeader: header,
        nextSyncCommitteeHash: next_sync_committee_hash,
        newHead: U256::from(head),
        prevHeader: prev_header,
        prevHead: U256::from(prev_head),
        syncCommitteeHash: sync_committee_hash,
        startSyncCommitteeHash: start_sync_committee_hash,
        slots: verified_slots,
    };
    sp1_zkvm::io::commit_slice(&proof_outputs.abi_encode());
}
