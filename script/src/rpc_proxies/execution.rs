use alloy::{
    eips::BlockId,
    network::Ethereum,
    providers::{Provider as _, ProviderBuilder, RootProvider},
    rpc::types::EIP1186AccountProofResponse,
    transports::http::Http,
};
use alloy_primitives::{Address, B256};
use anyhow::anyhow;
use anyhow::{Context, Result};
use futures::future::FutureExt;
use reqwest::{Client, Url};
use std::{env, time::Duration};
use tokio::time::timeout;
use tracing::warn;

use crate::{types::ContractStorageBuilder, verify_storage_slot_proofs};

use super::multiplex;

#[derive(Clone)]
pub struct Proxy {
    providers: Vec<RootProvider<Http<Client>>>,
}

impl Proxy {
    pub fn from_env() -> Self {
        Self::try_from_env().unwrap()
    }

    pub fn try_from_env() -> Result<Self> {
        let mut providers = vec![];
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
                            let provider =
                                ProviderBuilder::new().network::<Ethereum>().on_http(url);
                            providers.push(provider);
                        }
                        Err(e) => {
                            warn!(
                                target: "Proxy::from_env",
                                "Skipping invalid URL '{}': {:#?}", url_str, e
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

    /// Requests Merkle proof from execution client. Times out if it rpc call takes longer than 4 seconds
    // todo? consider adding retries with exp. backoff.
    async fn get_proof_and_check(
        client: RootProvider<Http<Client>>,
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
