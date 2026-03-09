# SP1 Helios

Extends [SP1 Helios](https://succinctlabs.github.io/sp1-helios/) to support proving source chain contract storage slots, in addition to consensus transitions. Messages can be stored on the source chain and proved on the destination chain, effectively creating a ZK-powered message bridge.

## Architecture

- **program/** — SP1 ZK program (guest code). Verifies beacon chain sync committee updates, finality, and storage proofs.
- **zk-api/** — Axum server that tracks the beacon chain and dispatches proof requests to Succinct's prover network. Uses Redis for state.
- **primitives/** — Shared types (`ProofInputs`, `ProofOutputs`, `StorageSlot`, etc.)
- **elf/** — Pre-built ZK program binary.
- **cli/** — `genesis` (initial contract state) and `vkey` (verification key) binaries.

## Quick Start

```bash
# Build
cargo build

# Run ZK-API (needs .env)
cargo run --bin sp1-helios-api

# Generate genesis state
cargo run --bin genesis -- --slot <SLOT>

# Print vkey
cargo run --bin vkey

# Rebuild ELF (requires sp1 toolchain + Docker)
just update-elf
```

## Environment

Copy `.env.example` to `.env` and fill in RPC URLs and Redis config. See `.env.example` for all variables.
