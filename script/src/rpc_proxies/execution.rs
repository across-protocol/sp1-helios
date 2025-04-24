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
use log::warn;
use reqwest::{Client, Url};
use std::{env, time::Duration};
use tokio::time::timeout;

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
                                "Skipping invalid URL '{}': {}", url_str, e
                            );
                        }
                    }
                }
            }
            Err(e) => {
                warn!(target: "Proxy::from_env", "Failed to read execution URLs: {}", e);
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
        keys: Vec<B256>,
        // todo? We don't have to require block_id like that. It's easiest for now. Option<BlockId>
        block_id: BlockId,
    ) -> Result<EIP1186AccountProofResponse> {
        let proof = multiplex(
            |client| {
                let keys = keys.clone();
                (async move { Self::get_proof_and_check(client, address, keys, block_id).await })
                    .boxed()
            },
            &self.providers,
        )
        .await?;
        Ok(proof)
    }

    /// Requests Merkle proof from execution client. Times out if it rpc call takes longer than 5 seconds
    // todo: add retries to this via retri
    async fn get_proof_and_check(
        client: RootProvider<Http<Client>>,
        address: Address,
        keys: Vec<B256>,
        block_id: BlockId,
    ) -> Result<EIP1186AccountProofResponse> {
        let proof = timeout(
            Duration::from_secs(4),
            client.get_proof(address, keys).block_id(block_id),
        )
        .await;

        match proof {
            /*
            todo: check Merkle proof locally here. If errors, return error; if suceeds, return Ok(proof).
            Practically, we're using only trusworthy execution RPCs, so this call may not be as important.
             */
            Ok(Ok(proof)) => Ok(proof),
            Ok(Err(e)) => Err(anyhow!("rpc error: {e}")),
            Err(e) => Err(anyhow!("rpc call timed out after {e}")),
        }
    }
}
