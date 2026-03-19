# SP1 v5 to v6 Migration Guide

This document describes every change made to migrate sp1-helios from SP1 v5.2.1 to SP1 v6.0.2, including the reasoning behind non-obvious decisions.

---

## 1. Version Pin Updates

SP1 pins its version in 8 locations (documented in `CLAUDE.md`). All were updated from `5.2.1` to `6.0.2`:

| File | Change |
|------|--------|
| `Cargo.toml` | `sp1-sdk` and `sp1-build` workspace deps |
| `program/Cargo.toml` | `sp1-zkvm` (not workspace-managed; runs inside the ZK VM) |
| `zk-api/build.rs` | Docker image tag for reproducible ELF build |
| `justfile` | Docker image tag for local ELF rebuild |
| `.github/workflows/elf.yml` | `sp1up --version` and `--tag` in CI |
| `CLAUDE.md` / `AGENTS.md` | Documentation references |

After updating, `cargo update` was run to refresh `Cargo.lock`.

---

## 2. sp1-sdk Feature Flags

```toml
# Before (v5)
sp1-sdk = { version = "5.2.1", default-features = false, features = ["network"] }

# After (v6)
sp1-sdk = { version = "6.0.2", default-features = false, features = ["network", "blocking", "native-gnark"] }
```

**Why `blocking`:** SP1 v6 reorganized its API into async-first and blocking modules. The blocking API (`sp1_sdk::blocking::*`) is now behind a feature flag. Our codebase uses the blocking prover inside `tokio::task::spawn_blocking`, so this feature is required.

**Why `native-gnark`:** SP1 v6 moved Groth16/PLONK proof wrapping behind this feature flag. Without it, `.groth16()` is not available on prove requests.

**Alternative considered:** Migrating to the async SP1 API. This was rejected because the prover is CPU-intensive and already runs inside `spawn_blocking`. Switching to async would add complexity without benefit — the async API is designed for the network prover, but our `spawn_blocking` pattern already handles that correctly.

---

## 3. Precompile Patch Updates

SP1 uses patched versions of crypto crates to replace expensive operations with precompiled VM instructions. Each patch must match the SP1 version.

### 3a. Standard patch version bumps

All patches updated from `sp1-4.0.0` / `sp1-5.0.0` tags to `sp1-6.0.0`:

```toml
sha2-v0-9-9:  patch-sha2-0.9.9-sp1-4.0.0  → patch-sha2-0.9.9-sp1-6.0.0
sha3-v0-10-8: patch-sha3-0.10.8-sp1-4.0.0  → patch-sha3-0.10.8-sp1-6.0.0
tiny-keccak:  patch-2.0.2-sp1-4.0.0        → patch-2.0.2-sp1-6.0.0
```

### 3b. sha2 v0.10 patch: target version change

```toml
# Before
sha2-v0-10-8 = { ..., tag = "patch-sha2-0.10.8-sp1-4.0.0" }

# After
sha2-v0-10-9 = { ..., tag = "patch-sha2-0.10.9-sp1-6.0.0" }
```

**Why 0.10.9 instead of 0.10.8:** The dependency tree (via `alloy` 1.x upgrades) now resolves `sha2` to version `0.10.9`, not `0.10.8`. A `[patch.crates-io]` entry is silently ignored if the target version doesn't exist in the dependency graph. With the old `0.10.8` patch, cargo emitted:

```
warning: patch `sha2 v0.10.8 (...)` was not used in the crate graph
```

This meant sha2 0.10.x operations inside the ZK VM would use the unpatched (slow) implementation. Updating the patch to target `0.10.9` ensures the precompile is actually applied.

### 3c. bls12_381 patch: digest version conflict

```toml
# Before
bls12_381 = { ..., tag = "patch-0.8.0-sp1-5.0.0-v2" }

# After
bls12_381 = { ..., tag = "patch-0.8.0-sp1-6.0.0-v2" }
```

This was the most complex patch issue in the migration. The initial attempt used `patch-0.8.0-sp1-6.0.0` (without `-v2`), which caused a `digest` crate version conflict:

- The `sp1-6.0.0` bls12_381 patch depends on `digest 0.10.7`
- Helios uses `sha2 0.9.9`, which depends on `digest 0.9.0`
- Helios BLS verification code passes `sha2::Sha256` (implementing `digest 0.9.0::FixedOutput`) to `bls12_381::HashToCurve` (expecting `digest 0.10.7::FixedOutput`)
- This produced trait bound errors at compile time

The `-v2` tag for the bls12_381 patch resolves this incompatibility.

**Alternative considered:** Removing the bls12_381 patch entirely. This would compile but leave BLS signature verification unoptimized inside the ZK VM — a significant performance regression for a light client that verifies BLS signatures on every update.

**Alternative considered:** Keeping the old `sp1-5.0.0-v2` tag. This was rejected because patch crates must match the SP1 version to use the correct precompile syscall interface. A v5 patch running against the v6 VM runtime would produce incorrect results or panic.

---

## 4. SDK API Changes

### 4a. ELF loading

```rust
// Before (v5)
const ELF: &[u8] = include_bytes!("../../../elf/sp1-helios-elf");

// After (v6) — in zk-api (uses build.rs)
const ELF: Elf = include_elf!("sp1-helios-program");

// After (v6) — in CLI binaries (use pre-built ELF)
const HELIOS_ELF: &[u8] = include_bytes!("../../elf/sp1-helios-elf");
// then: Elf::Static(HELIOS_ELF)
```

SP1 v6 introduced a typed `Elf` enum (`Elf::Static(&'static [u8])` / `Elf::Dynamic(Arc<[u8]>)`) replacing raw `&[u8]`. The `include_elf!` macro now returns `Elf` directly.

**Choice: `include_elf!` vs `include_bytes!` + `Elf::Static`:**

- `zk-api` uses `include_elf!` because it has a `build.rs` that compiles the guest program via `sp1-build`. The `include_elf!` macro reads from the build output directory and is the canonical approach.
- CLI binaries (`genesis`, `vkey`) use `include_bytes!` + `Elf::Static()` because they read the pre-built ELF from `elf/sp1-helios-elf` on disk. They don't compile the program themselves.

### 4b. Prover client initialization

```rust
// Before (v5)
let prover_client = ProverClient::from_env();

// After (v6) — in zk-api
let prover_client = EnvProver::new();

// After (v6) — in CLI binaries
let client = MockProver::new();
```

SP1 v6 replaced the unified `ProverClient` with separate concrete types: `EnvProver`, `MockProver`, `CpuProver`, `CudaProver`. `EnvProver` reads `SP1_PROVER` from the environment (same behavior as `ProverClient::from_env()` in v5).

**Choice: `EnvProver::new()` vs `ProverClient::from_env()`:** In v6, `ProverClient::from_env()` still exists as an alias that returns `EnvProver`. We use `EnvProver::new()` directly because it makes the type explicit, which matters for the struct field type (`Arc<EnvProver>`).

**Choice: `MockProver` for CLI tools:** The CLI tools (`genesis`, `vkey`) only need to derive the verification key from the ELF — they never generate proofs. `MockProver` is the lightest prover that can do `setup()`. The v5 code used `ProverClient::builder().cpu().build()`, which unnecessarily initialized the full CPU prover. `MockProver` is more correct for this use case.

### 4c. Key setup

```rust
// Before (v5)
let (pk, vk) = prover_client.setup(ELF);

// After (v6)
let pk = prover_client.setup(ELF)?;              // returns Result
let vk = pk.verifying_key();                      // accessed via ProvingKey trait
```

In v6, `setup()` returns `Result<ProvingKey>` (fallible) and the verifying key is accessed through the `ProvingKey` trait method `verifying_key()`, rather than being returned as a separate value from a tuple.

### 4d. Proving key type

```rust
// Before (v5)
proving_key: Arc<SP1ProvingKey>,

// After (v6)
proving_key: Arc<<EnvProver as Prover>::ProvingKey>,
```

**Why the associated type syntax:** In v6, each prover has its own proving key type. `EnvProver`'s key is `EnvProvingKey`, but this type is not publicly exported from `sp1_sdk` (the `env` module is private). Using `<EnvProver as Prover>::ProvingKey` references it through the trait's associated type, which is the intended API.

**Alternative considered:** Using `Arc<dyn ProvingKey>` (trait object). This would work but adds dynamic dispatch overhead and requires the `ProvingKey` trait to be object-safe. The associated type approach is zero-cost and type-safe.

### 4e. Trait imports

```rust
// New required imports in v6
use sp1_sdk::ProvingKey;    // trait, for .verifying_key()
use sp1_sdk::blocking::ProveRequest;  // trait, for .groth16() and .run()
use sp1_sdk::blocking::Prover;       // trait, for .setup() and .prove()
```

In v5, `verifying_key` was a direct field access (`pk.vk`), `prove` was a method on `ProverClient`, and proof mode was set via the same builder. In v6, these are split across traits.

### 4f. Prove call

```rust
// Before (v5)
prover_client.prove(&proving_key, &stdin).groth16().run()

// After (v6)
prover_client.prove(&proving_key, stdin).groth16().run()
```

`prove()` now takes `SP1Stdin` by value instead of by reference. This is a minor signature change.

### 4g. Error handling in prove

```rust
// Before (v5) — prove().run() returned anyhow::Result
prover_client.prove(&proving_key, &stdin).groth16().run()
// → Result<SP1ProofWithPublicValues, anyhow::Error>

// After (v6) — prove().run() returns Result with prover-specific error
prover_client.prove(&proving_key, stdin).groth16().run()
// → Result<SP1ProofWithPublicValues, P::Error>
```

The v6 `ProveRequest::run()` returns `Result<..., P::Error>` where `P::Error` is the prover's associated error type (not `anyhow::Error`). We map the error immediately with `.map_err(|e| anyhow!(...))` to keep the downstream error handling consistent.

### 4h. Verification key access

```rust
// Before (v5)
self.proving_key.vk.bytes32_raw()

// After (v6)
self.proving_key.verifying_key().bytes32_raw()
```

Direct field access (`pk.vk`) was replaced with the `ProvingKey::verifying_key()` trait method. The `bytes32_raw()` and `bytes32()` methods on `HashableKey` are unchanged.

---

## 5. Removed Code

### 5a. `utils::setup_logger()` removed from genesis CLI

```rust
// Removed
sp1_sdk::utils::setup_logger();
```

This utility was removed or moved in v6. The genesis CLI doesn't need SP1-specific logging setup — it uses standard `tracing` via helios.

---

## 6. Guest Program (No Changes Required)

The guest program (`program/src/main.rs`) required no code changes. The v6 `sp1-zkvm` crate maintains backward compatibility for:

- `sp1_zkvm::entrypoint!(main)` macro
- `sp1_zkvm::io::read_vec()` for reading inputs
- `sp1_zkvm::io::commit_slice()` for committing outputs

Only the version in `program/Cargo.toml` was bumped.

---

## 7. Remaining Steps

After merging these changes:

1. **Rebuild the ELF binary** — Run `just update-elf` (requires Docker + SP1 toolchain v6.0.2). CI will verify ELF reproducibility via `elf.yml`.
2. **Install the SP1 toolchain** — Run `sp1up --version 6.0.2` on development machines.
3. **Verify proof generation** — Test with `SP1_PROVER=mock` first, then against the prover network.
