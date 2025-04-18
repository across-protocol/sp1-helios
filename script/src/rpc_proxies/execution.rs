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
use log::warn;
use reqwest::{Client, Url};
use std::{env, future::Future, time::Duration};

#[derive(Clone)]
struct Provider {
    name: String,
    provider: RootProvider<Http<Client>>,
}

#[derive(Clone)]
pub struct ExecutionRpcProxy {
    providers: Vec<Provider>,
}

const ENV_VAR_NAMES: &[&str] = &[
    "SOURCE_EXECUTION_RPC_URL",
    "SOURCE_EXECUTION_RPC_URL_BACKUP_0",
    "SOURCE_EXECUTION_RPC_URL_BACKUP_1",
];

// Public interface
impl ExecutionRpcProxy {
    #[must_use]
    pub fn from_env() -> Self {
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
    /// with retry and timeout logic. I don't think RootProvider implements any retries / timeout handling by default, so we have to impl ourselves
    // todo: consider using retri for retrying with exponential backoff
    pub async fn get_proof(
        &self,
        address: Address,
        slots: Vec<B256>,
        block_id: Option<BlockId>,
        retries: Option<usize>,
        timeout_duration: Option<Duration>,
    ) -> Result<EIP1186AccountProofResponse> {
        let operation_name = "get_proof";
        let retry_delay = Duration::from_secs(1);

        let request_closure = move |provider: RootProvider<Http<Client>>| {
            let keys = slots.clone();
            async move {
                let result = match block_id {
                    Some(id) => provider.get_proof(address, keys).block_id(id).await,
                    None => provider.get_proof(address, keys).await,
                };
                // todo: here, check the Merkle proof and return an error if it's incorrect
                result.map_err(|e| anyhow!("RPC error from provider: {}", e))
            }
        };

        self._proxy_request_with_retries(
            operation_name,
            retries,
            timeout_duration,
            retry_delay,
            request_closure,
        )
        .await
    }
}

// todo? For now, returning the result that's arrived first. Could change this to a quorum-based solution
impl ExecutionRpcProxy {
    /// Generic helper to perform a request against all providers concurrently and return the first
    /// successful response within a timeout.
    async fn _proxy_request_try_once<R, F, Fut>(
        &self,
        timeout_duration: Duration,
        f: F,
    ) -> Result<R>
    where
        R: Send + 'static,
        F: Fn(RootProvider<Http<Client>>) -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Result<R>> + Send + 'static,
    {
        if self.providers.is_empty() {
            return Err(anyhow!("No execution providers configured."));
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<R>>(self.providers.len());

        for provider in &self.providers {
            let provider_clone = provider.provider.clone();
            let f_clone = f.clone();
            let tx_clone = tx.clone();
            let provider_name = provider.name.clone();

            tokio::spawn(async move {
                let result = f_clone(provider_clone).await;
                if let Err(e) = &result {
                    warn!(target: "ProviderProxy::_proxy_request_try_once", "Provider '{}' failed: {}", provider_name, e);
                }
                // todo: this should never error. Should I unwrap?
                let _ = tx_clone.try_send(result);
            });
        }

        // Drop last active `tx`
        drop(tx);

        match tokio::time::timeout(timeout_duration, async {
            while let Some(result) = rx.recv().await {
                match result {
                    Ok(value) => return Ok(value), // Return the first successful result
                    Err(_) => { /* Error already logged in the spawn, continue waiting */ }
                }
            }
            // All providers failed without success
            Err(anyhow!(
                "All providers failed to return a successful response."
            ))
        })
        .await
        {
            Ok(Ok(result)) => Ok(result), // Inner op succeeded
            Ok(Err(e)) => Err(e),         // Inner op failed (all providers failed)
            Err(_) => {
                // Timeout occurred
                Err(anyhow!("Request timed out after {:?}", timeout_duration))
            }
        }
    }

    /// Generic helper to perform a request with retries.
    async fn _proxy_request_with_retries<R, F, Fut>(
        &self,
        operation_name: &str, // For logging purposes
        retries: Option<usize>,
        timeout_duration: Option<Duration>,
        retry_delay: Duration,
        f: F,
    ) -> Result<R>
    where
        R: Send + 'static,
        F: Fn(RootProvider<Http<Client>>) -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Result<R>> + Send + 'static,
    {
        let max_retries = retries.unwrap_or(1).max(1);
        let request_timeout = timeout_duration.unwrap_or(Duration::from_secs(10));

        for attempt in 1..=max_retries {
            let f_clone = f.clone();
            match self._proxy_request_try_once(request_timeout, f_clone).await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    // Log the error for this attempt
                    warn!(
                        target: "ProviderProxy::_proxy_request_with_retries",
                        "Operation '{}' attempt {}/{} failed: {}",
                        operation_name, attempt, max_retries, e
                    );

                    if attempt < max_retries {
                        tokio::time::sleep(retry_delay).await;
                    }
                }
            }
        }

        // All attempts failed
        Err(anyhow!(
            "Operation '{}' failed after {} attempts.",
            operation_name,
            max_retries
        ))
    }
}
