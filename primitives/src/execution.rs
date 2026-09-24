//! Helpers anchoring execution-layer data to a beacon light-client header.
//!
//! From Gloas (EIP-7732 / ePBS) onwards, a `LightClientHeader` no longer carries the
//! full execution payload header — only a single `execution_block_hash`. The execution
//! `state_root` (the anchor for storage-slot proofs) and `block_number` must instead be
//! recovered from the RLP-encoded execution block header whose keccak256 equals that
//! trusted hash.
//!
//! These helpers implement that recovery uniformly for pre- and post-Gloas headers, so
//! the ZK program has a single execution-anchoring code path across the fork boundary.

use alloy_primitives::{keccak256, Address, Bloom, B256, U256};
use alloy_rlp::{Decodable, Error as RlpError, Header as RlpListHeader};
use helios_consensus_core::types::LightClientHeader;

/// Execution block header fields the ZK program needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionHeaderFields {
    pub state_root: B256,
    pub block_number: u64,
}

/// Returns the execution block hash a light-client header commits to.
///
/// - Capella..Electra: the block hash of the slot's own execution payload (the full
///   payload header is embedded and Merkle-proven against `body_root` during update
///   verification).
/// - Gloas (EIP-7732): `execution_block_hash`, proven against `body_root` via the
///   builder bid's `parent_block_hash`. Note the ePBS semantics: this is the newest
///   execution block *as of the start of the slot*, i.e. one payload behind the
///   pre-Gloas meaning.
///
/// Errors for pre-Capella headers, which carry no execution data.
pub fn execution_anchor_hash(header: &LightClientHeader) -> Result<B256, RlpError> {
    if let Ok(hash) = header.execution_block_hash() {
        // Gloas variant
        return Ok(*hash);
    }
    let execution = header
        .execution()
        .map_err(|_| RlpError::Custom("light-client header carries no execution data"))?;
    Ok(*execution.block_hash())
}

/// Decodes `state_root` (field 3) and `number` (field 8) from an RLP-encoded execution
/// block header.
///
/// The decode is positional over the leading fields only, which are fixed for every
/// fork since Frontier. Forks only ever *append* header fields (withdrawals_root, blob
/// gas fields, parent_beacon_block_root, requests_hash, EIP-7928's
/// block_access_list_hash, ...), so trailing content is deliberately ignored and new
/// forks cannot break this decode. Callers must verify the header's keccak256 against a
/// trusted hash — the hash, not this decode, is what authenticates the content.
pub fn decode_execution_header_fields(rlp: &[u8]) -> Result<ExecutionHeaderFields, RlpError> {
    let buf = &mut &rlp[..];
    let list = RlpListHeader::decode(buf)?;
    if !list.list {
        return Err(RlpError::Custom("execution header RLP is not a list"));
    }

    let _parent_hash = B256::decode(buf)?;
    let _ommers_hash = B256::decode(buf)?;
    let _beneficiary = Address::decode(buf)?;
    let state_root = B256::decode(buf)?;
    let _transactions_root = B256::decode(buf)?;
    let _receipts_root = B256::decode(buf)?;
    let _logs_bloom = Bloom::decode(buf)?;
    let _difficulty = U256::decode(buf)?;
    let block_number = u64::decode(buf)?;

    Ok(ExecutionHeaderFields {
        state_root,
        block_number,
    })
}

/// Verifies that `rlp` is the preimage of the execution block hash committed to by
/// `header`, and extracts the fields the ZK program needs from it.
///
/// This is the single execution-anchoring step shared by the ZK program (where failure
/// panics, so no proof exists) and the ZK-API (which runs the same check natively
/// before paying for proving).
pub fn verify_execution_header_rlp(
    header: &LightClientHeader,
    rlp: &[u8],
) -> Result<ExecutionHeaderFields, RlpError> {
    let anchor = execution_anchor_hash(header)?;
    if keccak256(rlp) != anchor {
        return Err(RlpError::Custom(
            "execution header RLP does not hash to the header's execution block hash",
        ));
    }
    decode_execution_header_fields(rlp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_rlp::Encodable;

    /// RLP-encode a minimal 9-field legacy header plus `extra` appended items, mimicking
    /// how post-Frontier forks extend the header.
    fn encode_header(state_root: B256, number: u64, extra_items: usize) -> Vec<u8> {
        let mut payload = Vec::new();
        B256::ZERO.encode(&mut payload); // parent_hash
        B256::ZERO.encode(&mut payload); // ommers_hash
        Address::ZERO.encode(&mut payload); // beneficiary
        state_root.encode(&mut payload); // state_root
        B256::ZERO.encode(&mut payload); // transactions_root
        B256::ZERO.encode(&mut payload); // receipts_root
        Bloom::ZERO.encode(&mut payload); // logs_bloom
        U256::ZERO.encode(&mut payload); // difficulty
        number.encode(&mut payload); // number
        for _ in 0..extra_items {
            B256::ZERO.encode(&mut payload); // appended fork fields
        }
        let mut out = Vec::new();
        RlpListHeader {
            list: true,
            payload_length: payload.len(),
        }
        .encode(&mut out);
        out.extend_from_slice(&payload);
        out
    }

    #[test]
    fn decodes_leading_fields_and_ignores_appended_ones() {
        let root = B256::repeat_byte(0xab);
        for extra in [0usize, 3, 8] {
            let rlp = encode_header(root, 123_456, extra);
            let fields = decode_execution_header_fields(&rlp).unwrap();
            assert_eq!(fields.state_root, root);
            assert_eq!(fields.block_number, 123_456);
        }
    }

    #[test]
    fn rejects_non_list_rlp() {
        let mut out = Vec::new();
        B256::ZERO.encode(&mut out);
        assert!(decode_execution_header_fields(&out).is_err());
    }
}
