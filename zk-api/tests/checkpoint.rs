//! Fallback behaviour of the checkpoint fetch against unresponsive consensus RPCs.
//!
//! Each test stands up local TCP listeners standing in for beacon API endpoints. The primary
//! is tried first; once it times out, the backups are raced and the healthy one must answer.
//! When every endpoint stalls the call must still fail within the configured timeouts
//! instead of hanging.

use alloy_primitives::{b256, B256};
use sp1_helios_api::consensus_client::checkpoint::fetch_checkpoint_from;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const SLOT: u64 = 11_343_904;
const ROOT: B256 = b256!("0x1111111111111111111111111111111111111111111111111111111111111111");
const TIMEOUT: Duration = Duration::from_millis(500);

/// Upper bound on a whole test, so a regression that removes the request timeout fails
/// here instead of hanging the test binary.
const TEST_DEADLINE: Duration = Duration::from_secs(30);

/// Accepts connections and reads the request but never writes a response.
async fn stalled_before_headers() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                // Hold the connection open until the client gives up.
                while socket.read(&mut buf).await.unwrap_or(0) > 0 {}
            });
        }
    });
    url
}

/// Sends a 200 with a Content-Length larger than the bytes it ever writes, so the
/// request succeeds but body decoding stalls.
async fn stalled_during_body() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let partial = b"{\"data\":{\"root\":\"0x";
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                    partial.len() + 1024
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(partial).await;
                // Never send the rest; hold the connection open until the client gives up.
                while socket.read(&mut buf).await.unwrap_or(0) > 0 {}
            });
        }
    });
    url
}

/// Serves a valid beacon header response for every request.
async fn healthy() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let body = format!("{{\"data\":{{\"root\":\"{ROOT}\"}}}}");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    url
}

async fn assert_falls_back_to_backup(primary: String) {
    let backup = healthy().await;
    let started = Instant::now();
    let root = tokio::time::timeout(
        TEST_DEADLINE,
        fetch_checkpoint_from(&[primary, backup], SLOT, TIMEOUT),
    )
    .await
    .expect("fetch did not complete before the test deadline")
    .expect("healthy backup should answer");
    let elapsed = started.elapsed();

    assert_eq!(root, ROOT);
    assert!(
        elapsed >= TIMEOUT,
        "primary was not given the full timeout ({elapsed:?})"
    );
}

#[tokio::test]
async fn falls_back_when_primary_never_responds() {
    assert_falls_back_to_backup(stalled_before_headers().await).await;
}

#[tokio::test]
async fn falls_back_when_primary_stalls_mid_body() {
    assert_falls_back_to_backup(stalled_during_body().await).await;
}

#[tokio::test]
async fn answers_from_primary_without_touching_backups() {
    let primary = healthy().await;
    let backup = stalled_before_headers().await;
    let started = Instant::now();
    let root = tokio::time::timeout(
        TEST_DEADLINE,
        fetch_checkpoint_from(&[primary, backup], SLOT, TIMEOUT),
    )
    .await
    .expect("fetch did not complete before the test deadline")
    .expect("healthy primary should answer");

    assert_eq!(root, ROOT);
    assert!(
        started.elapsed() < TIMEOUT,
        "a healthy primary should not wait on the backup timeout"
    );
}

#[tokio::test]
async fn errors_when_every_rpc_stalls() {
    let rpcs = [stalled_before_headers().await, stalled_during_body().await];
    let started = Instant::now();
    let err = tokio::time::timeout(TEST_DEADLINE, fetch_checkpoint_from(&rpcs, SLOT, TIMEOUT))
        .await
        .expect("fetch did not complete before the test deadline")
        .expect_err("no endpoint answered");
    let elapsed = started.elapsed();

    // Primary times out, then the backups are raced and time out too.
    assert!(
        elapsed >= TIMEOUT * 2,
        "primary and backups were not each given the full timeout ({elapsed:?})"
    );
    assert!(
        err.to_string().contains(&rpcs[1]),
        "error should name the failed backup: {err}"
    );
}

#[tokio::test]
async fn errors_when_no_rpcs_configured() {
    let err = fetch_checkpoint_from(&[], SLOT, TIMEOUT)
        .await
        .expect_err("empty RPC list");
    assert!(err.to_string().contains("CONSENSUS_RPCS_LIST"), "{err}");
}
