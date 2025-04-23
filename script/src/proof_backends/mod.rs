//! Defines the core trait for asynchronous proof generation backends.

use anyhow::Result;
use async_trait::async_trait;
use serde::{de::DeserializeOwned, Serialize};
use sp1_helios_primitives::types::ProofInputs;

pub mod sp1;

/// Defines the trait for asynchronous proof generation backends (e.g., SP1, R0VM).
///
/// Implementors take a [`ProofRequest`] and produce a serializable `ProofOutput`
/// containing the generated proof data.
#[async_trait]
pub trait ProofBackend {
    type ProofOutput: Clone + Serialize + DeserializeOwned + Send + Sync + 'static;

    /*
    todo: for now, ProofInputs is from `use sp1_helios_primitives::types::ProofInputs;`, which seems
    tied to sp1, but in reality we want the same inputs for every backend, just moved from the sp1
    primitives crate
     */
    async fn generate_proof(&self, inputs: ProofInputs) -> Result<Self::ProofOutput>;
}
