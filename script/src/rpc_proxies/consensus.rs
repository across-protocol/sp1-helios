use alloy::primitives::B256;
use async_trait::async_trait;
use eyre::{eyre, Result};
use helios_consensus_core::{
    consensus_spec::ConsensusSpec,
    types::{BeaconBlock, Bootstrap, FinalityUpdate, OptimisticUpdate, Update},
};
use helios_ethereum::rpc::{http_rpc::HttpRpc, ConsensusRpc};
use std::{env, time::Duration};
use tracing::{info, warn};
use tree_hash::TreeHash;

/// Wraps `helios_ethereum::rpc::http_rpc::HttpRpc`. Provides extra functionality:
///   - multiplexing to multiple configured clients
///   - request timeouts
///   - todo: *some* output data integrity checks
///
/// Multiplexing strategy: query the main RPC first, then if that fails, query the others.
/// This is because we only have one production RPC, and other ones are public, potentially less
/// reliable both in terms of data integrity and uptime
// todo: the main thing to add is estimates on current committee period and finalized slot
pub struct ConsensusRpcProxy {
    rpcs: Vec<HttpRpc>,
}

/// The default strategy for these client-facing calls should be: first request form the most trusted RPC. If that fails, try to use fallbacks.
/// In either case, client will not accept invalid response, so we're good to use public rpcs as backups.
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl<S: ConsensusSpec> ConsensusRpc<S> for ConsensusRpcProxy {
    /// Creates a new instance using RPC endpoints from an environment variable.
    ///
    /// # Parameters
    ///
    /// * `env_var_name` - Name of the environment variable containing a comma-separated
    ///   list of RPC endpoint URLs. If the variable is not set or empty, this will fail.
    ///
    /// # Example
    ///
    /// ```
    /// // Set env var with multiple RPC endpoints
    /// std::env::set_var("CONSENSUS_RPCS", "https://primary-rpc.com,https://backup-rpc.com");
    ///
    /// // Create proxy using that environment variable
    /// let rpc = ConsensusRpcProxy::<MainnetSpec>::new("CONSENSUS_RPCS");
    /// ```
    fn new(env_var_name: &str) -> Self {
        let urls_str = env::var(env_var_name)
            .map_err(|_| eyre!("Environment variable {} not found", env_var_name))
            .expect("ConsensusRpcProxy: No RPC URLs found in environment variable");

        let urls: Vec<&str> = urls_str
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        assert!(
            !urls.is_empty(),
            "ConsensusRpcProxy: No RPC URLs found in environment variable"
        );

        info!(
            "Creating ConsensusRPCProxy with {} RPC endpoints",
            urls.len()
        );

        info!("Primary endpoint is {}", urls.first().unwrap());

        let rpcs: Vec<HttpRpc> = urls
            .iter()
            .map(|url| <HttpRpc as ConsensusRpc<S>>::new(url))
            .collect();

        Self { rpcs }
    }

    /// Tries to fetch bootstrap from the first available RPC. If that errors, fetches from all the backups and returns a valid one or errors
    async fn get_bootstrap(&self, checkpoint: B256) -> Result<Bootstrap<S>> {
        // 1) Try the primary RPC first
        match <HttpRpc as ConsensusRpc<S>>::get_bootstrap(self.rpcs.first().unwrap(), checkpoint)
            .await
        {
            Ok(result) => return Ok(result),
            Err(err) => {
                warn!(target: "proxy::bootstrap", "main rpc failed trying backups {:#?}", err);
            }
        };

        // 2) Build timeout‑wrapped futures for all the backups
        let tasks: Vec<_> = self
            .rpcs
            .iter()
            .skip(1)
            .map(|rpc| {
                tokio::time::timeout(
                    // this timeout protects us from indefinitely hanging RPC requests. It's not added for HttpRpc requests by default
                    Duration::from_secs(5),
                    <HttpRpc as ConsensusRpc<S>>::get_bootstrap(rpc, checkpoint),
                )
            })
            .collect();

        // 3) Run them all, drop any that timed out or errored, and take the first valid one, if available
        futures::future::join_all(tasks)
            .await
            .into_iter()
            .filter_map(|res| res.ok().and_then(Result::ok))
            .find(|bootstrap| bootstrap.header().beacon().tree_hash_root() == checkpoint)
            .ok_or_else(|| eyre::eyre!("Failed to fetch bootstrap from any backups"))
    }

    async fn get_updates(&self, period: u64, count: u8) -> Result<Vec<Update<S>>> {
        // 1) Try the primary RPC first
        if let Ok(updates) =
            <HttpRpc as ConsensusRpc<S>>::get_updates(self.rpcs.first().unwrap(), period, count)
                .await
        {
            return Ok(updates);
        }
        warn!(target: "proxy::updates", "main rpc failed; falling back to backups for updates");

        // 2) Build timeout‑wrapped futures for all the backups
        let tasks: Vec<_> = self
            .rpcs
            .iter()
            .skip(1)
            .map(|rpc| {
                tokio::time::timeout(
                    Duration::from_secs(5),
                    <HttpRpc as ConsensusRpc<S>>::get_updates(rpc, period, count),
                )
            })
            .collect();

        // todo: here, I want to check for the current expected slot + period and check output against that
        //  rather than blindly taking the first non-errored output.
        // 3) Run them all, drop any that timed out or errored, and take the first successful Vec<Update>
        futures::future::join_all(tasks)
            .await
            .into_iter()
            .filter_map(|res| {
                // drop timeouts, then drop RPC errors, leaving Vec<Update>
                res.ok().and_then(Result::ok)
            })
            .next()
            .ok_or_else(|| eyre::eyre!("Failed to fetch updates from any backups"))
    }

    async fn get_finality_update(&self) -> Result<FinalityUpdate<S>> {
        // 1) Try the primary RPC first
        if let Ok(update) =
            <HttpRpc as ConsensusRpc<S>>::get_finality_update(self.rpcs.first().unwrap()).await
        {
            return Ok(update);
        }
        warn!(
            target: "proxy::finality",
            "main rpc failed; falling back to backups for finality update"
        );

        // 2) Wrap all the backup calls in a timeout
        let tasks: Vec<_> = self
            .rpcs
            .iter()
            .skip(1)
            .map(|rpc| {
                tokio::time::timeout(
                    Duration::from_secs(5),
                    <HttpRpc as ConsensusRpc<S>>::get_finality_update(rpc),
                )
            })
            .collect();

        // todo: here, I want to check for the current expected slot and check output against that
        //  rather than blindly taking the first non-errored output.
        // 3) Run them all, drop timeouts & RPC errors, take the first success
        futures::future::join_all(tasks)
            .await // Vec<Result<Result<FinalityUpdate, RpcError>, Elapsed>>
            .into_iter()
            .filter_map(|res| {
                res.ok() // drop Err(Elapsed)
                    .and_then(Result::ok) // drop Err(RpcError)
            })
            .next() // Option<FinalityUpdate>
            .ok_or_else(|| eyre::eyre!("Failed to fetch finality update from any backups"))
    }

    async fn get_optimistic_update(&self) -> Result<OptimisticUpdate<S>> {
        // 1) Try the primary RPC first
        match <HttpRpc as ConsensusRpc<S>>::get_optimistic_update(self.rpcs.first().unwrap()).await
        {
            Ok(update) => return Ok(update),
            Err(err) => {
                warn!(
                    target: "proxy::optimistic",
                    "main rpc failed; falling back to backups for optimistic update: {:#?}", err
                );
            }
        }

        // 2) Build timeout‑wrapped futures for all the backups
        let tasks: Vec<_> = self
            .rpcs
            .iter()
            .skip(1)
            .map(|rpc| {
                tokio::time::timeout(
                    Duration::from_secs(5),
                    <HttpRpc as ConsensusRpc<S>>::get_optimistic_update(rpc),
                )
            })
            .collect();

        // 3) Run them all, drop any that timed out or errored, and take the first valid one
        futures::future::join_all(tasks)
            .await
            .into_iter()
            .filter_map(|res| res.ok().and_then(Result::ok))
            .next()
            .ok_or_else(|| eyre::eyre!("Failed to fetch optimistic update from any backups"))
    }

    async fn get_block(&self, slot: u64) -> Result<BeaconBlock<S>> {
        // 1) Try the primary RPC first
        match <HttpRpc as ConsensusRpc<S>>::get_block(self.rpcs.first().unwrap(), slot).await {
            Ok(block) => return Ok(block),
            Err(err) => {
                warn!(
                    target: "proxy::block",
                    "main rpc failed; falling back to backups for block at slot {}: {:#?}", slot, err
                );
            }
        }

        // 2) Build timeout‑wrapped futures for all the backups
        let tasks: Vec<_> = self
            .rpcs
            .iter()
            .skip(1)
            .map(|rpc| {
                tokio::time::timeout(
                    Duration::from_secs(5),
                    <HttpRpc as ConsensusRpc<S>>::get_block(rpc, slot),
                )
            })
            .collect();

        // 3) Run them all, drop any that timed out or errored, and take the first valid one
        futures::future::join_all(tasks)
            .await
            .into_iter()
            .filter_map(|res| res.ok().and_then(Result::ok))
            .next()
            .ok_or_else(|| eyre::eyre!("Failed to fetch block for slot {} from any backups", slot))
    }

    async fn chain_id(&self) -> Result<u64> {
        // 1) Try the primary RPC first
        match <HttpRpc as ConsensusRpc<S>>::chain_id(self.rpcs.first().unwrap()).await {
            Ok(id) => return Ok(id),
            Err(err) => {
                warn!(
                    target: "proxy::chain_id",
                    "main rpc failed; falling back to backups for chain_id: {:#?}", err
                );
            }
        }

        // 2) Build timeout‑wrapped futures for all the backups
        let tasks: Vec<_> = self
            .rpcs
            .iter()
            .skip(1)
            .map(|rpc| {
                tokio::time::timeout(
                    Duration::from_secs(5),
                    <HttpRpc as ConsensusRpc<S>>::chain_id(rpc),
                )
            })
            .collect();

        // 3) Run them all, drop any that timed out or errored, and take the first valid one
        futures::future::join_all(tasks)
            .await
            .into_iter()
            .filter_map(|res| res.ok().and_then(Result::ok))
            .next()
            .ok_or_else(|| eyre::eyre!("Failed to fetch chain_id from any backups"))
    }
}
