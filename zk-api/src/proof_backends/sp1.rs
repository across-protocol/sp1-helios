//! Implements the `ProofBackend` trait for the SP1 ZK proof system.

use crate::{
    types::SP1HeliosProofData, // Concrete ProofOutput type
};
use alloy_primitives::hex;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use sp1_helios_primitives::types::ProofInputs;
use sp1_sdk::{
    env::{EnvProver, EnvProvingKey},
    Elf, HashableKey, ProveRequest, Prover, ProverClient, ProvingKey, SP1ProofWithPublicValues,
    SP1Stdin,
};
use std::sync::Arc;
use tracing::{debug, info};

use super::ProofBackend;

const ELF: Elf = Elf::Static(include_bytes!("../../../elf/sp1-helios-elf"));
const NETWORK_GAS_LIMIT_MULTIPLIER_BPS: u64 = 12_500;

/// An implementation of `ProofBackend` using the SP1 prover.
#[derive(Clone)]
pub struct SP1Backend {
    prover_client: Arc<EnvProver>,
    proving_key: Arc<<EnvProver as Prover>::ProvingKey>,
}

impl SP1Backend {
    // todo: can improve env configurability here
    pub async fn from_env() -> Result<Self> {
        info!(target: "sp1_backend::init", "Initializing SP1Backend...");

        // Initialize prover client from environment variables
        let prover_client = Arc::new(ProverClient::from_env().await);
        info!(target: "sp1_backend::init", "SP1 ProverClient created from environment.");

        // Setup proving and verification keys
        // Note: This can be computationally intensive
        info!(target: "sp1_backend::init", "Setting up SP1 proving and verification keys...");
        let pk = prover_client
            .setup(ELF)
            .await
            .context("Failed to setup SP1 proving key")?;
        info!(target: "sp1_backend::init", "SP1 keys setup complete.");

        Ok(Self {
            prover_client,
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

    /// Runs the SP1 prover asynchronously.
    async fn run_sp1_prover(&self, stdin: SP1Stdin) -> Result<SP1ProofWithPublicValues> {
        debug!(target: "sp1_backend::prove", "Starting SP1 proof generation.");

        let proof = match (&*self.prover_client, &*self.proving_key) {
            (EnvProver::Network(prover), EnvProvingKey::Network { pk, .. }) => {
                let (_, report) = prover
                    .execute(pk.elf().clone(), stdin.clone())
                    .calculate_gas(true)
                    .await
                    .map_err(|e| anyhow!("SP1 gas estimation failed before network submit: {e}"))?;

                let estimated_gas = report.gas().ok_or_else(|| {
                    anyhow!("SP1 execution report did not include gas usage during estimation")
                })?;
                let adjusted_gas = estimated_gas
                    .saturating_mul(NETWORK_GAS_LIMIT_MULTIPLIER_BPS)
                    .div_ceil(10_000);

                info!(
                    target: "sp1_backend::prove",
                    estimated_gas,
                    adjusted_gas,
                    "Submitting network proof with temporary gas-limit mitigation"
                );

                prover
                    .prove(pk, stdin)
                    .gas_limit(adjusted_gas)
                    .groth16()
                    .await
                    .map_err(|e| anyhow!("SP1 prover run failed: {e}"))?
            }
            _ => self
                .prover_client
                .prove(&self.proving_key, stdin)
                .groth16()
                .await
                .map_err(|e| anyhow!("SP1 prover run failed: {e}"))?,
        };

        debug!(target: "sp1_backend::prove", "Successfully generated SP1 proof.");
        Ok(proof)
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
        self.proving_key.verifying_key().bytes32_raw().to_vec()
    }
}
