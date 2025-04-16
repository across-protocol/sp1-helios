//! Defines the core trait for asynchronous proof generation backends.

use crate::api::ProofRequest;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod sp1;

/// Defines the trait for asynchronous proof generation backends (e.g., SP1, R0VM).
///
/// Implementors take a [`ProofRequest`] and produce a serializable `ProofOutput`
/// containing the generated proof data.
#[async_trait]
pub trait ProofBackend<ProofOutput>
where
    // `ProofOutput` bounds:
    // - `Serialize + Deserialize<'static> + 'static`: For data ownership and storage (e.g., Redis).
    // - `Send + Sync`: Required for async/thread safety.
    ProofOutput: Serialize + Deserialize<'static> + Send + Sync + 'static,
{
    /// Asynchronously generates proof data for the given request.
    ///
    /// # Arguments
    ///
    /// * `request`: Contains the parameters for the proof generation.
    ///
    /// # Returns
    ///
    /// The generated `ProofOutput` on success, or an error on failure.
    async fn generate_proof(&self, request: ProofRequest) -> Result<ProofOutput>;
}
