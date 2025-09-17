use crate::types::{ProofId, ProofRequestState, ProofRequestStatus, ProofServiceError};
use anyhow::{anyhow, Context};
use helios_consensus_core::types::LightClientHeader;
use redis::{
    aio::{ConnectionManager, ConnectionManagerConfig},
    AsyncCommands, Client,
};
use serde::{de::DeserializeOwned, Serialize};
use std::{env, marker::PhantomData, time::Duration};
use tracing::{debug, info, warn};

// todo: change to .env variable once our deployment env is ready for that
const PROOF_STATE_TTL_SECS: u64 = 7 * 24 * 60 * 60; // 1 week

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
            .set_connection_timeout(Duration::from_secs(10))
            .set_response_timeout(Duration::from_secs(10));

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

    /// Set the latest finalized header.
    pub async fn set_finalized_header(
        &mut self,
        header: &LightClientHeader,
    ) -> Result<(), ProofServiceError> {
        let key = self.finalized_header_key();
        let json = serde_json::to_string(header)?;
        self.conn_manager.set::<_, _, ()>(key, json).await?;
        Ok(())
    }

    /// Atomically update the finalized header and multiple proof states.
    pub async fn update_finalized_header_and_proof_states(
        &mut self,
        latest_finalized_header: &LightClientHeader,
        updated_states: Vec<(ProofId, ProofRequestState<ProofOutput>)>,
    ) -> Result<(), ProofServiceError> {
        let mut pipe = redis::pipe();
        pipe.atomic();

        // Add the new finalized header to the transaction
        pipe.set(
            self.finalized_header_key(),
            serde_json::to_string(latest_finalized_header)?,
        );

        // Add updates for each proof state
        for (id, state) in updated_states {
            let key = self.proof_state_key(&id);
            pipe.cmd("SET")
                .arg(&key)
                .arg(serde_json::to_string(&state)?)
                .arg("EX")
                .arg(PROOF_STATE_TTL_SECS);
        }

        // Execute the transaction
        pipe.query_async::<()>(&mut self.conn_manager).await?;
        Ok(())
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
    /// Acquire the global lock.
    pub async fn acquire_global_lock(&mut self) -> Result<bool, ProofServiceError> {
        let key = self.global_lock_key();
        self.try_acquire_lock(&key, self.global_lock_duration.as_millis() as u64)
            .await
    }

    /// Release the global lock (best-effort).
    pub async fn release_global_lock(&mut self) {
        let key = self.global_lock_key();
        let _: Result<(), _> = self.conn_manager.del(&key).await;
        debug!(target: "redis_store", "Global lock released (best-effort). Key: {}", key);
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
