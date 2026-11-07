use super::super::ProofService;
use crate::{proof_backends::ProofBackend, redis_store::RedisStore};
use alloy::consensus::Header;
use anyhow::{Context, Result};
use async_trait::async_trait;
use sp1_helios_primitives::types::ProofInputs;
use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{oneshot, Mutex, MutexGuard},
};

// RedisStore/Proxy read environment configuration. Serialize just these tests; each owns a
// private Redis process, so they never load .env or connect to a developer's Redis instance.
static ENV: Mutex<()> = Mutex::const_new(());

pub struct TestContext {
    pub store: RedisStore<String>,
    pub service: ProofService<UnusedBackend>,
    _server: RedisProcess,
    _env: MutexGuard<'static, ()>,
}

struct RedisProcess(Child);
impl Drop for RedisProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl TestContext {
    pub async fn new(rpc: &str) -> Result<Self> {
        let guard = ENV.lock().await;
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let server = RedisProcess(
            Command::new("redis-server")
                .args([
                    "--bind",
                    "127.0.0.1",
                    "--port",
                    &port.to_string(),
                    "--save",
                    "",
                    "--appendonly",
                    "no",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("install redis-server to run finality-cache tests")?,
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("private Redis did not start")?;
        std::env::set_var("REDIS_URL", format!("redis://127.0.0.1:{port}"));
        std::env::set_var("REDIS_LOCK_DURATION_SECS", "10");
        std::env::set_var("REDIS_KEY_PREFIX", "finality-test");
        std::env::set_var("SOURCE_EXECUTION_RPC_URL", rpc);
        Ok(Self {
            store: RedisStore::new().await?,
            service: ProofService::new(UnusedBackend).await?,
            _server: server,
            _env: guard,
        })
    }
}

#[derive(Clone)]
pub struct UnusedBackend;
#[async_trait]
impl ProofBackend for UnusedBackend {
    type ProofOutput = String;
    async fn generate_proof(&self, _: ProofInputs) -> Result<String> {
        panic!("test requests must not start proof generation")
    }
    fn vkey_digest(&self) -> Vec<u8> {
        vec![0; 32]
    }
}

/// One execution RPC response, released explicitly after the test checks concurrent work.
pub struct GatedRpc {
    pub url: String,
    pub observed: oneshot::Receiver<()>,
    pub release: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl GatedRpc {
    pub async fn new(header: &Header) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let mut block = serde_json::to_value(header)?;
        block["hash"] = serde_json::to_value(header.hash_slow())?;
        block["transactions"] = serde_json::json!([]);
        block["uncles"] = serde_json::json!([]);
        let (seen_tx, observed) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = vec![];
            let id = loop {
                let mut buf = [0; 4096];
                let len = socket.read(&mut buf).await.unwrap();
                assert!(len > 0);
                received.extend_from_slice(&buf[..len]);
                if let Some(start) = received.windows(4).position(|v| v == b"\r\n\r\n") {
                    if let Ok(json) =
                        serde_json::from_slice::<serde_json::Value>(&received[start + 4..])
                    {
                        assert_eq!(json["method"], "eth_getBlockByHash");
                        break json["id"].clone();
                    }
                }
            };
            seen_tx.send(()).unwrap();
            release_rx.await.unwrap();
            let body = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": block}).to_string();
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        Ok(Self {
            url,
            observed,
            release: Some(release),
            task,
        })
    }
}

impl Drop for GatedRpc {
    fn drop(&mut self) {
        self.task.abort();
    }
}
