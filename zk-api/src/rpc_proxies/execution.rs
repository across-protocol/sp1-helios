use alloy::{
    eips::BlockId,
    network::Ethereum,
    providers::{Provider as _, ProviderBuilder, RootProvider},
    rpc::types::EIP1186AccountProofResponse,
};
use alloy_primitives::{keccak256, Address, Bytes, B256};
use anyhow::anyhow;
use anyhow::{Context, Result};
use futures::future::FutureExt;
use reqwest::Url;
use sp1_helios_primitives::execution::{decode_execution_header_fields, ExecutionHeaderFields};
use std::{env, time::Duration};
use tokio::time::timeout;
use tracing::warn;

use crate::{types::ContractStorageBuilder, verify_storage_slot_proofs};

use super::multiplex;

/// An execution block header fetched by hash, re-encoded to RLP and verified to be the
/// keccak256 preimage of the requested hash.
#[derive(Debug, Clone)]
pub struct VerifiedExecutionHeader {
    /// RLP encoding of the header; `keccak256(rlp) == block hash` is guaranteed.
    pub rlp: Bytes,
    /// `state_root` and `block_number` decoded from the RLP.
    pub fields: ExecutionHeaderFields,
}

#[derive(Clone)]
pub struct Proxy {
    providers: Vec<RootProvider<Ethereum>>,
}

impl Proxy {
    pub fn from_env() -> Self {
        Self::try_from_env().unwrap()
    }

    pub fn try_from_env() -> Result<Self> {
        let mut providers: Vec<RootProvider<Ethereum>> = vec![];
        let urls_env =
            env::var("SOURCE_EXECUTION_RPC_URL").context("SOURCE_EXECUTION_RPC_URL not set");

        match urls_env {
            Ok(urls_str) => {
                for url_str in urls_str
                    .split(',')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                {
                    match url_str.parse::<Url>() {
                        Ok(url) => {
                            let provider = ProviderBuilder::new()
                                .network::<Ethereum>()
                                .connect_http(url);
                            providers.push(provider.root().clone());
                        }
                        Err(e) => {
                            warn!(
                                target: "Proxy::from_env",
                                "Skipping invalid URL '{}': {:#?}",
                                url_str, e
                            );
                        }
                    }
                }
            }
            Err(e) => {
                warn!(target: "Proxy::from_env", "Failed to read execution URLs: {:#?}", e);
            }
        }

        if providers.is_empty() {
            Err(anyhow!(
                "No valid execution RPC URLs found in SOURCE_EXECUTION_RPC_URL."
            ))
        } else {
            Ok(Self { providers })
        }
    }

    /// Requests Merkle proof from execution client. Multiplexes rpc calls across multiple available RPC providers.
    /// Handles RPC timeouts, retries and result consistency checking
    pub async fn get_proof(
        &self,
        address: Address,
        storage_slot_keys: &[B256],
        block_id: BlockId,
        state_root: B256,
    ) -> Result<EIP1186AccountProofResponse> {
        let proof = multiplex(
            |client| {
                let keys = storage_slot_keys.to_owned();
                (async move {
                    Self::get_proof_and_check(client, address, keys, block_id, state_root).await
                })
                .boxed()
            },
            &self.providers,
        )
        .await?;
        Ok(proof)
    }

    /// Fetches the execution block header for `block_hash`, re-encodes it to RLP and verifies
    /// the encoding is the keccak256 preimage of `block_hash` before returning it. Multiplexes
    /// across the available RPC providers.
    ///
    /// The preimage check both rejects lying RPCs and fails loudly if the local alloy version
    /// cannot round-trip a new fork's header layout (e.g. a header field added by a hard fork
    /// that this build's alloy does not know yet).
    pub async fn get_execution_header(&self, block_hash: B256) -> Result<VerifiedExecutionHeader> {
        multiplex(
            |client| {
                (async move { Self::get_execution_header_and_check(client, block_hash).await })
                    .boxed()
            },
            &self.providers,
        )
        .await
    }

    async fn get_execution_header_and_check(
        client: RootProvider<Ethereum>,
        block_hash: B256,
    ) -> Result<VerifiedExecutionHeader> {
        let block = timeout(Duration::from_secs(4), client.get_block_by_hash(block_hash))
            .await
            .map_err(|e| anyhow!("get_block_by_hash timed out after {:#?}", e))?
            .map_err(|e| anyhow!("rpc error: {:#?}", e))?
            .ok_or_else(|| anyhow!("block {} not found", block_hash))?;

        let rlp = alloy_rlp::encode(&block.header.inner);
        if keccak256(&rlp) != block_hash {
            return Err(anyhow!(
                "re-encoded execution header does not hash to {} — lying RPC, or this alloy \
                version cannot round-trip the current fork's header layout",
                block_hash
            ));
        }

        let fields = decode_execution_header_fields(&rlp)
            .map_err(|e| anyhow!("failed to decode execution header fields: {e}"))?;

        Ok(VerifiedExecutionHeader {
            rlp: rlp.into(),
            fields,
        })
    }

    /// Requests Merkle proof from execution client. Times out if it rpc call takes longer than 4 seconds
    // todo? consider adding retries with exp. backoff.
    async fn get_proof_and_check(
        client: RootProvider<Ethereum>,
        address: Address,
        keys: Vec<B256>,
        block_id: BlockId,
        state_root: B256,
    ) -> Result<EIP1186AccountProofResponse> {
        let proof = timeout(
            Duration::from_secs(4),
            client.get_proof(address, keys.clone()).block_id(block_id),
        )
        .await;

        match proof {
            Ok(Ok(proof)) => {
                // verify Merkle proof response from this RPC before returning Ok()
                let contract_storage = ContractStorageBuilder::build(&keys, proof.clone())?;
                _ = verify_storage_slot_proofs(state_root, contract_storage)?;
                Ok(proof)
            }
            Ok(Err(e)) => Err(anyhow!("rpc error: {:#?}", e)),
            Err(e) => Err(anyhow!("rpc call timed out after {:#?}", e)),
        }
    }
}
