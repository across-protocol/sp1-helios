//! Implements the `ProofBackend` trait for the SP1 ZK proof system.

use crate::{
    types::SP1HeliosProofData, // Concrete ProofOutput type
};
use alloy_primitives::hex;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use sp1_helios_primitives::types::ProofInputs;
use sp1_sdk::{EnvProver, ProverClient, SP1ProofWithPublicValues, SP1ProvingKey, SP1Stdin};
use std::sync::Arc;
use tracing::{debug, info, warn};

use super::ProofBackend;

const ELF: &[u8] = include_bytes!("../../../elf/sp1-helios-elf");

/// An implementation of `ProofBackend` using the SP1 prover.
#[derive(Clone)]
pub struct SP1Backend {
    prover_client: Arc<EnvProver>,
    proving_key: SP1ProvingKey,
}

impl SP1Backend {
    // todo: can improve env configurability here
    pub fn from_env() -> Result<Self> {
        info!(target: "sp1_backend::init", "Initializing SP1Backend...");

        // Initialize prover client from environment variables
        let prover_client = Arc::new(ProverClient::from_env());
        info!(target: "sp1_backend::init", "SP1 ProverClient created from environment.");

        // Setup proving and verification keys
        // Note: This can be computationally intensive
        info!(target: "sp1_backend::init", "Setting up SP1 proving and verification keys...");
        let (pk, _vk) = prover_client.setup(ELF);
        info!(target: "sp1_backend::init", "SP1 keys setup complete.");

        Ok(Self {
            prover_client,
            proving_key: pk,
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

    /// Runs the SP1 prover in a blocking thread.
    async fn run_sp1_prover(&self, stdin: SP1Stdin) -> Result<SP1ProofWithPublicValues> {
        let prover_client = self.prover_client.clone();
        let proving_key = self.proving_key.clone();

        debug!(target: "sp1_backend::prove", "Spawning blocking task for SP1 proof generation.");
        // Execute the potentially long-running prover logic in a blocking thread
        let result = tokio::task::spawn_blocking(move || {
            prover_client.prove(&proving_key, &stdin).groth16().run()
        })
        .await;

        // todo: Is this meaningful error handling? I feel like we should only have 1 error arm. Flatten errors?
        match result {
            Ok(Ok(proof)) => {
                debug!(target: "sp1_backend::prove", "Successfully generated SP1 proof.");
                Ok(proof)
            }
            Ok(Err(prover_err)) => {
                warn!(target: "sp1_backend::prove", "SP1 prover failed: {:#?}", prover_err);
                Err(anyhow!("SP1 prover failed: {:#?}", prover_err))
            }
            Err(join_err) => {
                warn!(target: "sp1_backend::prove", "Spawned prover task failed: {:#?}", join_err);
                Err(anyhow!("Spawned prover task failed: {:#?}", join_err))
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
}
