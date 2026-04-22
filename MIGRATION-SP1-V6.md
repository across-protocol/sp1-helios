# SP1 v6.0.2 to v6.1.0 Migration Guide

This document describes the changes made on this branch to upgrade `sp1-helios` from SP1 `v6.0.2` to `v6.1.0`.

Unlike the earlier `v5 -> v6` migration, this upgrade did not require Rust API changes in the application code. The work here is primarily a coordinated version bump across the pinned SP1 surfaces, a lockfile refresh, and validation that the existing code still compiles against the new SDK.

---

## 1. Version Pin Updates

SP1 is pinned in several places that must stay in sync. These were updated from `6.0.2` to `6.1.0`:

| File | Change |
|------|--------|
| `Cargo.toml` | `sp1-sdk` and `sp1-build` workspace dependencies |
| `program/Cargo.toml` | `sp1-zkvm` guest dependency |
| `zk-api/build.rs` | Docker image tag for reproducible ELF builds |
| `justfile` | Docker image tag for local ELF rebuilds |
| `.github/workflows/elf.yml` | `sp1up --version` and Docker build tag in CI |
| `AGENTS.md` | Documentation references to the pinned SP1 version |

After updating those pins, `cargo update -p sp1-sdk --precise 6.1.0` was run to refresh `Cargo.lock`.

---

## 2. Cargo Dependency Graph Changes

Refreshing the lockfile pulled the SP1 crate family from `6.0.2` to `6.1.0`, including:

- `sp1-sdk`
- `sp1-build`
- `sp1-zkvm`
- `sp1-prover`
- `sp1-core-*`
- `sp1-recursion-*`
- `sp1-verifier`
- `slop-*`

It also introduced some transitive dependency churn from upstream SP1, including:

- new `sp1-core-executor-runner` and `sp1-core-executor-runner-binary` crates
- removal of `sp1-cuda` from the resolved graph
- removal of `downloader`
- new crash-reporting related dependencies such as `crash-context`, `crash-handler`, and `mach2`

These were lockfile-only changes. No project code was added to reference them directly.

---

## 3. No SDK API Changes Required In This Repo

The existing application code compiled against `sp1-sdk v6.1.0` without source changes.

That means the interfaces already adopted during the earlier v6 migration remained compatible, including:

- `Elf::Static(...)`
- `ProverClient::from_env().await`
- `ProverClient::builder().mock().build().await`
- `setup(...).await`
- `prover_client.prove(&pk, stdin).groth16().await`
- `ProvingKey::verifying_key()`

The current `sp1-sdk` feature set in `Cargo.toml` also remained valid as-is:

```toml
sp1-sdk = { version = "6.1.0", default-features = false, features = ["network", "blocking", "native-gnark"] }
```

No changes were needed in:

- `zk-api/src/proof_backends/sp1.rs`
- `cli/bin/genesis.rs`
- `cli/bin/vkey.rs`
- `program/src/main.rs`

---

## 4. Precompile Patch Configuration

The `[patch.crates-io]` overrides in the workspace `Cargo.toml` were left unchanged.

In particular, the repo continues to use the SP1 `6.0.0` patch tags for:

- `sha2-v0-9-9`
- `sha2-v0-10-9`
- `sha3-v0-10-8`
- `tiny-keccak`
- `bls12_381`

These patch tags still resolved successfully during the `6.1.0` upgrade and did not block compilation. No patch retargeting was required for this migration.

This is an observation from the successful build, not a statement that SP1 patch tags always change only on major/minor boundaries. Future upgrades should still verify patch compatibility explicitly.

---

## 5. Validation

### 5a. Lockfile refresh

The dependency graph was refreshed with:

```bash
cargo update -p sp1-sdk --precise 6.1.0
```

### 5b. Workspace compile

The workspace was validated with:

```bash
GOCACHE=/tmp/go-build-cache SP1_SKIP_PROGRAM_BUILD=true cargo check --workspace
```

This completed successfully.

### 5c. Why `GOCACHE` was overridden

The first `cargo check` attempt failed in `sp1-recursion-gnark-ffi` because its Go build step tried to write into the default macOS Go cache under `~/Library/Caches/go-build`, which is blocked by the sandboxed environment used in this session.

Redirecting `GOCACHE` into `/tmp` avoided that environment-specific failure and allowed the workspace to compile. This was not an SP1 API incompatibility or a project code issue.

### 5d. Why `SP1_SKIP_PROGRAM_BUILD=true` was used

`zk-api` has a `build.rs` that rebuilds the guest program. For a fast compatibility check, the build was run with `SP1_SKIP_PROGRAM_BUILD=true` so the validation exercised the Rust dependency and SDK surfaces without forcing an ELF rebuild during `cargo check`.

This means the compile verification confirms the codebase is compatible with `sp1-sdk v6.1.0`, but it does not by itself confirm that the checked-in ELF has been regenerated with the new toolchain.

---

## 6. Guest Program and ELF Status

The guest crate version was bumped:

```toml
sp1-zkvm = "6.1.0"
```

No guest source changes were required in `program/src/main.rs`.

However, the checked-in ELF should still be rebuilt with the `6.1.0` toolchain so the binary matches the pinned version and CI reproducibility expectations.

---

## 7. Remaining Steps

After these source changes:

1. Rebuild the ELF binary with `just update-elf` using SP1 `6.1.0`.
2. Verify the regenerated `elf/sp1-helios-elf` is committed if it changes.
3. Run any proof-generation smoke tests you want against `SP1_PROVER=mock` and, if relevant, the network prover.

Recommended local commands:

```bash
sp1up --version 6.1.0
just update-elf
GOCACHE=/tmp/go-build-cache cargo check --workspace
```
