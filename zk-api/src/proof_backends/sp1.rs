//! Implements the `ProofBackend` trait for the SP1 ZK proof system.

use crate::{
    types::SP1HeliosProofData, // Concrete ProofOutput type
};
use alloy_primitives::{hex, Address};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use sp1_helios_primitives::types::ProofInputs;
use sp1_sdk::{
    network::FulfillmentStrategy, EnvProver, HashableKey, NetworkProver, ProverClient,
    SP1ProofWithPublicValues, SP1ProvingKey, SP1Stdin,
};
use std::{env, str::FromStr, sync::Arc};
use tracing::{debug, info};

use super::ProofBackend;

const ELF: &[u8] = include_bytes!("../../../elf/sp1-helios-elf");

/// Environment variable for configuring whitelisted provers on the Succinct Prover Network.
/// Expects a comma-separated list of addresses.
/// See: https://docs.succinct.xyz/docs/sp1/prover-network/advanced-usage#whitelist
const PROVER_WHITELIST_ENV_VAR: &str = "SP1_PROVER_WHITELIST";

/// Parses the prover whitelist from the SP1_PROVER_WHITELIST environment variable.
/// Returns None if the env var is not set or empty (uses SDK default behavior).
fn get_prover_whitelist() -> Option<Vec<Address>> {
    let whitelist_str = env::var(PROVER_WHITELIST_ENV_VAR).ok()?;
    if whitelist_str.trim().is_empty() {
        return None;
    }

    let addresses: Vec<Address> = whitelist_str
        .split(',')
        .map(|addr| {
            let trimmed = addr.trim();
            Address::from_str(trimmed)
                .unwrap_or_else(|_| panic!("Invalid address in {}: {}", PROVER_WHITELIST_ENV_VAR, trimmed))
        })
        .collect();

    Some(addresses)
}

/// Returns true if SP1_PROVER is set to "network"
fn is_network_prover() -> bool {
    env::var("SP1_PROVER")
        .map(|v| v.to_lowercase() == "network")
        .unwrap_or(false)
}

/// An implementation of `ProofBackend` using the SP1 prover.
/// Supports multiple prover modes via SP1_PROVER env var (network, cpu, mock, cuda).
/// When SP1_PROVER=network, uses NetworkProver with whitelisted provers.
#[derive(Clone)]
pub struct SP1Backend {
    env_prover: Arc<EnvProver>,
    network_prover: Option<Arc<NetworkProver>>,
    proving_key: Arc<SP1ProvingKey>,
}

impl SP1Backend {
    pub fn from_env() -> Result<Self> {
        info!(target: "sp1_backend::init", "Initializing SP1Backend...");

        // Initialize prover client from environment variables
        let env_prover = Arc::new(ProverClient::from_env());
        info!(target: "sp1_backend::init", "SP1 ProverClient created from environment.");

        // Setup proving and verification keys
        info!(target: "sp1_backend::init", "Setting up SP1 proving and verification keys...");
        let (pk, _vk) = env_prover.setup(ELF);
        info!(target: "sp1_backend::init", "SP1 keys setup complete.");

        // Create NetworkProver if SP1_PROVER=network for whitelist support
        let network_prover = if is_network_prover() {
            info!(target: "sp1_backend::init", "Network prover detected, enabling whitelist support.");
            Some(Arc::new(ProverClient::builder().network().build()))
        } else {
            info!(target: "sp1_backend::init", "Using non-network prover mode.");
            None
        };

        Ok(Self {
            env_prover,
            network_prover,
            proving_key: Arc::new(pk),
        })
    }

    fn stdin_from_inputs(inputs: ProofInputs) -> Result<SP1Stdin> {
        // Serialize inputs to CBOR for SP1Stdin
        let encoded_proof_inputs =
            serde_cbor::to_vec(&inputs).context("Failed to serialize ProofInputs to CBOR")?;
        let mut stdin = SP1Stdin::new();
        stdin.write_slice(&encoded_proof_inputs);

        Ok(stdin)
    }

    /// Runs the SP1 prover. Uses NetworkProver with whitelist when SP1_PROVER=network,
    /// otherwise falls back to EnvProver for local/mock proving.
    async fn run_sp1_prover(&self, stdin: SP1Stdin) -> Result<SP1ProofWithPublicValues> {
        // Use NetworkProver with whitelist if available (SP1_PROVER=network)
        if let Some(network_prover) = &self.network_prover {
            let whitelist = get_prover_whitelist();
            if let Some(ref wl) = whitelist {
                debug!(target: "sp1_backend::prove", "Using NetworkProver with {} whitelisted provers.", wl.len());
            } else {
                debug!(target: "sp1_backend::prove", "Using NetworkProver with SDK default whitelist.");
            }

            return network_prover
                .prove(&self.proving_key, &stdin)
                .groth16()
                .strategy(FulfillmentStrategy::Auction)
                .whitelist(whitelist)
                .run_async()
                .await
                .context("SP1 network prover run failed");
        }

        // Fall back to EnvProver for non-network modes (cpu, mock, cuda)
        debug!(target: "sp1_backend::prove", "Using EnvProver (non-network mode).");
        let env_prover = self.env_prover.clone();
        let proving_key = self.proving_key.clone();

        let result: Result<Result<SP1ProofWithPublicValues, _>, _> =
            tokio::task::spawn_blocking(move || {
                env_prover.prove(&proving_key, &stdin).groth16().run()
            })
            .await;

        // todo: Is this meaningful error handling? I feel like we should only have 1 error arm. Flatten errors?
        match result {
            Ok(Ok(proof)) => {
                debug!(target: "sp1_backend::prove", "Successfully generated SP1 proof.");
                Ok(proof)
            }
            Ok(Err(prover_err)) => {
                debug!(target: "sp1_backend::prove", "SP1 prover run failed: {:#?}", prover_err);
                Err(prover_err.context("SP1 prover run failed"))
            }
            Err(join_err) => {
                debug!(target: "sp1_backend::prove", "SP1 prover thread panicked or was cancelled: {:#?}", join_err);
                Err(anyhow!(
                    "SP1 prover thread panicked or was cancelled: {}",
                    join_err
                ))
            }
        }
    }

    /// Formats the raw prover output into the target `SP1HeliosProofData`.
    fn format_output(proof_with_values: SP1ProofWithPublicValues) -> SP1HeliosProofData {
        // Extract proof bytes and public values, then hex-encode them
        let proof_hex_string = hex::encode(proof_with_values.bytes());
        let public_values_hex_string = hex::encode(proof_with_values.public_values.to_vec());

        debug!(target: "sp1_backend::format", "Formatting proof output.");
        SP1HeliosProofData {
            proof: proof_hex_string,
            public_values: public_values_hex_string,
        }
    }
}

#[async_trait]
impl ProofBackend for SP1Backend {
    type ProofOutput = SP1HeliosProofData;
    /// Asynchronously generates SP1 proof data based on the provided request details.
    ///
    /// This involves building the necessary inputs (fetching data from consensus/execution clients)
    /// and then running the SP1 prover.
    async fn generate_proof(&self, inputs: ProofInputs) -> Result<SP1HeliosProofData> {
        // 1. Build SP1Stdin
        let stdin = Self::stdin_from_inputs(inputs)?;

        // 2. Run Prover
        let proof_with_values = self
            .run_sp1_prover(stdin)
            .await
            .context("Failed to run SP1 prover")?;

        // 3. Format Output
        let output = Self::format_output(proof_with_values);

        Ok(output)
    }

    fn vkey_digest(&self) -> Vec<u8> {
        self.proving_key.vk.bytes32_raw().to_vec()
    }
}
