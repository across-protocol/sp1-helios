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
        });
    }

    let execution = finalized_header
        .execution()
        .map_err(|_| anyhow::anyhow!("Finalized header has no execution commitment"))?;
    Ok(ExecutionFields {
        state_root: *execution.state_root(),
        block_number: *execution.block_number(),
        block_hash: *execution.block_hash(),
    })
}
