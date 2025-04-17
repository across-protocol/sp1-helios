//! Defines the core trait for asynchronous proof generation backends.

use crate::api::ProofRequest;
use anyhow::Result;
use async_trait::async_trait;
use serde::{de::DeserializeOwned, Serialize};

pub mod sp1;

/// Defines the trait for asynchronous proof generation backends (e.g., SP1, R0VM).
///
/// Implementors take a [`ProofRequest`] and produce a serializable `ProofOutput`
/// containing the generated proof data.
#[async_trait]
pub trait ProofBackend {
    type ProofOutput: Clone + Serialize + DeserializeOwned + Send + Sync + 'static;

    /// Asynchronously generates proof data for the given request.
    ///
    /// # Arguments
    ///
    /// * `request`: Contains the parameters for the proof generation.
    ///
    /// # Returns
    ///
    /// The generated `ProofOutput` on success, or an error on failure.
    // todo: this might be giving ProofBackend more responsibility than needed. E.g. we shouldn't be
    // todo: making RPC calls to consensus / execution layer from the ProofBackend. It should happen
    // todo: on the ProofService level. This fn could instead be smth like:
    // todo: `async fn generate_proof(&self, inputs: ProofInputs) -> Result<Self::ProofOutput>`
    // todo: Oh well.
    async fn generate_proof(&self, request: ProofRequest) -> Result<Self::ProofOutput>;
}
