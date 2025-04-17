use alloy::{
    eips::BlockId,
    network::Ethereum,
    providers::{Provider as _, ProviderBuilder, RootProvider},
    rpc::types::EIP1186AccountProofResponse,
    transports::http::Http,
};
use alloy_primitives::{Address, B256};
use anyhow::anyhow;
use anyhow::{Context, Error, Result};
use log::warn;
use reqwest::{Client, Url};
use std::{env, time::Duration};

struct Provider {
    name: String,
    provider: RootProvider<Http<Client>>,
}

pub struct ProviderProxy {
    providers: Vec<Provider>,
}

const ENV_VAR_NAMES: &[&str] = &[
    "SOURCE_EXECUTION_RPC_URL",
    "SOURCE_EXECUTION_RPC_URL_BACKUP_0",
    "SOURCE_EXECUTION_RPC_URL_BACKUP_1",
];
impl ProviderProxy {
    // todo? This function uses hardcoded env var names specific to this package
    #[must_use]
    pub fn from_env() -> Self {
        dotenv::dotenv().ok();

        let mut providers = vec![];
        for name in ENV_VAR_NAMES {
            let url: Result<Url> = {
                match env::var(name).context(format!("{} not set", name)) {
                    Ok(url_string) => {
                        match url_string
                            .parse()
                            .context(format!("Failed to parse {}", name))
                        {
                            Ok(url) => Ok(url),
                            Err(err) => Err(err),
                        }
                    }
                    Err(err) => Err(err),
                }
            };

            match url {
                Ok(url) => {
                    let provider = ProviderBuilder::new().network::<Ethereum>().on_http(url);
                    providers.push(Provider {
                        name: name.to_string(),
                        provider,
                    });
                }
                Err(err) => {
                    warn!(
                        target: "ProviderProxy::from_env",
                        "Skipping url: {} . Reason: {}",
                        name, err
                    );
                }
            }
        }

        Self { providers }
    }

    /// Fetches an Ethereum storage proof (`EIP1186AccountProofResponse`) from the configured providers
    /// with retry and timeout logic.
    pub async fn get_proof(
        &self,
        address: Address,
        slots: Vec<B256>,
        block_id: Option<BlockId>,
        retries: Option<usize>,
        timeout_duration: Option<Duration>,
    ) -> Result<EIP1186AccountProofResponse> {
        let max_retries = retries.unwrap_or(1);
        let request_timeout = timeout_duration.unwrap_or(Duration::from_secs(10));
        let retry_delay = Duration::from_secs(1); // Fixed 1-second delay

        if self.providers.is_empty() {
            return Err(anyhow!("No execution providers configured."));
        }

        let mut last_errors: Vec<(String, Error)> = vec![];

        for attempt in 1..=max_retries {
            match self
                .get_proof_try_once(address, slots.clone(), block_id, Some(request_timeout))
                .await
            {
                Ok(proof) => return Ok(proof),
                Err(e) => {
                    warn!(
                        target: "ProviderProxy::get_proof",
                        "Attempt {}/{} failed: {}",
                        attempt, max_retries, e
                    );
                    last_errors.push(("get_proof_try_once".to_string(), e));

                    // Don't sleep after the last attempt
                    if attempt < max_retries {
                        tokio::time::sleep(retry_delay).await;
                    }
                }
            }
        }

        Err(anyhow!(
            "All {} attempts to get proof failed. Last errors: {:?}",
            max_retries,
            last_errors
        ))
    }

    async fn get_proof_try_once(
        &self,
        address: Address,
        slots: Vec<B256>,
        block_id: Option<BlockId>,
        timeout_duration: Option<Duration>,
    ) -> Result<EIP1186AccountProofResponse> {
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<Result<EIP1186AccountProofResponse>>(self.providers.len());
        for provider in &self.providers {
            let root_provider = provider.provider.clone();
            let keys = slots.clone();
            let tx = tx.clone();

            tokio::spawn(async move {
                let result = match block_id {
                    Some(block_id) => root_provider.get_proof(address, keys).block_id(block_id),
                    None => root_provider.get_proof(address, keys),
                }
                .await
                .map_err(|e| anyhow!("RPC error {}", e));
                // todo: this should never error. We ignore this for now
                let _ = tx.try_send(result);
            });
        }

        // Drop the original sender to avoid keeping the channel open
        drop(tx);

        let timeout = timeout_duration.unwrap_or(Duration::from_secs(10));
        let mut errors = Vec::new();

        // Use tokio::time::timeout to enforce a timeout on the entire operation
        match tokio::time::timeout(timeout, async {
            // Process responses as they arrive
            while let Some(result) = rx.recv().await {
                match result {
                    Ok(proof) => return Ok(proof),
                    Err(e) => errors.push(e),
                }
            }

            // If we get here, all senders have dropped without sending a successful response
            Err(anyhow!(
                "All providers failed to return a valid proof: {}",
                errors
                    .iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow!(
                "Proof request timed out after {:?}. Accumulated errors: {}",
                timeout,
                errors
                    .iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}
