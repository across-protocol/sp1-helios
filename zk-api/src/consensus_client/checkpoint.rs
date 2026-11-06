//! Checkpoint (beacon block root) lookup against the configured consensus RPCs.
//!
//! The helios `ConsensusRpc` trait no longer exposes block fetching, so the block root at a
//! slot is read from the standard beacon API (`/eth/v1/beacon/headers/{slot}`) directly. The
//! fallback policy is the one the old `Client::get_block` used: the primary RPC is tried first
//! and, if it fails, the backups are raced through the shared [`multiplex`] helper.

use alloy_primitives::B256;
use anyhow::anyhow;
use futures::future::FutureExt;
use std::time::Duration;
use tracing::warn;

use crate::rpc_proxies::multiplex;

use super::CONENSUS_RPCS_ENV_VAR;

/// Upper bound on a single checkpoint request, covering connection, headers and body decoding.
/// Without it a primary RPC that accepts the connection but never answers would block the
/// fallback and keep healthy backups from ever being tried.
pub const CHECKPOINT_RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// Fetch the checkpoint (beacon block root) at `slot` from the RPCs in `CONSENSUS_RPCS_LIST`.
/// This only works for slots that are the first slot in their epoch.
pub async fn fetch_checkpoint(slot: u64) -> anyhow::Result<B256> {
    let rpcs = std::env::var(CONENSUS_RPCS_ENV_VAR)
        .map_err(|_| anyhow!("{CONENSUS_RPCS_ENV_VAR} not set"))?;
    let rpcs: Vec<String> = rpcs
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    fetch_checkpoint_from(&rpcs, slot, CHECKPOINT_RPC_TIMEOUT).await
}

/// Fetch the beacon block root at `slot`, trying the primary RPC first and falling back to
/// the remaining RPCs concurrently (through the shared [`multiplex`] helper) if it fails.
/// Each request, including its response body, is bounded by `timeout` so a stalled
/// endpoint cannot block the fallback or hang the call.
pub async fn fetch_checkpoint_from(
    rpcs: &[String],
    slot: u64,
    timeout: Duration,
) -> anyhow::Result<B256> {
    let (primary, backups) = rpcs
        .split_first()
        .ok_or_else(|| anyhow!("no consensus RPCs configured in {CONENSUS_RPCS_ENV_VAR}"))?;

    let client = reqwest::Client::builder().timeout(timeout).build()?;

    // 1) Try the primary RPC first
    match fetch_block_root(&client, primary, slot).await {
        Ok(root) => return Ok(root),
        Err(e) => warn!(
            target: "consensus_client::checkpoint",
            "Primary RPC failed; falling back to backups for slot {}: {}", slot, e
        ),
    }

    // 2) Race the backups and take the first that answers
    multiplex(
        |rpc: String| {
            let client = client.clone();
            async move { fetch_block_root(&client, &rpc, slot).await }.boxed()
        },
        backups,
    )
    .await
    .map_err(|e| anyhow!("All RPCs failed to fetch checkpoint for slot {slot}: {e}"))
}

/// Fetch the beacon block root at `slot` from a single beacon API endpoint.
async fn fetch_block_root(client: &reqwest::Client, rpc: &str, slot: u64) -> anyhow::Result<B256> {
    #[derive(serde::Deserialize)]
    struct HeaderResponse {
        data: HeaderData,
    }
    #[derive(serde::Deserialize)]
    struct HeaderData {
        root: B256,
    }

    let url = format!(
        "{}/eth/v1/beacon/headers/{}",
        rpc.trim_end_matches('/'),
        slot
    );
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow!("request to {rpc} failed: {e}"))?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "{rpc} returned status {} for slot {slot}",
            response.status()
        ));
    }
    let header = response
        .json::<HeaderResponse>()
        .await
        .map_err(|e| anyhow!("failed to parse header response from {rpc}: {e}"))?;
    Ok(header.data.root)
}
