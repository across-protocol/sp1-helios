//! Regression tests for migration, competing finality writers and global-lock ownership.
//! Fixtures live here and in support; no production interface is exposed for these tests.
mod support;

use super::*;
use alloy::{consensus::Header, primitives::Address};
use helios_consensus_core::types::{
    BeaconBlockHeader, ExecutionPayloadHeader, ExecutionPayloadHeaderElectra,
    LightClientHeaderElectra, LightClientHeaderGloas,
};
use support::{GatedRpc, TestContext};

fn request(marker: u8) -> ProofRequest {
    ProofRequest {
        src_chain_contract_address: Address::repeat_byte(marker),
        src_chain_storage_slots: vec![],
        src_chain_block_number: 100,
        dst_chain_contract_from_head: 1,
        dst_chain_contract_from_header: B256::ZERO,
        vkey: B256::ZERO,
    }
}

fn electra(slot: u64, number: u64) -> LightClientHeader {
    LightClientHeader::Electra(LightClientHeaderElectra {
        beacon: BeaconBlockHeader {
            slot,
            body_root: B256::from(alloy_primitives::U256::from(number)),
            ..Default::default()
        },
        execution: ExecutionPayloadHeader::Electra(ExecutionPayloadHeaderElectra {
            block_number: number,
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn gloas(slot: u64, execution: &Header) -> LightClientHeader {
    LightClientHeader::Gloas(LightClientHeaderGloas {
        beacon: BeaconBlockHeader {
            slot,
            body_root: execution.hash_slow(),
            ..Default::default()
        },
        execution_block_hash: execution.hash_slow(),
        ..Default::default()
    })
}

async fn write_legacy_header(
    store: &mut RedisStore<String>,
    header: &LightClientHeader,
) -> Result<()> {
    redis::cmd("SET")
        .arg(store.finalized_header_key())
        .arg(serde_json::to_string(header)?)
        .query_async::<()>(&mut store.conn_manager)
        .await?;
    Ok(())
}

#[tokio::test]
async fn requests_never_fetch_or_backfill_missing_execution_data() -> Result<()> {
    let execution = Header {
        number: 10,
        ..Default::default()
    };
    let mut rpc = GatedRpc::new(&execution).await?;
    let mut ctx = TestContext::new(&rpc.url).await?;
    write_legacy_header(&mut ctx.store, &gloas(320, &execution)).await?;
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        ctx.service.request_proof(request(1)),
    )
    .await?
    .unwrap_err();
    assert!(error.to_string().contains("not ready"));
    for status in [ProofRequestStatus::Success, ProofRequestStatus::Generating] {
        let known = request(2);
        let mut state = ProofRequestState::<String>::new(known.clone());
        state.status = status.clone();
        ctx.store
            .set_proof_state(&ProofId::new(&known), &state)
            .await?;
        assert_eq!(ctx.service.request_proof(known).await?.1, status);
    }
    assert!(matches!(
        rpc.observed.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert!(ctx
        .store
        .read_finalized_header_and_block_number()
        .await?
        .unwrap()
        .block_number
        .is_none());
    Ok(())
}

#[tokio::test]
async fn finality_loop_migrates_an_unchanged_header_without_holding_the_lock_during_rpc(
) -> Result<()> {
    let execution = Header {
        number: 10,
        ..Default::default()
    };
    let mut rpc = GatedRpc::new(&execution).await?;
    let mut ctx = TestContext::new(&rpc.url).await?;
    let header = gloas(320, &execution);
    write_legacy_header(&mut ctx.store, &header).await?;
    // Ignore the unbound number written by the previous implementation.
    redis::cmd("SET")
        .arg("finality-test:finalized_header:execution_block_number")
        .arg(999)
        .query_async::<()>(&mut ctx.store.conn_manager)
        .await?;
    let mut worker = ctx.service.clone();
    let update = tokio::spawn(async move { worker.process_new_finalized_header(header).await });
    tokio::time::timeout(Duration::from_secs(1), &mut rpc.observed).await??;
    let token = ctx
        .store
        .acquire_global_lock()
        .await?
        .expect("RPC must not hold the lock");
    let contender = ctx.service.request_proof(request(3)).await.unwrap_err();
    assert!(matches!(contender, ProofServiceError::LockContention(_)));
    let owner: String = redis::cmd("GET")
        .arg(ctx.store.global_lock_key())
        .query_async(&mut ctx.store.conn_manager)
        .await?;
    assert_eq!(owner, token, "a contender must not release our lock");
    ctx.store.release_global_lock(&token).await;
    rpc.release.take().unwrap().send(()).unwrap();
    assert!(update.await??);
    let stored = ctx
        .store
        .read_finalized_header_and_block_number()
        .await?
        .unwrap();
    assert_eq!(stored.header.beacon().slot, 320);
    assert_eq!(stored.block_number, Some(10));
    assert_eq!(
        ctx.service.request_proof(request(4)).await?.1,
        ProofRequestStatus::WaitingForFinality
    );
    Ok(())
}

#[tokio::test]
async fn a_late_execution_response_cannot_replace_a_newer_finalized_header() -> Result<()> {
    let execution = Header {
        number: 10,
        ..Default::default()
    };
    let mut rpc = GatedRpc::new(&execution).await?;
    let mut ctx = TestContext::new(&rpc.url).await?;
    let older = gloas(320, &execution);
    write_legacy_header(&mut ctx.store, &older).await?;
    let mut worker = ctx.service.clone();
    let update = tokio::spawn(async move { worker.process_new_finalized_header(older).await });
    tokio::time::timeout(Duration::from_secs(1), &mut rpc.observed).await??;

    // Another finality worker has already authenticated this newer execution header.
    let newer = gloas(
        640,
        &Header {
            number: 20,
            ..Default::default()
        },
    );
    let token = ctx.store.acquire_global_lock().await?.unwrap();
    assert!(
        ctx.service
            .process_new_finalized_header_locked(newer, 20)
            .await?
    );
    ctx.store.release_global_lock(&token).await;
    rpc.release.take().unwrap().send(()).unwrap();
    assert!(update.await??);

    let stored = ctx
        .store
        .read_finalized_header_and_block_number()
        .await?
        .unwrap();
    assert_eq!(stored.header.beacon().slot, 640);
    assert_eq!(stored.block_number, Some(20));
    Ok(())
}

#[tokio::test]
async fn a_legacy_writer_invalidates_the_cache_and_the_loop_repairs_it_without_new_finality(
) -> Result<()> {
    let mut ctx = TestContext::new("http://127.0.0.1:1").await?;
    assert!(
        ctx.service
            .process_new_finalized_header(electra(320, 10))
            .await?
    );
    let newer = electra(640, 20);
    write_legacy_header(&mut ctx.store, &newer).await?;
    assert!(ctx
        .store
        .read_finalized_header_and_block_number()
        .await?
        .unwrap()
        .block_number
        .is_none());
    assert!(ctx
        .service
        .request_proof(request(5))
        .await
        .unwrap_err()
        .to_string()
        .contains("not ready"));
    assert!(ctx.service.process_new_finalized_header(newer).await?);
    let stored = ctx
        .store
        .read_finalized_header_and_block_number()
        .await?
        .unwrap();
    assert_eq!(stored.header.beacon().slot, 640);
    assert_eq!(stored.block_number, Some(20));
    Ok(())
}

#[tokio::test]
async fn stale_publication_cannot_overwrite_a_newer_snapshot_or_proof_states() -> Result<()> {
    let mut ctx = TestContext::new("http://127.0.0.1:1").await?;
    let older = electra(320, 10);
    assert!(
        ctx.service
            .process_new_finalized_header(older.clone())
            .await?
    );
    let stale = ctx
        .store
        .read_finalized_header_and_block_number()
        .await?
        .unwrap();
    let req = request(6);
    let id = ProofId::new(&req);
    let mut state = ProofRequestState::<String>::new(req);
    state.status = ProofRequestStatus::WaitingForFinality;
    // Verify that a successful publish includes proof states and their expiration.
    assert!(
        ctx.store
            .update_finalized_header_and_proof_states(
                Some(&stale),
                &electra(640, 20),
                20,
                vec![(id, state.clone())],
            )
            .await?
    );
    let ttl: i64 = redis::cmd("TTL")
        .arg(ctx.store.proof_state_key(&id))
        .query_async(&mut ctx.store.conn_manager)
        .await?;
    assert!(ttl > 0);
    state.status = ProofRequestStatus::Generating;
    assert!(
        !ctx.store
            .update_finalized_header_and_proof_states(Some(&stale), &older, 10, vec![(id, state)],)
            .await?
    );
    let stored = ctx
        .store
        .read_finalized_header_and_block_number()
        .await?
        .unwrap();
    assert_eq!(stored.header.beacon().slot, 640);
    assert_eq!(stored.block_number, Some(20));
    assert_eq!(
        ctx.store.get_proof_state(&id).await?.unwrap().status,
        ProofRequestStatus::WaitingForFinality
    );
    // A lagging worker must not move the header backwards either.
    assert!(ctx.service.process_new_finalized_header(older).await?);
    assert_eq!(
        ctx.store
            .read_finalized_header()
            .await?
            .unwrap()
            .beacon()
            .slot,
        640
    );
    Ok(())
}

#[tokio::test]
async fn an_expired_lock_owner_cannot_release_its_successor() -> Result<()> {
    let mut ctx = TestContext::new("http://127.0.0.1:1").await?;
    let first = ctx.store.acquire_global_lock().await?.unwrap();
    // Expire immediately, without a timing-dependent sleep.
    redis::cmd("PEXPIRE")
        .arg(ctx.store.global_lock_key())
        .arg(0)
        .query_async::<i64>(&mut ctx.store.conn_manager)
        .await?;
    let second = ctx.store.acquire_global_lock().await?.unwrap();
    ctx.store.release_global_lock(&first).await;
    assert!(ctx.store.acquire_global_lock().await?.is_none());
    ctx.store.release_global_lock(&second).await;
    assert!(ctx.store.acquire_global_lock().await?.is_some());
    Ok(())
}

#[tokio::test]
async fn conflicting_headers_at_the_same_slot_are_rejected() -> Result<()> {
    let mut ctx = TestContext::new("http://127.0.0.1:1").await?;
    assert!(
        ctx.service
            .process_new_finalized_header(electra(320, 10))
            .await?
    );
    assert!(ctx
        .service
        .process_new_finalized_header(electra(320, 20))
        .await
        .is_err());
    assert_eq!(
        ctx.store
            .read_finalized_header_and_block_number()
            .await?
            .unwrap()
            .block_number,
        Some(10)
    );
    Ok(())
}
