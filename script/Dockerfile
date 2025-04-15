# IMPORTANT: This Dockerfile builds the 'operator2' binary from the 'script' package.
# It MUST be built from the WORKSPACE ROOT directory as the build context.
#
# --- Recommended Workflow ---
#
# 1. Build & Load for Local Testing (matches your current architecture, e.g., arm64 for M-series Macs):
#    docker buildx build --platform linux/arm64 --load -f script/Dockerfile -t sp1-helios/operator2:local .
#
# 2. Run Local Test:
#    docker run --rm -it -p 8080:8080 --env-file script/.env sp1-helios/operator2:local
#
# 3. (If Tests Pass) Build & Push Multi-Arch for Deployment:
#    docker buildx build --platform linux/amd64,linux/arm64 --push -f script/Dockerfile -t your-registry/sp1-helios/operator2:latest .

# Use the Rust version specified in script/rust-toolchain
# Stage 1: Builder
FROM rust:1.82.0 AS builder

# Install required toolchain components specified in script/rust-toolchain
# This is done early to leverage Docker layer caching
RUN rustup component add llvm-tools rustc-dev

# Skip rebuilding ELF from program/ assuming that elf is pre-built already. It will be copied during
# build stage. This creates a strong connection between elf stored in git and used in the docker image. 
# We don't rebuild elf on the fly. We should enforce ELF integrity in CI instead.
ENV SP1_SKIP_PROGRAM_BUILD=true

WORKDIR /app

# Copy workspace manifests and lock file first to leverage Docker cache for dependencies
COPY Cargo.toml Cargo.lock ./
# Copy required workspace members' source code
COPY primitives ./primitives
COPY script ./script
# elf is required during build, as it's included by a build-time macro include_bytes! in proof_service.rs
COPY elf ./elf

# todo: copying full program/ and test/ to keep root Cargo.toml happy. They're not in the operator2
# dependency graph. Maybe we can somehow avoid copying full folders here?
COPY program ./program
COPY test ./test

# Build the specific binary in release mode
# Ensure network access if dependencies need downloading during build
RUN --mount=type=cache,target=/app/target \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release --package sp1-helios-script --bin operator2 && \
    # Copy the built binary from the cached target dir to a regular path
    cp /app/target/release/operator2 /app/operator2-bin

# Stage 2: Runner
FROM debian:bookworm-slim

# todo? Install any necessary runtime dependencies (e.g., OpenSSL)
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Create a non-root user for security
RUN useradd --create-home appuser
USER appuser

# Copy the compiled binary from the builder stage (from the path we copied it to)
COPY --from=builder /app/operator2-bin /usr/local/bin/operator2

# Set the entrypoint for the container
ENTRYPOINT ["/usr/local/bin/operator2"]

# env vars will be passed either during running locally via --env-file .env or by cloud-run

# Default command arguments can be added here if necessary
# CMD ["--help"] 