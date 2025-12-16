use std::{
    cmp::Ordering,
    env,
    future::Future,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::anyhow;

use alloy::{
    eips::BlockId,
    hex::{self},
};
use alloy_primitives::B256;
use anyhow::Result;
use chrono::Duration;
use futures::{
    future::{join_all, select_ok},
    FutureExt,
};
use helios_consensus_core::{
    apply_bootstrap, calc_sync_period,
    consensus_spec::ConsensusSpec,
    types::{FinalityUpdate, LightClientStore, Update},
    verify_bootstrap,
};
use helios_ethereum::{config::Config, rpc::ConsensusRpc};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::api::ProofRequest;

pub const MAX_REQUEST_LIGHT_CLIENT_UPDATES: u8 = 128;
const CONENSUS_RPCS_ENV_VAR: &str = "CONSENSUS_RPCS_LIST";

/// A modified implementation of `Inner`, that allows for tight control over multiplexing
pub struct Client<S: ConsensusSpec, R: ConsensusRpc<S>> {
    pub config: Arc<Config>,
    pub store: LightClientStore<S>,
    rpcs: Vec<R>,
    // todo? Honestly, this last_checkpoint is not used for much. We could be logging it for easier debug, but ...
    last_checkpoint: Option<B256>,
    // todo? smth like `finalized_block_send: Sender<Block<Transaction>>,`
}

pub struct ConsensusProofInputs<S: ConsensusSpec> {
    pub initial_store: LightClientStore<S>,
    pub expected_current_slot: u64,
    pub genesis_root: B256,
    pub forks: helios_consensus_core::types::Forks,
    pub sync_committee_updates: Vec<Update<S>>,
    pub finality_update: FinalityUpdate<S>,
    pub execution_block_id: BlockId,
    pub config: Arc<Config>,
}

pub async fn get_proof_inputs<S: ConsensusSpec>(
    request: &ProofRequest,
) -> Result<ConsensusProofInputs<S>> {
    debug!(target: "consensus_client::proof_inputs", "start");

    let mut client = Client::<S, helios_ethereum::rpc::http_rpc::HttpRpc>::from_env()?;
    client
        .bootstrap(request.dst_chain_contract_from_header)
        .await?;

    let initial_store = client.store.clone();

    let trace = client.sync_to_chain().await?;

    // ZK program requires that the head(slot) advance at least once. If it didn't, return error and
    // let caller try again
    let initial_slot = initial_store.finalized_header.beacon().slot;
    let new_slot = client.store.finalized_header.beacon().slot;
    if new_slot <= initial_slot {
        return Err(anyhow!(
            "finalized head did not advance. Initial head: {} , finalized head: {}",
            initial_slot,
            new_slot
        ));
    }

    let execution_block_number = *client
        .store
        .finalized_header
        .execution()
        .map_err(|_| {
            anyhow!(
            "current epoch does not belong to a hard fork that enables header execution payload"
        )
        })?
        .block_number();

    debug!(target: "consensus_client::proof_inputs", "successfully generated proof inputs. Finalized slot {}", trace.final_store.finalized_header.beacon().slot);

    Ok(ConsensusProofInputs {
        initial_store,
        expected_current_slot: trace.expected_current_slot,
        genesis_root: client.config.chain.genesis_root,
        forks: client.config.forks.clone(),
        sync_committee_updates: trace.sync_committee_updates,
        finality_update: trace.finality_update,
        execution_block_id: BlockId::Number(alloy::eips::BlockNumberOrTag::Number(
            execution_block_number,
        )),
        config: client.config.clone(),
    })
}

// todo: If we decide to implement `get_proof_inputs` for Inner as well, this is a good starting point
// async fn get_consensus_part_of_inputs<S: ConsensusSpec, R: ConsensusRpc<S>>(
//     &self,
//     request: &ProofRequest,
// ) -> anyhow::Result<(Inner<S, R>, Vec<Update<S>>, FinalityUpdate<S>)> {
//     let client = try_get_client::<S, R>(request.head_checkpoint)
//         .await
//         .context("Failed to get light client from checkpoint")
//         .map_err(|e| anyhow!(e.to_string()))?;

//     let sync_committee_updates = try_get_updates::<S, R>(&client)
//         .await
//         .context("Failed to get sync committee updates")
//         .map_err(|e| anyhow!(e.to_string()))?;

//     let finality_update: FinalityUpdate<S> =
//         client.rpc.get_finality_update().await.map_err(|e| {
//             anyhow!(format!(
//                 "Failed to get finality update from consensus RPC: {}",
//                 e
//             ))
//         })?;

//     Ok((client, sync_committee_updates, finality_update))
// }

// todo? Consider adding metadata, like rpc name or execution time etc.
struct SyncToChainTrace<S: ConsensusSpec> {
    sync_committee_updates: Vec<Update<S>>,
    finality_update: FinalityUpdate<S>,
    final_store: LightClientStore<S>,
    last_checkpoint: Option<B256>,
    expected_current_slot: u64,
}

impl<S: ConsensusSpec, R: ConsensusRpc<S> + std::fmt::Debug> Client<S, R> {
    pub fn from_env() -> Result<Self> {
        let chain_id = std::env::var("SOURCE_CHAIN_ID")
            .map_err(|e| anyhow::anyhow!("Failed to get SOURCE_CHAIN_ID: {}", e))?;
        let network = helios_ethereum::config::networks::Network::from_chain_id(
            chain_id
                .parse()
                .map_err(|e| anyhow::anyhow!("Failed to parse chain_id: {}", e))?,
        )
        .map_err(|_| anyhow::anyhow!("Failed to get network from chain_id: {}", chain_id))?;
        let base_config = network.to_base_config();

        let config = Config {
            chain: base_config.chain,
            forks: base_config.forks,
            strict_checkpoint_age: false,
            max_checkpoint_age: 604800, // 1 week
            ..Default::default()
        };

        // In a simple case, paths_str is a coma-separated list of URLs. But that depends on which
        // R we're using. Some R's expect these "urls" to have different meaning, e.g. it can be
        // a name of another env variable with some init params for that R in R::new
        let paths_str = env::var(CONENSUS_RPCS_ENV_VAR)
            .expect("Client: No RPC URLs found in environment variable");

        let paths: Vec<&str> = paths_str
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        Self::new(Arc::new(config), &paths)
    }

    pub fn new(config: Arc<Config>, paths: &[&str]) -> Result<Self> {
        assert!(
            !paths.is_empty(),
            "Client: No RPC URLs found in environment variable"
        );

        let rpcs: Vec<R> = paths.iter().map(|path| R::new(path)).collect();

        info!(
            target: "consensus_client::new",
            "creating client with {} RPC endpoints: {:?}",
            rpcs.len(),
            rpcs
        );

        Ok(Self {
            config,
            store: LightClientStore::default(),
            rpcs,
            last_checkpoint: None,
        })
    }

    pub async fn sync(&mut self, checkpoint: B256) -> Result<()> {
        self.store = LightClientStore::default();
        self.last_checkpoint = None;

        self.bootstrap(checkpoint).await?;
        self.sync_to_chain().await?;

        info!(
            target: "consensus_client::sync",
            "in sync with checkpoint: 0x{}",
            hex::encode(self.last_checkpoint.unwrap_or_default())
        );

        Ok(())
    }

    /// Get expected current slot based on current time.
    /// Convenience method that delegates to ConfigExt.
    pub fn expected_current_slot(&self) -> u64 {
        self.config.expected_current_slot()
    }

    /// Fetch a beacon block by slot.
    /// Races all RPCs in parallel, returns the first successful result.
    pub async fn get_block(
        &self,
        slot: u64,
    ) -> Result<helios_consensus_core::types::BeaconBlock<S>> {
        let futs: Vec<_> = self
            .rpcs
            .iter()
            .map(|rpc| {
                async move {
                    timeout(std::time::Duration::from_secs(5), rpc.get_block(slot))
                        .await
                        .map_err(|_| anyhow!("RPC timed out"))?
                        .map_err(|e| anyhow!("RPC failed: {}", e))
                }
                .boxed()
            })
            .collect();

        if futs.is_empty() {
            return Err(anyhow!(
                "No RPCs available to fetch block for slot {}",
                slot
            ));
        }

        match select_ok(futs).await {
            Ok((block, _remaining)) => Ok(block),
            Err(e) => Err(anyhow!(
                "All RPCs failed to fetch block for slot {}: {}",
                slot,
                e
            )),
        }
    }

    // bootstrap is a heavy call, use a 12-sec timeout for it
    const BOOTSTRAP_TIMEOUT_SECS: u64 = 12;
    pub async fn bootstrap(&mut self, checkpoint: B256) -> Result<()> {
        let mut futs = vec![];
        for rpc in &self.rpcs {
            let mut store = self.store.clone();
            let config = self.config.clone();
            // Create a future for every RPC that will try to bootstrap on the provided checkpoint
            let fut: std::pin::Pin<Box<dyn Future<Output = Result<LightClientStore<S>>> + Send>> = async move {
                match timeout(std::time::Duration::from_secs(Self::BOOTSTRAP_TIMEOUT_SECS), async {
                    let bootstrap = rpc
                        .get_bootstrap(checkpoint)
                        .await
                        .map_err(|e| anyhow::anyhow!("RPC error getting bootstrap: {:#?}", e))?;

                    // @notice `is_valid_checkpoint` only checks slot's age
                    let is_valid = config.is_valid_checkpoint(bootstrap.header().beacon().slot);

                    if !is_valid {
                        if config.strict_checkpoint_age {
                            return Err(anyhow::anyhow!("ConsensusError::CheckpointTooOld.into()"));
                        } else {
                            warn!(target: "consensus_client::bootstrap", "checkpoint too old, consider using a more recent block");
                        }
                    }

                    verify_bootstrap(&bootstrap, checkpoint, &config.forks)
                        .map_err(|e| anyhow!("{e}"))?;
                    apply_bootstrap(&mut store, &bootstrap);

                    Ok::<helios_consensus_core::types::LightClientStore<S>, anyhow::Error>(store)
                }).await {
                    Ok(Ok(res)) => Ok(res),
                    Ok(Err(e)) => Err(anyhow!("rpc error: {e}")),
                    Err(e) => Err(anyhow!("rpc timed out: {e}")),
                }
            }.boxed();
            futs.push(fut);
        }

        // Take first non-erroring result. Each fut checks the integrity of bootstrap and therefore
        // resulting `LightClientStore`. So take the fastest completing one here
        let validated_store = select_ok(futs).await;
        match validated_store {
            Ok((store, futs)) => {
                drop(futs);
                self.store = store;
                // todo: this is a deviation from helios logic, but I'm 99% it's warranted
                self.last_checkpoint = Some(checkpoint);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    const ADVANCE_TIMEOUT_SECS: u64 = 4;
    pub async fn advance(&mut self) -> Result<()> {
        debug!(
            target: "consensus_client::advance",
            "start"
        );

        let futs = self.rpcs.iter().map(|rpc| {
            let store = self.store.clone();
            let config = self.config.clone();
            async move {
                match timeout(
                    std::time::Duration::from_secs(Self::ADVANCE_TIMEOUT_SECS),
                    async move { Self::advance_single_rpc(store, config, rpc).await },
                )
                .await
                {
                    Ok(Ok(res)) => Ok(res),
                    Ok(Err(err)) => Err(anyhow!("rpc error {:?} : {err} ", rpc)),
                    Err(elapsed) => Err(anyhow!("rpc error {:?} : {elapsed} ", rpc)),
                }
            }
        });

        debug!(target: "consensus_client::advance", "waiting for response from {} rpcs", futs.len());

        // Wait for all futures and pick the most advanced store
        let results = join_all(futs).await;
        let mut candidate: Option<(LightClientStore<S>, Option<B256>)> = None;
        for result in results {
            match result {
                Ok((result_store, result_checkpoint)) => {
                    if let Some((ref candidate_store, _)) = candidate {
                        if compare_store_advancement(&result_store, candidate_store)
                            == Ordering::Greater
                        {
                            candidate = Some((result_store, result_checkpoint));
                        }
                    } else {
                        candidate = Some((result_store, result_checkpoint));
                    }
                }
                Err(e) => {
                    debug!(
                        target: "consensus_client::advance",
                        "advance errored for rpc {}", e
                    );
                }
            }
        }

        // todo: log that we successfully advanced
        if let Some(candidate) = candidate {
            let (store, last_checkpoint) = candidate;
            self.apply_store_update(store, last_checkpoint);

            Ok(())
        } else {
            Err(anyhow!("all rpcs failed"))
        }
    }

    async fn advance_single_rpc(
        mut store: LightClientStore<S>,
        config: Arc<Config>,
        rpc: &R,
    ) -> Result<(LightClientStore<S>, Option<B256>)> {
        let mut last_checkpoint: Option<B256> = None;
        let finality_update = rpc
            .get_finality_update()
            .await
            .map_err(|e| anyhow!("{e}"))?;

        helios_consensus_core::verify_finality_update::<S>(
            &finality_update,
            config.expected_current_slot(),
            &store,
            config.chain.genesis_root,
            &config.forks,
        )
        .map_err(|e| anyhow!("{e}"))?;

        Self::apply_finality_update(&mut last_checkpoint, &mut store, &finality_update);

        if store.next_sync_committee.is_none() {
            info!(target: "consensus_client::advance_single_rpc", "checking for sync committee update");
            let current_period = calc_sync_period::<S>(store.finalized_header.beacon().slot);
            let mut updates = rpc
                .get_updates(current_period, 1)
                .await
                .map_err(|e| anyhow!("{e}"))?;

            if updates.len() == 1 {
                let update = updates.get_mut(0).unwrap();
                let res = helios_consensus_core::verify_update::<S>(
                    update,
                    config.expected_current_slot(),
                    &store,
                    config.chain.genesis_root,
                    &config.forks,
                )
                .map_err(|e| anyhow!("{e}"));

                if res.is_ok() {
                    Self::apply_update(&mut last_checkpoint, &mut store, update);
                }
            }
        }

        Ok((store, last_checkpoint))
    }

    const SYNC_TO_CHAIN_TIMEOUT_SECS: u64 = 12;
    async fn sync_to_chain(&mut self) -> Result<SyncToChainTrace<S>> {
        let futs = self.rpcs.iter().map(|rpc| {
            let store = self.store.clone();
            let config = self.config.clone();
            async move {
                match timeout(
                    std::time::Duration::from_secs(Self::SYNC_TO_CHAIN_TIMEOUT_SECS),
                    async move { Self::sync_to_chain_single_rpc(store, config, rpc).await },
                )
                .await
                {
                    Ok(Ok(res)) => Ok(res),
                    Ok(Err(err)) => Err(anyhow!("rpc error {:?} : {err} ", rpc)),
                    Err(elapsed) => Err(anyhow!("rpc error {:?} : {elapsed} ", rpc)),
                }
            }
        });

        // Wait for all RPCs to finish in order to pick the result that advances our chain view the most
        let results = join_all(futs).await;
        let mut candidate: Option<SyncToChainTrace<S>> = None;
        for result in results {
            match result {
                Ok(result_trace) => {
                    if let Some(ref candidate_trace) = candidate {
                        if compare_store_advancement(
                            &result_trace.final_store,
                            &candidate_trace.final_store,
                        ) == Ordering::Greater
                        {
                            candidate = Some(result_trace);
                        }
                    } else {
                        candidate = Some(result_trace);
                    }
                }
                Err(e) => {
                    warn!(
                        target: "consensus_client::sync_to_chain",
                        "sync errored for RPC {:#?}", e
                    );
                }
            }
        }

        if let Some(trace) = candidate {
            self.apply_store_update(trace.final_store.clone(), trace.last_checkpoint);

            Ok(trace)
        } else {
            Err(anyhow!("all rpcs failed"))
        }
    }

    async fn sync_to_chain_single_rpc(
        mut store: LightClientStore<S>,
        config: Arc<Config>,
        rpc: &R,
    ) -> Result<SyncToChainTrace<S>> {
        let mut last_checkpoint: Option<B256> = None;
        let expected_current_slot = config.expected_current_slot();
        let current_finalized_slot = store.finalized_header.beacon().slot;
        let updates =
            Self::get_updates_single_rpc(expected_current_slot, current_finalized_slot, rpc)
                .await?;

        for update in &updates {
            helios_consensus_core::verify_update::<S>(
                update,
                config.expected_current_slot(),
                &store,
                config.chain.genesis_root,
                &config.forks,
            )
            .map_err(|e| anyhow!("{e}"))?;
            Self::apply_update(&mut last_checkpoint, &mut store, update);
        }

        let finality_update = rpc
            .get_finality_update()
            .await
            .map_err(|e| anyhow!("{e}"))?;

        helios_consensus_core::verify_finality_update::<S>(
            &finality_update,
            config.expected_current_slot(),
            &store,
            config.chain.genesis_root,
            &config.forks,
        )
        .map_err(|e| anyhow!("{e}"))?;

        Self::apply_finality_update(&mut last_checkpoint, &mut store, &finality_update);

        // If we're at this point, all verifications succeeded so we return the final trace
        Ok(SyncToChainTrace {
            sync_committee_updates: updates,
            finality_update,
            final_store: store,
            last_checkpoint,
            expected_current_slot,
        })
    }

    async fn get_updates_single_rpc(
        expected_current_slot: u64,
        current_finalized_slot: u64,
        rpc: &R,
    ) -> Result<Vec<Update<S>>> {
        let expected_current_period = calc_sync_period::<S>(expected_current_slot);
        let mut next_update_fetch_period = calc_sync_period::<S>(current_finalized_slot);

        let mut updates: Vec<Update<S>> = vec![];
        if expected_current_period - next_update_fetch_period >= 128 {
            while next_update_fetch_period < expected_current_period {
                let batch_size = std::cmp::min(
                    expected_current_period - next_update_fetch_period,
                    MAX_REQUEST_LIGHT_CLIENT_UPDATES.into(),
                );
                let update = rpc
                    .get_updates(next_update_fetch_period, batch_size.try_into().unwrap())
                    .await
                    .map_err(|e| anyhow!("{e}"))?;
                updates.extend(update);

                next_update_fetch_period += batch_size;
            }
        }

        let update: Vec<Update<S>> = rpc
            .get_updates(next_update_fetch_period, MAX_REQUEST_LIGHT_CLIENT_UPDATES)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        updates.extend(update);

        Ok(updates)
    }

    /// A helper function to never forget updating checkpoint
    fn apply_update(
        checkpoint: &mut Option<B256>,
        store: &mut LightClientStore<S>,
        update: &Update<S>,
    ) {
        let new_checkpoint = helios_consensus_core::apply_update::<S>(store, update);
        if new_checkpoint.is_some() {
            *checkpoint = new_checkpoint;
        }
    }

    /// A helper function to never forget updating checkpoint
    fn apply_finality_update(
        checkpoint: &mut Option<B256>,
        store: &mut LightClientStore<S>,
        update: &FinalityUpdate<S>,
    ) {
        let new_checkpoint = helios_consensus_core::apply_finality_update::<S>(store, update);
        if new_checkpoint.is_some() {
            *checkpoint = new_checkpoint;
        }
    }

    /// A helper function to never forget updating checkpoint
    fn apply_store_update(&mut self, new_store: LightClientStore<S>, new_checkpoint: Option<B256>) {
        let prev_finalized_slot = self.store.finalized_header.beacon().slot;
        let prev_optimistic_slot = self.store.optimistic_header.beacon().slot;
        self.store = new_store;
        let new_finalized_slot = self.store.finalized_header.beacon().slot;
        let new_optimistic_slot = self.store.optimistic_header.beacon().slot;
        if new_checkpoint.is_some() {
            self.last_checkpoint = new_checkpoint;
        }
        if new_finalized_slot != prev_finalized_slot {
            self.log_finality_update();
        }
        if new_optimistic_slot != prev_optimistic_slot {
            self.log_optimistic_update()
        }
    }

    fn log_finality_update(&self) {
        let age = self.config.age(self.store.finalized_header.beacon().slot);

        info!(
            target: "consensus_client::update",
            "finalized slot             slot={}  age={:02}:{:02}:{:02}:{:02}",
            self.store.finalized_header.beacon().slot,
            age.num_days(),
            age.num_hours() % 24,
            age.num_minutes() % 60,
            age.num_seconds() % 60,
        );
    }

    fn log_optimistic_update(&self) {
        let age = self.config.age(self.store.optimistic_header.beacon().slot);

        info!(
            target: "consensus_client::update",
            "updated head               slot={}  age={:02}:{:02}:{:02}:{:02}",
            self.store.optimistic_header.beacon().slot,
            age.num_days(),
            age.num_hours() % 24,
            age.num_minutes() % 60,
            age.num_seconds() % 60,
        );
    }
}

/// Compares two LightClientStore instances to determine which is more advanced.
/// Advancement is primarily determined by the finalized header slot.
/// If slots are equal, the presence of a next_sync_committee is used as a tie-breaker.
fn compare_store_advancement<S: ConsensusSpec>(
    store_a: &LightClientStore<S>,
    store_b: &LightClientStore<S>,
) -> Ordering {
    // Compare finalized header slots
    match store_a
        .finalized_header
        .beacon()
        .slot
        .cmp(&store_b.finalized_header.beacon().slot)
    {
        Ordering::Less => Ordering::Less,
        Ordering::Greater => Ordering::Greater,
        Ordering::Equal => {
            // If slots are equal, compare sync committee presence
            match (
                store_a.next_sync_committee.is_some(),
                store_b.next_sync_committee.is_some(),
            ) {
                (true, false) => Ordering::Greater, // a has committee, b doesn't -> a is more advanced
                (false, true) => Ordering::Less, // b has committee, a doesn't -> b is more advanced
                _ => Ordering::Equal, // both have or both don't have -> equally advanced by this metric
            }
        }
    }
}

pub trait ConfigExt {
    fn is_valid_checkpoint(&self, blockhash_slot: u64) -> bool;
    fn expected_current_slot(&self) -> u64;
    fn slot_timestamp(&self, slot: u64) -> u64;
    #[allow(dead_code)]
    fn age(&self, slot: u64) -> Duration;
    fn duration_until_next_update(&self) -> Duration;
}

impl<T: std::ops::Deref<Target = Config>> ConfigExt for T {
    fn is_valid_checkpoint(&self, blockhash_slot: u64) -> bool {
        let current_slot = self.expected_current_slot();
        let current_slot_timestamp = self.slot_timestamp(current_slot);
        let blockhash_slot_timestamp = self.slot_timestamp(blockhash_slot);

        let slot_age = current_slot_timestamp
            .checked_sub(blockhash_slot_timestamp)
            .unwrap_or_default();

        slot_age < self.max_checkpoint_age
    }

    fn expected_current_slot(&self) -> u64 {
        let now = SystemTime::now();
        helios_consensus_core::expected_current_slot(now, self.chain.genesis_time)
    }

    fn slot_timestamp(&self, slot: u64) -> u64 {
        slot * 12 + self.chain.genesis_time
    }

    fn age(&self, slot: u64) -> Duration {
        let expected_time = self.slot_timestamp(slot);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| panic!("unreachable"));

        let delay = now - std::time::Duration::from_secs(expected_time);
        chrono::Duration::from_std(delay).unwrap()
    }

    fn duration_until_next_update(&self) -> Duration {
        let current_slot = self.expected_current_slot();
        let next_slot = current_slot + 1;
        let next_slot_timestamp = self.slot_timestamp(next_slot);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| panic!("unreachable"))
            .as_secs();

        let time_to_next_slot = next_slot_timestamp - now;
        let next_update = time_to_next_slot + 4;

        Duration::try_seconds(next_update as i64).unwrap()
    }
}
