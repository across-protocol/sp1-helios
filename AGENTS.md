# SP1 Helios

ZK light client for Ethereum's beacon chain, built on Succinct's SP1 ZKVM. Proves beacon chain state transitions and storage slot values, verifiable on-chain.

## Architecture

```
Client → ZK-API (Axum + Redis) → SP1 Prover Network → Proof
                                                         ↓
                                              On-chain verification
```

**ZK Program** (`program/`) — Guest code running inside SP1 VM. Verifies sync committee updates, finality updates, and storage proofs (MPT). Compiles to ELF binary.

**ZK-API** (`zk-api/`) — Axum server that tracks the beacon chain, accepts proof requests, and dispatches them to Succinct's prover network. Uses Redis for state. Entry: `zk-api/src/main.rs`. Key modules:
- `api.rs` — REST endpoints (`POST /v1/proof`, `GET /v1/proof/{id}`, etc.)
- `proof_service.rs` — Core loop: fetch requests, wait for finality, generate proofs
- `proof_backends/sp1.rs` — SP1 network integration
- `redis_store.rs` — Proof state persistence
- `consensus.rs` — Beacon chain client (wraps Helios)

**Primitives** (`primitives/`) — Shared types: `ProofInputs`, `ProofOutputs`, `StorageSlot`, `ContractStorage`.

**ELF** (`elf/sp1-helios-elf`) — Pre-built ZK program binary (~1.6MB). Rebuilt via `cargo prove build --docker --tag v5.2.1`.

**CLI** (`cli/`) — Binaries:
- `genesis` — Generates initial state for contract deployment
- `vkey` — Prints verification key from the ELF

## Build & Run

```bash
# Install SP1 toolchain (must match project version, NOT latest)
sp1up --version 5.2.1

# Build everything
cargo build

# Rebuild ELF (requires sp1 toolchain + Docker)
just update-elf

# Run ZK-API (needs .env with RPC URLs + Redis)
cargo run --bin sp1-helios-api

# Generate genesis state
cargo run --bin genesis -- --slot <SLOT>

# Print vkey
cargo run --bin vkey

# Lint
cargo fmt --all -- --check
cargo clippy --all-features --all-targets
```

## Key Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| sp1-zkvm / sp1-sdk | 5.2.1 | ZK VM and proof generation |
| helios-consensus-core / helios-ethereum | 0.9.4 | Beacon chain consensus |
| alloy | 1.0.37 | Ethereum types and RPC |
| axum | 0.8.3 | HTTP server |
| redis | 0.26.0 | State store |

## CI

- **pr.yaml** — `cargo fmt` + `cargo clippy`
- **elf.yml** — Rebuild ELF in Docker, verify no diff (reproducibility check)
- **release-binaries.yml** — Cross-platform binary releases on tag push

## Environment

See `.env.example`. Key vars: `SOURCE_CONSENSUS_RPC_URL`, `SOURCE_EXECUTION_RPC_URL`, `REDIS_URL`, `SP1_PROVER` (mock/network).
