//! Implements the `ProofBackend` trait for the SP1 ZK proof system.

use crate::{
    types::SP1HeliosProofData, // Concrete ProofOutput type
};
use alloy_primitives::{hex, Address};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use sp1_helios_primitives::types::ProofInputs;
use sp1_sdk::{
    network::FulfillmentStrategy, EnvProver, HashableKey, ProverClient, SP1ProofWithPublicValues,
    SP1ProvingKey, SP1Stdin,
};
use std::{str::FromStr, sync::Arc};
use tracing::{debug, info};

use super::ProofBackend;

const ELF: &[u8] = include_bytes!("../../../elf/sp1-helios-elf");

/// Default whitelist of reliable provers on the Succinct Prover Network.
/// These addresses are recommended by Succinct to ensure proof requests are fulfilled reliably.
/// See: https://docs.succinct.xyz/docs/sp1/prover-network/advanced-usage#whitelist
const PROVER_WHITELIST: &[&str] = &[
    "0xD4FCFCE0DEE91A9895C3AD71A6248D57C287A4F5",
    "0xB3780A2BBBC20A36C86DA1BF4FA0B0D3C4B60DCF",
    "0x22F87C35B900B117C19CC4C9CA6A9F59FB38FA4D",
    "0xE6B2B50B3EBA1EFF48B360D636D673459FF5B5E3",
    "0x546239E8539CE944120CDE00CC1F5338010E4A42",
    "0x6F7F48E0A79B607061ACB09FA2C0893973A988D5",
    "0x25204CC3D0F55AEF52936055E4E225DBD611F815",
    "0x3D008BDB990E69C40AFB6AA7161C91E82F0FA125",
    "0x4EAC32F0A25EA9D7F22D1AA40735CB761C672BD6",
    "0x5A00604BF1832E79713ABA108622C18A1F1A4349",
    "0x5380D2BD50FD183B0D5D24888E05DFD4DD7D7E4D",
    "0x05CE8CB29375858C5C9010423C43007181453766",
];

/// Parses the prover whitelist addresses into a vector of `Address`.
fn get_prover_whitelist() -> Vec<Address> {
    PROVER_WHITELIST
        .iter()
        .map(|addr| Address::from_str(addr).expect("Invalid prover whitelist address"))
        .collect()
}

/// An implementation of `ProofBackend` using the SP1 prover.
#[derive(Clone)]
pub struct SP1Backend {
    prover_client: Arc<EnvProver>,
    proving_key: Arc<SP1ProvingKey>,
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

    /// Runs the SP1 prover in a blocking thread.
    async fn run_sp1_prover(&self, stdin: SP1Stdin) -> Result<SP1ProofWithPublicValues> {
        let prover_client = self.prover_client.clone();
        let proving_key = self.proving_key.clone();
        let whitelist = get_prover_whitelist();

        debug!(target: "sp1_backend::prove", "Spawning blocking task for SP1 proof generation.");
        // Execute the potentially long-running prover logic in a blocking thread
        let result = tokio::task::spawn_blocking(move || {
            prover_client
                .prove(&proving_key, &stdin)
                .groth16()
                .strategy(FulfillmentStrategy::Auction)
                .whitelist(Some(whitelist))
                .run()
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
