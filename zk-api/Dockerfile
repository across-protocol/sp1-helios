# IMPORTANT: This Dockerfile builds the 'zk-api' binary from the 'zk-api' package.
# It MUST be built from the WORKSPACE ROOT directory as the build context.
#
# --- Recommended Workflow ---
#
# 1. Build & Load for Local Testing (matches your current architecture, e.g., arm64 for M-series Macs):
#    docker buildx build --platform linux/arm64 --load -f zk-api/Dockerfile -t sp1-helios/zk-api:local .
#
# 2. Run Local Test:
#    docker run --rm -it -p 8080:8080 --env-file .env sp1-helios/zk-api:local
#
# 3. (If Tests Pass) Build & Push Multi-Arch for Deployment:
#    docker buildx build --platform linux/amd64,linux/arm64 --push -f zk-api/Dockerfile -t your-registry/sp1-helios/zk-api:latest .

# Stage 1: Go toolchain
FROM golang:1.24-bookworm AS go-toolchain

# Stage 2: Builder
FROM rust:1.91.0-bookworm AS builder

# Make BuildKit's platform vars visible for cache scoping
ARG TARGETPLATFORM

# Provide a modern Go toolchain required by SP1 native gnark FFI.
COPY --from=go-toolchain /usr/local/go /usr/local/go
ENV PATH="/usr/local/go/bin:${PATH}"

# Install build-time system dependencies required by protobuf codegen and bindgen.
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler libprotobuf-dev clang libclang-dev && rm -rf /var/lib/apt/lists/*

# Install required toolchain components
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
COPY zk-api ./zk-api
COPY cli ./cli
# elf is required during build, as it's included by a build-time macro include_bytes! in proof_service.rs
COPY elf ./elf

# todo: copying full program/ to keep root Cargo.toml happy. It's not in the service
# dependency graph. Maybe we can somehow avoid copying the full folder here?
COPY program ./program

# Build the specific binary in release mode
# Ensure network access if dependencies need downloading during build
RUN --mount=type=cache,id=cargo-target-${TARGETPLATFORM},target=/app/target \
    --mount=type=cache,id=cargo-git-${TARGETPLATFORM},target=/usr/local/cargo/git/db \
    --mount=type=cache,id=cargo-registry-${TARGETPLATFORM},target=/usr/local/cargo/registry \
    cargo build --release --package zk-api && \
    # Copy the built binary from the cached target dir to a regular path
    cp /app/target/release/zk-api /app/zk-api-bin

# Stage 2: Runner
FROM debian:bookworm-slim

# todo? Install any necessary runtime dependencies (e.g., OpenSSL)
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Create a non-root user for security
RUN useradd --create-home appuser
USER appuser

# Copy the compiled binary from the builder stage (from the path we copied it to)
COPY --from=builder /app/zk-api-bin /usr/local/bin/zk-api

# Set the entrypoint for the container
ENTRYPOINT ["/usr/local/bin/zk-api"]

# env vars will be passed either during running locally via --env-file .env or by cloud-run

# Default command arguments can be added here if necessary
# CMD ["--help"]
