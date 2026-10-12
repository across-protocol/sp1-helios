//! Authenticate execution-layer fields against a verified light-client header.
//!
//! Up to Fulu, a `LightClientHeader` embeds the full execution payload header, which update
//! verification already proves against `body_root`. From Gloas (EIP-7732 / ePBS) onwards the
//! header only commits to an execution block hash, so the execution `state_root` (the anchor
//! for storage-slot proofs) and `block_number` must be recovered from a witness header that
//! hashes to that commitment.

use alloy_consensus::Header;
use alloy_primitives::B256;
use anyhow::{ensure, Context, Result};
use helios_consensus_core::types::LightClientHeader;

/// Execution fields authenticated by the finalized consensus header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionFields {
    /// The execution state trie root.
    pub state_root: B256,
    /// The execution block number.
    pub block_number: u64,
    /// The execution block hash.
    pub block_hash: B256,
    /// The execution receipts trie root.
    pub receipts_root: B256,
}

/// Extracts the execution fields a verified light-client header anchors to.
///
/// - Pre-Gloas: read straight from the embedded execution payload header; `execution_header`
///   is ignored.
/// - Gloas: `execution_header` is required and must hash to the header's
///   `execution_block_hash` before any field is consumed. Note the ePBS semantics: this is the
///   builder bid's parent block, i.e. the newest execution block as of the start of the slot.
///
/// This is the single execution-anchoring step shared by the ZK program (where failure panics,
/// so no proof exists) and the ZK-API (which runs the same check natively before proving).
pub fn verified_execution_fields(
    finalized_header: &LightClientHeader,
    execution_header: Option<&Header>,
) -> Result<ExecutionFields> {
    if let LightClientHeader::Gloas(header) = finalized_header {
        let execution = execution_header.context("Gloas execution header witness is missing")?;
        let block_hash = execution.hash_slow();
        ensure!(
            block_hash == header.execution_block_hash,
            "Execution header hash mismatch: expected {}, got {}",
            header.execution_block_hash,
            block_hash
        );
        return Ok(ExecutionFields {
            state_root: execution.state_root,
            block_number: execution.number,
            block_hash,
            receipts_root: execution.receipts_root,
        });
    }

    let execution = finalized_header
        .execution()
        .map_err(|_| anyhow::anyhow!("Finalized header has no execution commitment"))?;
    Ok(ExecutionFields {
        state_root: *execution.state_root(),
        block_number: *execution.block_number(),
        block_hash: *execution.block_hash(),
        receipts_root: *execution.receipts_root(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_consensus_core::types::{
        ExecutionPayloadHeader, ExecutionPayloadHeaderElectra, LightClientHeaderElectra,
        LightClientHeaderGloas,
    };

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
        assert_eq!(fields.receipts_root, execution.receipts_root);

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
            receipts_root: B256::repeat_byte(2),
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
        assert_eq!(fields.receipts_root, B256::repeat_byte(2));
        assert_eq!(fields.block_number, 42);
        assert_eq!(fields.block_hash, B256::repeat_byte(3));
    }
}
