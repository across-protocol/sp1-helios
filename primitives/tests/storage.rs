//! Storage-slot proof verification against synthetic account and storage tries.
//!
//! No RPC data is needed: the storage trie is built with alloy-trie's `HashBuilder`, its root
//! becomes the `storage_root` of a `TrieAccount`, and that account is inserted into a synthetic
//! state trie. Proofs are retained for the keys under test exactly as a node would serve them
//! from `eth_getProof`, so the verifier sees the same shapes it does in production.

use alloy_primitives::{address, b256, keccak256, Address, Bytes, B256, U256};
use alloy_rlp::Encodable;
use alloy_trie::{proof::ProofRetainer, HashBuilder, Nibbles, TrieAccount};
use sp1_helios_primitives::storage::verify_storage_slot_proofs;
use sp1_helios_primitives::types::{ContractStorage, StorageSlot, VerifiedStorageSlot};

const TARGET: Address = address!("0x1111111111111111111111111111111111111111");
const OTHER: Address = address!("0x2222222222222222222222222222222222222222");
const SLOT_A: B256 = b256!("0x0000000000000000000000000000000000000000000000000000000000000000");
const SLOT_B: B256 = b256!("0x0000000000000000000000000000000000000000000000000000000000000007");
const SLOT_UNPROVEN: B256 =
    b256!("0x00000000000000000000000000000000000000000000000000000000000000ff");

/// A trie built from `(preimage, rlp value)` leaves, with proofs retained for `targets`.
struct Trie {
    root: B256,
    proofs: Vec<(Nibbles, Vec<Bytes>)>,
}

impl Trie {
    /// Keys are preimages; they are hashed the same way the verifier hashes them.
    fn build(leaves: &[(&[u8], Vec<u8>)], targets: &[&[u8]]) -> Self {
        let targets: Vec<Nibbles> = targets
            .iter()
            .map(|t| Nibbles::unpack(keccak256(t)))
            .collect();
        let mut hb = HashBuilder::default()
            .with_proof_retainer(ProofRetainer::from_iter(targets.iter().copied()));

        let mut sorted: Vec<(Nibbles, &Vec<u8>)> = leaves
            .iter()
            .map(|(k, v)| (Nibbles::unpack(keccak256(k)), v))
            .collect();
        sorted.sort_by_key(|(k, _)| *k);
        for (k, v) in sorted {
            hb.add_leaf(k, v);
        }

        let root = hb.root();
        let nodes = hb.take_proof_nodes();
        let proofs = targets
            .into_iter()
            .map(|t| {
                let proof = nodes
                    .matching_nodes_sorted(&t)
                    .into_iter()
                    .map(|(_, node)| node)
                    .collect();
                (t, proof)
            })
            .collect();
        Self { root, proofs }
    }

    fn proof_for(&self, preimage: &[u8]) -> Vec<Bytes> {
        let key = Nibbles::unpack(keccak256(preimage));
        self.proofs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, p)| p.clone())
            .expect("no proof retained for key")
    }
}

/// `VerifiedStorageSlot` is `sol!`-generated and has no `Debug`, so `unwrap_err` can't be used.
fn error_message(result: anyhow::Result<Vec<VerifiedStorageSlot>>) -> String {
    match result {
        Ok(_) => panic!("expected verification to fail"),
        Err(e) => e.to_string(),
    }
}

fn rlp<T: Encodable>(value: &T) -> Vec<u8> {
    let mut out = Vec::new();
    value.encode(&mut out);
    out
}

/// Builds a state root plus a `ContractStorage` proving `slots` for `TARGET`, alongside an
/// unrelated account so the state trie has more than one leaf.
fn fixture(slots: &[(B256, U256)]) -> (B256, ContractStorage) {
    let storage_leaves: Vec<(&[u8], Vec<u8>)> = slots
        .iter()
        .map(|(key, value)| (key.as_slice(), rlp(value)))
        .chain([(SLOT_UNPROVEN.as_slice(), rlp(&U256::from(99)))])
        .collect();
    let storage_targets: Vec<&[u8]> = slots.iter().map(|(key, _)| key.as_slice()).collect();
    let storage = Trie::build(&storage_leaves, &storage_targets);

    let account = TrieAccount {
        nonce: 1,
        balance: U256::from(1_000),
        storage_root: storage.root,
        ..Default::default()
    };
    let other = TrieAccount {
        nonce: 7,
        ..Default::default()
    };
    let state = Trie::build(
        &[
            (TARGET.as_slice(), rlp(&account)),
            (OTHER.as_slice(), rlp(&other)),
        ],
        &[TARGET.as_slice()],
    );

    let contract_storage = ContractStorage {
        address: TARGET,
        expected_value: account,
        mpt_proof: state.proof_for(TARGET.as_slice()),
        storage_slots: slots
            .iter()
            .map(|(key, value)| StorageSlot {
                key: *key,
                expected_value: *value,
                mpt_proof: storage.proof_for(key.as_slice()),
            })
            .collect(),
    };
    (state.root, contract_storage)
}

#[test]
fn verifies_account_and_slot_proofs() {
    let slots = [(SLOT_A, U256::from(42)), (SLOT_B, U256::MAX)];
    let (state_root, contract_storage) = fixture(&slots);

    let verified = verify_storage_slot_proofs(state_root, &contract_storage).unwrap();

    assert_eq!(verified.len(), 2);
    for (slot, (key, value)) in verified.iter().zip(slots) {
        assert_eq!(slot.key, key);
        assert_eq!(slot.value, B256::from(value.to_be_bytes::<32>()));
        assert_eq!(slot.contractAddress, TARGET);
    }
}

#[test]
fn verifies_with_no_slots_requested() {
    let (state_root, contract_storage) = fixture(&[]);
    let verified = verify_storage_slot_proofs(state_root, &contract_storage).unwrap();
    assert!(verified.is_empty());
}

#[test]
fn rejects_wrong_state_root() {
    let (_, contract_storage) = fixture(&[(SLOT_A, U256::from(42))]);

    let err = error_message(verify_storage_slot_proofs(B256::ZERO, &contract_storage));

    assert!(
        err.to_string()
            .contains("Could not verify the contract's `TrieAccount`"),
        "{err}"
    );
}

#[test]
fn rejects_tampered_account() {
    let (state_root, mut contract_storage) = fixture(&[(SLOT_A, U256::from(42))]);
    contract_storage.expected_value.nonce += 1;

    let err = error_message(verify_storage_slot_proofs(state_root, &contract_storage));

    assert!(
        err.to_string()
            .contains("Could not verify the contract's `TrieAccount`"),
        "{err}"
    );
}

#[test]
fn rejects_tampered_slot_value() {
    let (state_root, mut contract_storage) = fixture(&[(SLOT_A, U256::from(42))]);
    contract_storage.storage_slots[0].expected_value = U256::from(43);

    let err = error_message(verify_storage_slot_proofs(state_root, &contract_storage));

    assert!(err.contains("Storage proof invalid for slot"), "{err}");
}

#[test]
fn rejects_slot_proof_from_another_slot() {
    let (state_root, mut contract_storage) =
        fixture(&[(SLOT_A, U256::from(42)), (SLOT_B, U256::MAX)]);
    // Present slot A's value with slot B's proof.
    let proof_b = contract_storage.storage_slots[1].mpt_proof.clone();
    contract_storage.storage_slots[0].mpt_proof = proof_b;

    let err = error_message(verify_storage_slot_proofs(state_root, &contract_storage));

    assert!(err.contains("Storage proof invalid for slot"), "{err}");
}

#[test]
fn rejects_slot_that_was_not_proven() {
    let (state_root, mut contract_storage) = fixture(&[(SLOT_A, U256::from(42))]);
    // Claim the unproven slot holds its real value, but with slot A's proof.
    contract_storage.storage_slots[0].key = SLOT_UNPROVEN;
    contract_storage.storage_slots[0].expected_value = U256::from(99);

    let err = error_message(verify_storage_slot_proofs(state_root, &contract_storage));

    assert!(err.contains("Storage proof invalid for slot"), "{err}");
}
