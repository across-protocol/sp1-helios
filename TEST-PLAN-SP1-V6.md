# SP1 v6 Migration — Test Plan

## Prerequisites

Before running any tests, ensure the following are installed and configured:

- [ ] **SP1 toolchain v6.0.2**: `curl -L https://sp1.succinct.xyz | bash && sp1up --version 6.0.2`
- [ ] **Docker**: Running and accessible (`docker info` succeeds)
- [ ] **just**: `brew install just`
- [ ] **Redis**: Running locally on port 6379 (needed for zk-api tests)
- [ ] **Environment file**: `.env` populated from `.env.example` with valid RPC URLs

---

## Phase 1: Local Build Verification

These tests verify the code compiles correctly and the tooling works.

### 1.1 Format check

```bash
cargo fmt --all -- --check
```

- [ ] **Pass**: No formatting diffs

### 1.2 Clippy

```bash
cargo clippy --all-features --all-targets -- -D warnings -A incomplete-features
```

- [ ] **Pass**: No warnings or errors

### 1.3 ELF rebuild (Docker)

This is the most important build test — it verifies the guest program compiles inside the SP1 v6 Docker image with the pinned dependency versions.

```bash
just update-elf
```

- [ ] **Pass**: Build completes without errors
- [ ] **Pass**: `elf/sp1-helios-elf` is produced

### 1.4 ELF reproducibility

After rebuilding the ELF, check whether it matches the committed version:

```bash
git diff elf/
```

- [ ] **Note result**: If the ELF changed, that's expected on the first v6 build (the v5 ELF is being replaced). Commit the new ELF. On subsequent builds, there should be no diff.

---

## Phase 2: CLI Smoke Tests

These tests verify the CLI tools work with the v6 SDK and the new ELF.

### 2.1 Verification key generation

```bash
cargo run --bin vkey
```

- [ ] **Pass**: Prints a verification key in the format `"0x..."` (64 hex chars after prefix)
- [ ] **Pass**: No panics or errors

**What this validates**: ELF loading via `include_bytes!` + `Elf::Static()`, `MockProver::setup()`, `ProvingKey` trait (`verifying_key()`), `HashableKey` trait (`bytes32()`).

### 2.2 Genesis generation

Requires `.env` with valid `CONSENSUS_RPCS_LIST`, `SOURCE_EXECUTION_RPC_URL`, `PRIVATE_KEY`, and `SP1_PROVER=mock`.

```bash
cargo run --bin genesis -- --slot <RECENT_FINALIZED_SLOT> --env-file .env --out contracts
```

To find a recent finalized slot, check a beacon chain explorer or use:
```bash
curl -s <CONSENSUS_RPC>/eth/v1/beacon/headers/finalized | jq '.data.header.message.slot'
```

- [ ] **Pass**: `contracts/genesis.json` is created
- [ ] **Pass**: JSON contains non-zero values for `heliosProgramVkey`, `header`, `syncCommitteeHash`, `head`
- [ ] **Pass**: `heliosProgramVkey` matches the output of `cargo run --bin vkey`

**What this validates**: Full helios consensus client sync, beacon chain RPC connectivity, MockProver key setup, verification key derivation matches between vkey and genesis binaries.

---

## Phase 3: Mock Prover Integration

Tests the zk-api server end-to-end using the mock prover (no real proof generation).

### 3.1 Start the API server

Set `.env`:
```
SP1_PROVER=mock
CONSENSUS_RPCS_LIST=<beacon_rpc_url>
SOURCE_EXECUTION_RPC_URL=<execution_rpc_url>
REDIS_URL=redis://127.0.0.1:6379
REDIS_KEY_PREFIX=sp1-helios-test
```

```bash
cargo run --bin sp1-helios-api
```

- [ ] **Pass**: Server starts without panics
- [ ] **Pass**: Logs show `SP1Backend` initialization succeeding (`SP1 keys setup complete`)
- [ ] **Pass**: Logs show beacon chain sync starting

**What this validates**: `EnvProver::new()` reads `SP1_PROVER=mock` correctly, `setup(ELF)` succeeds with the `include_elf!` macro, Redis connection works.

### 3.2 Submit a proof request

While the server is running:

```bash
curl -X POST http://localhost:3000/v1/proof \
  -H "Content-Type: application/json" \
  -d '{"vkey": "<VKEY_FROM_2.1>"}'
```

- [ ] **Pass**: Returns a proof request ID
- [ ] **Pass**: `GET /v1/proof/<id>` eventually shows a completed proof
- [ ] **Pass**: Response includes non-empty `proof` and `publicValues` hex strings

**What this validates**: Full proof pipeline — input construction, `SP1Stdin` serialization, `prove().groth16().run()` with mock prover, `SP1ProofWithPublicValues` output formatting.

---

## Phase 4: Network Prover Integration

Tests real proof generation on the Succinct prover network. **This costs prover credits.**

### 4.1 Network proof generation

Set `.env`:
```
SP1_PROVER=network
NETWORK_PRIVATE_KEY=<your_private_key>
```

Start the server and submit a proof request (same as Phase 3).

- [ ] **Pass**: Server starts and connects to the prover network
- [ ] **Pass**: Proof request is submitted to the network
- [ ] **Pass**: Proof completes (this may take several minutes)
- [ ] **Pass**: Returned Groth16 proof is valid (non-empty, different from mock proof)

**What this validates**: `EnvProver` correctly initializes the network prover, SP1 v6 network API compatibility, Groth16 wrapping works end-to-end.

---

## Phase 5: CI Pipeline

Push the branch and verify all CI checks pass.

### 5.1 PR workflow (`pr.yaml`)

- [ ] **Pass**: `cargo fmt --all -- --check` passes
- [ ] **Pass**: `cargo clippy --all-features --all-targets` passes

### 5.2 ELF workflow (`elf.yml`)

- [ ] **Pass**: SP1 toolchain v6.0.2 installs successfully in CI
- [ ] **Pass**: Docker ELF build completes
- [ ] **Pass**: ELF reproducibility check passes (no diff against committed ELF)

**Note**: The ELF check will fail until the new v6 ELF is committed. The workflow sequence should be:
1. Build ELF locally with `just update-elf`
2. Commit the new ELF
3. Push — CI verifies reproducibility

### 5.3 PR lint (`pr_lint.yaml`)

- [ ] **Pass**: PR title follows semantic convention (e.g., `feat: migrate to SP1 v6`)

---

## Phase 6: Verification Key Stability

The verification key determines which proofs the on-chain verifier accepts. A vkey change means the contract must be updated.

### 6.1 Compare vkey to v5

```bash
# On main branch (v5)
git stash && cargo run --bin vkey
# Note the key

# On migration branch (v6)
git stash pop && cargo run --bin vkey
# Compare
```

- [ ] **Note result**: The vkey **will** change (different SP1 version = different circuit = different vkey). This is expected. Document the old and new vkeys for the contract update.

---

## Test Priority

If time is limited, run tests in this order:

| Priority | Test | Why |
|----------|------|-----|
| 1 | 1.3 ELF rebuild | Validates the Docker build works with pinned deps |
| 2 | 2.1 vkey | Fastest smoke test of the v6 SDK API changes |
| 3 | 3.1 + 3.2 Mock prover | Validates the full proof pipeline |
| 4 | 5.1 + 5.2 CI | Validates the build is reproducible |
| 5 | 4.1 Network prover | Validates real proof generation (costs credits) |
| 6 | 2.2 Genesis | Validates beacon chain integration |

---

## Known Issues / Gotchas

- **`cargo update` must be selective**: Running `cargo update` (without `-p`) will upgrade transitive deps (`unicode-ident`, `autocfg`, `serde`, etc.) to versions incompatible with the SP1 zkVM Docker image's Rust nightly. Always use `cargo update -p <package>` for specific crates.

- **`include_elf!` path changed**: In v6, `sp1-build` with `docker: true` outputs to `target/elf-compilation/docker/...` instead of `target/elf-compilation/...`. This only affects local development with `SP1_SKIP_PROGRAM_BUILD=true`.

- **ELF must be rebuilt**: The committed ELF from v5 will not work with v6 provers. The ELF must be rebuilt before any proof generation tests.
