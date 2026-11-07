use crate::types::{ProofId, ProofRequestState, ProofRequestStatus, ProofServiceError};
use alloy_primitives::B256;
use anyhow::{anyhow, Context};
use helios_consensus_core::types::LightClientHeader;
use redis::{
    aio::{ConnectionManager, ConnectionManagerConfig},
    AsyncCommands, Client,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{env, marker::PhantomData, time::Duration};
use tracing::{debug, info, warn};
use tree_hash::TreeHash;
use uuid::Uuid;

// todo: change to .env variable once our deployment env is ready for that
pub const PROOF_STATE_TTL_SECS: u64 = 7 * 24 * 60 * 60; // 1 week
pub const REDIS_TIMEOUT_SECS: u64 = 10;

#[derive(Deserialize, Serialize)]
struct FinalizedExecution {
    header_root: B256,
    block_number: u64,
}

/// A consistent read of the header and any execution metadata authenticated for it.
pub(crate) struct StoredFinalizedHeader {
    pub header: LightClientHeader,
    pub block_number: Option<u64>,
    // Preserve the exact Redis value for conditional writes, including legacy encodings.
    encoded: String,
}

pub struct RedisStore<ProofOutput>
where
    ProofOutput: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    pub conn_manager: ConnectionManager,
    key_prefix: String,
    global_lock_duration: Duration,
    _phantom: PhantomData<ProofOutput>,
}

impl<ProofOutput> RedisStore<ProofOutput>
where
    ProofOutput: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    pub async fn new() -> anyhow::Result<Self> {
        let redis_url =
            env::var("REDIS_URL").context("REDIS_URL environment variable must be set")?;
        let lock_duration_secs: u64 = env::var("REDIS_LOCK_DURATION_SECS")
            .context("REDIS_LOCK_DURATION_SECS environment variable must be set")?
            .parse()
            .context("REDIS_LOCK_DURATION_SECS must be a number")?;
        let key_prefix = env::var("REDIS_KEY_PREFIX")
            .context("REDIS_KEY_PREFIX environment variable must be set")?;

        info!("RedisStore configuration:");
        info!(" - URL: {}", redis_url);
        info!(" - Global Lock Duration: {}s", lock_duration_secs);
        info!(" - Key Prefix: {}", key_prefix);

        let config = ConnectionManagerConfig::new()
            .set_connection_timeout(Duration::from_secs(REDIS_TIMEOUT_SECS))
            .set_response_timeout(Duration::from_secs(REDIS_TIMEOUT_SECS));

        let client = Client::open(redis_url.as_str()).context("Failed to create Redis client")?;
        let conn_manager = match tokio::time::timeout(
            Duration::from_secs(5),
            ConnectionManager::new_with_config(client, config),
        )
        .await
        {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => return Err(anyhow!("Failed to connect to Redis: {}", e)),
            Err(_) => return Err(anyhow!("Timed out connecting to Redis after 5 seconds")),
        };

        Ok(Self {
            conn_manager,
            key_prefix,
            global_lock_duration: Duration::from_secs(lock_duration_secs),
            _phantom: PhantomData::<ProofOutput>,
        })
    }

    /// Generates the Redis key for storing the state of a specific proof request.
    pub fn proof_state_key(&self, id: &ProofId) -> String {
        format!("{}:state:{}", self.key_prefix, id.to_hex_string())
    }

    /// Generates the Redis key for storing the latest finalized header.
    pub fn finalized_header_key(&self) -> String {
        format!("{}:state:finalized_header", self.key_prefix)
    }

    // A new key avoids changing the format read by older deployments. The old unbound
    // execution_block_number key is not trusted. Keep metadata outside the proof-state scan.
    fn finalized_execution_key(&self) -> String {
        format!("{}:finalized_header:execution", self.key_prefix)
    }

    /// Generates a global redis lock for the service prefix.
    pub fn global_lock_key(&self) -> String {
        format!("{}:lock", self.key_prefix)
    }

    /// Generates a redis lock for a specific proof id.
    pub fn proof_generation_lock_key(&self, proof_id: &ProofId) -> String {
        format!("{}:lock:{}", self.key_prefix, proof_id.to_hex_string())
    }

    // Read/Write methods
    /// Fetch and deserialize the state for a specific proof request.
    pub async fn get_proof_state(
        &mut self,
        id: &ProofId,
    ) -> Result<Option<ProofRequestState<ProofOutput>>, ProofServiceError> {
        let key = self.proof_state_key(id);
        self.read_json_value(&key).await
    }

    /// Fetch and deserialize the latest finalized header.
    pub async fn read_finalized_header(
        &mut self,
    ) -> Result<Option<LightClientHeader>, ProofServiceError> {
        let key = self.finalized_header_key();
        self.read_json_value(&key).await
    }

    /// Read the header and its cached execution number atomically. A legacy writer can update
    /// only the header, so cached metadata is usable only when its header root still matches.
    pub(crate) async fn read_finalized_header_and_block_number(
        &mut self,
    ) -> Result<Option<StoredFinalizedHeader>, ProofServiceError> {
        let (header_json, execution_json): (Option<String>, Option<String>) = redis::cmd("MGET")
            .arg(self.finalized_header_key())
            .arg(self.finalized_execution_key())
            .query_async(&mut self.conn_manager)
            .await?;
        match header_json {
            Some(encoded) => {
                let header: LightClientHeader = serde_json::from_str(&encoded)?;
                let execution = execution_json
                    .map(|json| serde_json::from_str::<FinalizedExecution>(&json))
                    .transpose()?;
                let block_number = execution
                    .filter(|execution| execution.header_root == header.beacon().tree_hash_root())
                    .map(|execution| execution.block_number);
                Ok(Some(StoredFinalizedHeader {
                    header,
                    block_number,
                    encoded,
                }))
            }
            None => Ok(None),
        }
    }

    /// Set the state for a specific proof request.
    pub async fn set_proof_state(
        &mut self,
        id: &ProofId,
        state: &ProofRequestState<ProofOutput>,
    ) -> Result<(), ProofServiceError> {
        let key = self.proof_state_key(id);
        let json = serde_json::to_string(state)?;
        self.conn_manager
            .set_ex::<_, _, ()>(key, json, PROOF_STATE_TTL_SECS)
            .await?;
        Ok(())
    }

    /// Publish the header, its authenticated execution number and proof states together only
    /// if the header has not changed since it was read. This also guards against an expired
    /// lock or a legacy worker releasing another worker's lock during a rolling deployment.
    pub(crate) async fn update_finalized_header_and_proof_states(
        &mut self,
        expected: Option<&StoredFinalizedHeader>,
        latest_finalized_header: &LightClientHeader,
        execution_block_number: u64,
        updated_states: Vec<(ProofId, ProofRequestState<ProofOutput>)>,
    ) -> Result<bool, ProofServiceError> {
        let script = redis::Script::new(
            r#"
            if (redis.call('GET', KEYS[1]) or '') ~= ARGV[1] then
                return 0
            end
            redis.call('SET', KEYS[1], ARGV[2])
            redis.call('SET', KEYS[2], ARGV[3])
            for i = 3, #KEYS do
                redis.call('SET', KEYS[i], ARGV[i + 2], 'EX', ARGV[4])
            end
            return 1
        "#,
        );
        let execution = FinalizedExecution {
            header_root: latest_finalized_header.beacon().tree_hash_root(),
            block_number: execution_block_number,
        };
        let mut invocation = script.prepare_invoke();
        invocation
            .key(self.finalized_header_key())
            .key(self.finalized_execution_key())
            .arg(
                expected
                    .map(|stored| stored.encoded.as_str())
                    .unwrap_or_default(),
            )
            .arg(serde_json::to_string(latest_finalized_header)?)
            .arg(serde_json::to_string(&execution)?)
            .arg(PROOF_STATE_TTL_SECS);
        for (id, state) in updated_states {
            invocation
                .key(self.proof_state_key(&id))
                .arg(serde_json::to_string(&state)?);
        }
        Ok(invocation.invoke_async(&mut self.conn_manager).await?)
    }

    /// Retrieves all proof requests matching the given status from Redis.
    pub async fn find_requests_by_status(
        &mut self,
        status: ProofRequestStatus,
    ) -> Result<Vec<ProofRequestState<ProofOutput>>, ProofServiceError> {
        let pattern = format!("{}:state:*", self.key_prefix);
        let finalized_header_key_str = self.finalized_header_key();

        // Use SCAN to efficiently iterate through keys matching our pattern
        let mut cursor = 0;
        let mut found_requests = Vec::new();

        loop {
            // SCAN returns (next_cursor, [keys])
            let (next_cursor, keys): (i64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(100) // Process in batches of 100 keys
                .query_async(&mut self.conn_manager)
                .await
                .map_err(ProofServiceError::RedisError)?;

            // Filter out the finalized_header key before fetching values
            let proof_state_keys: Vec<String> = keys
                .into_iter()
                .filter(|k| k != &finalized_header_key_str)
                .collect();

            // Use MGET to fetch all values for actual proof state keys in a single round trip
            if !proof_state_keys.is_empty() {
                let state_jsons: Vec<Option<String>> = self
                    .conn_manager
                    .get(proof_state_keys.clone())
                    .await
                    .map_err(ProofServiceError::RedisError)?;

                // Process the results
                for (i, maybe_json) in state_jsons.into_iter().enumerate() {
                    if let Some(state_json) = maybe_json {
                        match serde_json::from_str::<ProofRequestState<ProofOutput>>(&state_json) {
                            Ok(state) if state.status == status => {
                                found_requests.push(state);
                            }
                            Ok(_) => {
                                // Not matching status, skip
                            }
                            Err(e) => {
                                // Log deserialization error but continue processing other keys
                                warn!(
                                    target: "redis_store",
                                    "Failed to deserialize proof request state from Redis key {}: {:#?}",
                                    proof_state_keys[i],
                                    e
                                );
                            }
                        }
                    }
                }
            }

            // If cursor is 0, we've completed the scan
            cursor = next_cursor;
            if cursor == 0 {
                break;
            }
        }

        debug!(
            target: "redis_store",
            "Scan found {} proof requests with status: {:?}",
            found_requests.len(),
            status
        );
        Ok(found_requests)
    }

    // Internal helper to fetch and deserialize a JSON value from Redis.
    async fn read_json_value<T>(
        &mut self,
        redis_key: &String,
    ) -> Result<Option<T>, ProofServiceError>
    where
        T: DeserializeOwned,
    {
        let state_json: Option<String> = self.conn_manager.get(redis_key).await?;
        match state_json {
            Some(json) => {
                let state: T = serde_json::from_str(&json)?;
                Ok(Some(state))
            }
            None => Ok(None),
        }
    }

    // Lock management methods
    /// Acquire the global lock and return its ownership token.
    pub(crate) async fn acquire_global_lock(
        &mut self,
    ) -> Result<Option<String>, ProofServiceError> {
        let token = Uuid::new_v4().to_string();
        let acquired: bool = redis::cmd("SET")
            .arg(self.global_lock_key())
            .arg(&token)
            .arg("NX")
            .arg("PX")
            .arg(self.global_lock_duration.as_millis() as u64)
            .query_async(&mut self.conn_manager)
            .await?;
        Ok(acquired.then_some(token))
    }

    /// Release only our own lock; an expired owner must not delete its successor's lock.
    pub(crate) async fn release_global_lock(&mut self, token: &str) {
        let _: Result<i64, _> = redis::Script::new(
            r#"
            if redis.call('GET', KEYS[1]) == ARGV[1] then
                return redis.call('DEL', KEYS[1])
            end
            return 0
        "#,
        )
        .key(self.global_lock_key())
        .arg(token)
        .invoke_async(&mut self.conn_manager)
        .await;
    }

    /// Acquire the lock for a specific proof generation task.
    pub async fn acquire_proof_generation_lock(
        &mut self,
        proof_id: &ProofId,
        duration_ms: u64,
    ) -> Result<bool, ProofServiceError> {
        let key = self.proof_generation_lock_key(proof_id);
        self.try_acquire_lock(&key, duration_ms).await
    }

    /// Release the lock for a specific proof generation task (best-effort).
    pub async fn release_proof_generation_lock(&mut self, proof_id: &ProofId) {
        let key = self.proof_generation_lock_key(proof_id);
        let _: Result<(), _> = self.conn_manager.del(&key).await;
        debug!(target: "redis_store", "Proof generation lock released (best-effort). Key: {}", key);
    }

    /// Extend the expiry of a proof generation lock.
    pub async fn extend_proof_generation_lock(
        &mut self,
        proof_id: &ProofId,
        duration_ms: u64,
    ) -> Result<bool, ProofServiceError> {
        let key = self.proof_generation_lock_key(proof_id);
        let res: i32 = redis::cmd("PEXPIRE")
            .arg(&key)
            .arg(duration_ms)
            .query_async(&mut self.conn_manager)
            .await?;
        Ok(res == 1) // Returns true if the expiry was successfully set
    }

    /// Internal helper to acquire a lock using SET NX PX.
    async fn try_acquire_lock(
        &mut self,
        key: &str,
        duration_ms: u64,
    ) -> Result<bool, ProofServiceError> {
        let acquired: bool = redis::cmd("SET")
            .arg(key)
            .arg("locked") // Value doesn't matter much, existence does
            .arg("NX") // Set only if key does not exist
            .arg("PX") // Set expiry in milliseconds
            .arg(duration_ms)
            .query_async(&mut self.conn_manager)
            .await?;
        Ok(acquired)
    }
}

impl<ProofOutput> Clone for RedisStore<ProofOutput>
where
    ProofOutput: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            conn_manager: self.conn_manager.clone(),
            key_prefix: self.key_prefix.clone(),
            global_lock_duration: self.global_lock_duration,
            _phantom: PhantomData::<ProofOutput>,
        }
    }
}
