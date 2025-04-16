//! Implements the `ProofBackend` trait for the SP1 ZK proof system.

use crate::{
    api::ProofRequest,
    try_get_checkpoint,
    try_get_client,
    try_get_updates,
    types::SP1HeliosProofData, // Concrete ProofOutput type
};
use alloy::{
    providers::{Provider, RootProvider},
    transports::http::Http,
};
use alloy_primitives::hex;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use helios_consensus_core::{consensus_spec::MainnetConsensusSpec, types::FinalityUpdate};
use helios_ethereum::rpc::ConsensusRpc;
use log::{debug, error, info, warn};
use reqwest::Client;
use sp1_helios_primitives::types::{ContractStorage, ProofInputs, StorageSlot};
use sp1_sdk::{EnvProver, ProverClient, SP1ProofWithPublicValues, SP1ProvingKey, SP1Stdin};
use std::{sync::Arc, time::Duration};
use tokio::time::sleep;

use super::ProofBackend;

const ELF: &[u8] = include_bytes!("../../../elf/sp1-helios-elf");

#[derive(Clone, Debug)]
pub struct SP1BackendConfig {
    pub max_input_setup_retries: usize,
    pub retry_delay: Duration,
}

impl Default for SP1BackendConfig {
    fn default() -> Self {
        Self {
            max_input_setup_retries: 3,
            retry_delay: Duration::from_secs(12), // Default to one slot time
        }
    }
}

/// An implementation of `ProofBackend` using the SP1 prover.
#[derive(Clone)]
pub struct SP1Backend {
    source_chain_provider: RootProvider<Http<Client>>,
    prover_client: Arc<EnvProver>,
    proving_key: SP1ProvingKey,
    config: SP1BackendConfig,
}

impl SP1Backend {
    /// Creates a new SP1Backend instance, initializing prover client and keys from environment.
    pub fn new(
        source_chain_provider: RootProvider<Http<Client>>,
        config: Option<SP1BackendConfig>,
    ) -> Result<Self> {
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
            source_chain_provider,
            prover_client,
            proving_key: pk,
            config: config.unwrap_or_default(),
        })
    }

    /// Builds the `SP1Stdin` required for proof generation, handling retries.
    /// This logic is extracted from the original `execute_proof_generation`.
    async fn build_sp1_stdin(&self, request: &ProofRequest) -> Result<SP1Stdin> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            debug!(target: "sp1_backend::input", "Attempt {}/{} to build SP1 stdin for request.", attempt, self.config.max_input_setup_retries);

            match self.try_build_sp1_stdin_once(request).await {
                Ok(stdin) => {
                    debug!(target: "sp1_backend::input", "Successfully built SP1 stdin.");
                    return Ok(stdin);
                }
                Err(e) => {
                    warn!(
                        target: "sp1_backend::input",
                        "Failed to build SP1 stdin (attempt {}/{}): {}",
                        attempt, self.config.max_input_setup_retries, e
                    );
                    if attempt >= self.config.max_input_setup_retries {
                        error!(target: "sp1_backend::input", "All {} attempts to build SP1 stdin failed.", self.config.max_input_setup_retries);
                        return Err(anyhow!("Final attempt to build SP1 stdin failed: {}", e));
                    }
                    // Sleep before retrying
                    sleep(self.config.retry_delay).await;
                }
            }
        }
    }

    /// Performs a single attempt to build the `SP1Stdin`.
    async fn try_build_sp1_stdin_once(&self, request: &ProofRequest) -> Result<SP1Stdin> {
        // Fetch the checkpoint at the requested slot
        let checkpoint = try_get_checkpoint(request.stored_contract_head)
            .await
            .context("Failed to get checkpoint")?;

        // Get the client from the checkpoint
        let client = try_get_client(checkpoint)
            .await
            .context("Failed to get light client from checkpoint")?;

        // Fetch necessary consensus data
        let sync_committee_updates = try_get_updates(&client)
            .await
            .context("Failed to get sync committee updates")?;
        let finality_update: FinalityUpdate<MainnetConsensusSpec> = client
            .rpc
            .get_finality_update()
            .await
            .map_err(|e| anyhow!("Failed to get finality update from consensus RPC: {}", e))?;

        let latest_finalized_header = finality_update.finalized_header();
        let expected_current_slot = client.expected_current_slot();

        // Fetch execution layer storage proof
        let latest_finalized_execution_header = latest_finalized_header
            .execution()
            .map_err(|_| anyhow!("No execution header in finality update"))?;

        let block_id = (*latest_finalized_execution_header.block_number()).into();
        debug!(target: "sp1_backend::input", "Fetching storage proof for address {} slot {} at block ID {:?}", request.hub_pool_address, request.storage_slot, block_id);

        let proof = self
            .source_chain_provider
            .get_proof(request.hub_pool_address, vec![request.storage_slot])
            .block_id(block_id)
            .await
            .context("Failed to get storage proof from execution provider")?;

        // Assemble the ProofInputs struct
        let storage_slot = StorageSlot {
            key: request.storage_slot,
            expected_value: proof
                .storage_proof
                .first()
                .context("Storage proof vector was empty")?
                .value,
            mpt_proof: proof
                .storage_proof
                .first()
                .context("Storage proof vector was empty")?
                .proof
                .clone(),
        };

        let inputs = ProofInputs {
            sync_committee_updates,
            finality_update,
            expected_current_slot,
            store: client.store.clone(),
            genesis_root: client.config.chain.genesis_root,
            forks: client.config.forks.clone(),
            contract_storage_slots: ContractStorage {
                address: proof.address,
                expected_value: alloy_trie::TrieAccount {
                    nonce: proof.nonce,
                    balance: proof.balance,
                    storage_root: proof.storage_hash,
                    code_hash: proof.code_hash,
                },
                mpt_proof: proof.account_proof,
                storage_slots: vec![storage_slot],
            },
        };

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
        let proving_key = self.proving_key.clone(); // SP1ProvingKey likely needs Clone

        debug!(target: "sp1_backend::prove", "Spawning blocking task for SP1 proof generation.");
        // Execute the potentially long-running prover logic in a blocking thread
        let result = tokio::task::spawn_blocking(move || {
            prover_client.prove(&proving_key, &stdin).groth16().run()
        })
        .await;

        match result {
            Ok(Ok(proof)) => {
                debug!(target: "sp1_backend::prove", "Successfully generated SP1 proof.");
                Ok(proof)
            }
            Ok(Err(prover_err)) => {
                warn!(target: "sp1_backend::prove", "SP1 prover failed: {}", prover_err);
                Err(anyhow!("SP1 prover failed: {}", prover_err))
            }
            Err(join_err) => {
                warn!(target: "sp1_backend::prove", "Spawned prover task failed: {}", join_err);
                Err(anyhow!("Spawned prover task failed: {}", join_err))
            }
        }
    }

    /// Formats the raw prover output into the target `SP1HeliosProofData`.
    fn format_output(&self, proof_with_values: SP1ProofWithPublicValues) -> SP1HeliosProofData {
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
    async fn generate_proof(&self, request: ProofRequest) -> Result<SP1HeliosProofData> {
        // 1. Build Inputs (with retries)
        let stdin = self
            .build_sp1_stdin(&request) // Pass request by reference here
            .await
            .context("Failed to build SP1 stdin")?;

        // 2. Run Prover
        let proof_with_values = self
            .run_sp1_prover(stdin)
            .await
            .context("Failed to run SP1 prover")?;

        // 3. Format Output
        let output = self.format_output(proof_with_values);

        Ok(output)
    }
}
