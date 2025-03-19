use alloy_primitives::B256;
use alloy_sol_types::sol;
use helios_consensus_core::consensus_spec::MainnetConsensusSpec;
use helios_consensus_core::types::Forks;
use helios_consensus_core::types::{FinalityUpdate, LightClientStore, Update};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct StorageSlot {
    pub address: B256,
    pub slot: B256,
    pub proof: Vec<B256>,
    pub expected_value: B256,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ProofInputs {
    pub sync_committee_updates: Vec<Update<MainnetConsensusSpec>>,
    pub finality_update: FinalityUpdate<MainnetConsensusSpec>,
    pub expected_current_slot: u64,
    pub store: LightClientStore<MainnetConsensusSpec>,
    pub genesis_root: B256,
    pub forks: Forks,
    pub storage_slots: Vec<StorageSlot>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct ExecutionStateProof {
    #[serde(rename = "executionStateRoot")]
    pub execution_state_root: B256,
    #[serde(rename = "executionStateBranch")]
    pub execution_state_branch: Vec<B256>,
    pub gindex: String,
}

sol! {
    struct VerifiedStorageSlot {
        bytes32 key;
        bytes32 value;
        address contractAddress;
    }

    struct ProofOutputs {
        bytes32 executionStateRoot;
        bytes32 newHeader;
        bytes32 nextSyncCommitteeHash;
        uint256 newHead;
        bytes32 prevHeader;
        uint256 prevHead;
        bytes32 syncCommitteeHash;
        bytes32 startSyncCommitteeHash;
        VerifiedStorageSlot[] slots;
    }
}
