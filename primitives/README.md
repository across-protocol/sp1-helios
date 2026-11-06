# sp1-helios-primitives

Code shared by the ZK program (`program/`, guest) and the ZK-API (`zk-api/`, host). Anything
here is compiled into the ELF, so a change to this crate changes the program's verification
key.

## Modules

- **`types`** — The proof interface. `ProofInputs` is what the host serializes for the guest:
  the light-client store, sync-committee and finality updates, the contract storage to prove
  (`ContractStorage`, `StorageSlot`) and, from Gloas onwards, the `execution_header` witness.
  `ProofOutputs` and `VerifiedStorageSlot` are the ABI-encoded public values the on-chain
  verifier consumes.
- **`execution`** — `verified_execution_fields` anchors the execution `state_root`,
  `block_number` and `block_hash` to a verified finalized light-client header. Pre-Gloas
  headers embed the execution payload header, so the fields are read directly. Gloas
  (EIP-7732) headers only commit to an execution block hash, so a witness header is required
  and must hash to that commitment. The guest runs this check before verifying storage proofs
  against the state root; the host runs the same check when assembling `ProofInputs` and when
  resolving the finalized execution block number.
- **`storage`** — `verify_storage_slot_proofs` checks a contract's account proof against that
  `state_root` and each requested slot proof against the account's `storage_root`, returning the
  `VerifiedStorageSlot`s the program commits to. The guest aborts the proof on failure; the host
  runs the same function to reject a bad `eth_getProof` response before requesting a proof.

## Tests

`tests/execution.rs` exercises both header shapes, and `tests/storage.rs` builds synthetic
account and storage tries with alloy-trie's `HashBuilder` to cover the proof verifier. Neither
needs RPC data:

```bash
cargo test -p sp1-helios-primitives
```
