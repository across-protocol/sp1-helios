//! Execution-field anchoring against synthetic pre-Gloas and Gloas light-client headers.

use alloy_consensus::Header;
use alloy_primitives::B256;
use helios_consensus_core::types::{
    ExecutionPayloadHeader, ExecutionPayloadHeaderElectra, LightClientHeader,
    LightClientHeaderElectra, LightClientHeaderGloas,
};
use sp1_helios_primitives::execution::verified_execution_fields;

#[test]
fn gloas_requires_a_witness_matching_the_committed_hash() {
    let execution = Header {
        state_root: B256::repeat_byte(1),
        receipts_root: B256::repeat_byte(2),
        number: 42,
        block_access_list_hash: Some(B256::repeat_byte(3)),
        slot_number: Some(96),
        ..Default::default()
    };
    let header = LightClientHeader::Gloas(LightClientHeaderGloas {
        execution_block_hash: execution.hash_slow(),
        ..Default::default()
    });
    assert!(header.execution().is_err());
    assert!(verified_execution_fields(&header, None).is_err());

    let fields = verified_execution_fields(&header, Some(&execution)).unwrap();
    assert_eq!(fields.state_root, execution.state_root);
    assert_eq!(fields.block_number, execution.number);
    assert_eq!(fields.block_hash, execution.hash_slow());

    for tampered in [
        Header {
            state_root: B256::ZERO,
            ..execution.clone()
        },
        Header {
            number: 43,
            ..execution.clone()
        },
        Header {
            block_access_list_hash: None,
            ..execution.clone()
        },
        Header {
            slot_number: None,
            ..execution.clone()
        },
    ] {
        assert!(verified_execution_fields(&header, Some(&tampered)).is_err());
    }
}

#[test]
fn pre_gloas_uses_the_embedded_payload_header() {
    let execution = ExecutionPayloadHeaderElectra {
        state_root: B256::repeat_byte(1),
        block_number: 42,
        block_hash: B256::repeat_byte(3),
        ..Default::default()
    };
    let header = LightClientHeader::Electra(LightClientHeaderElectra {
        execution: ExecutionPayloadHeader::Electra(execution),
        ..Default::default()
    });

    let fields = verified_execution_fields(&header, None).unwrap();
    assert_eq!(fields.state_root, B256::repeat_byte(1));
    assert_eq!(fields.block_number, 42);
    assert_eq!(fields.block_hash, B256::repeat_byte(3));
}
