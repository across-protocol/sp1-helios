# ZK API

The host service tracks beacon-chain finality, accepts storage-proof requests, and dispatches
proofs to the prover backend. `src/` owns the service implementation; `tests/` holds checkpoint
RPC tests and ignored manual utilities. `build.rs` builds the guest ELF.

## Finality state ownership

The finality loop in `src/proof_service.rs` resolves execution headers before acquiring the
global Redis lock. It owns publishing and migrating the finalized execution cache. Request
handlers only read the stored snapshot; a missing or mismatched cache asks a new request to
retry, while existing requests can still return their status.

`src/redis_store.rs` binds the cached block number to the beacon header root. A conditional
Redis script publishes the header, execution metadata and affected proof states together only
if the previously read header is still current. The loop can repair an unchanged header, so
migration does not wait for another finalized epoch. A legacy writer that updates only the
header invalidates the cache automatically. The old header JSON remains readable by legacy
deployments; the previous unbound numeric cache is ignored by this version.

Global locks have ownership tokens. A failed contender or an expired owner cannot release
another owner's lock. Conditional publication also protects the snapshot if a legacy worker
releases a lock it does not own. Proof-generation locks are a separate existing mechanism.

## Tests

Install `redis-server`, then run `SP1_SKIP_PROGRAM_BUILD=true cargo test -p zk-api`.
The finality tests in `src/proof_service/tests.rs` start private Redis and RPC servers; their
fixtures live in `src/proof_service/tests/support.rs`. They never load `.env` or use a running
development Redis instance. Manual utilities in `tests/manual.rs` remain ignored by default.
