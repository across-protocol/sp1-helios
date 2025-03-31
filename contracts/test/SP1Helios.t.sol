// SPDX-License-Identifier: MIT
pragma solidity ^0.8.22;

import {Test, console2} from "forge-std/Test.sol";
import {SP1Helios} from "../src/SP1Helios.sol";
import {SP1MockVerifier} from "@sp1-contracts/SP1MockVerifier.sol";
import {ISP1Verifier} from "@sp1-contracts/ISP1Verifier.sol";

contract SP1HeliosTest is Test {
    SP1Helios helios;
    SP1MockVerifier mockVerifier;
    address guardian = address(0x1);

    // Constants for test setup
    bytes32 constant GENESIS_VALIDATORS_ROOT = bytes32(uint256(1));
    uint256 constant GENESIS_TIME = 1606824023; // Dec 1, 2020
    uint256 constant SECONDS_PER_SLOT = 12;
    uint256 constant SLOTS_PER_EPOCH = 32;
    uint256 constant SLOTS_PER_PERIOD = 8192; // 256 epochs
    uint256 constant SOURCE_CHAIN_ID = 1; // Ethereum mainnet
    bytes32 constant INITIAL_HEADER = bytes32(uint256(2));
    bytes32 constant INITIAL_EXECUTION_STATE_ROOT = bytes32(uint256(3));
    bytes32 constant INITIAL_SYNC_COMMITTEE_HASH = bytes32(uint256(4));
    bytes32 constant HELIOS_PROGRAM_VKEY = bytes32(uint256(5));
    uint256 constant INITIAL_HEAD = 100;

    function setUp() public {
        mockVerifier = new SP1MockVerifier();

        SP1Helios.InitParams memory params = SP1Helios.InitParams({
            executionStateRoot: INITIAL_EXECUTION_STATE_ROOT,
            genesisTime: GENESIS_TIME,
            genesisValidatorsRoot: GENESIS_VALIDATORS_ROOT,
            guardian: guardian,
            head: INITIAL_HEAD,
            header: INITIAL_HEADER,
            heliosProgramVkey: HELIOS_PROGRAM_VKEY,
            secondsPerSlot: SECONDS_PER_SLOT,
            slotsPerEpoch: SLOTS_PER_EPOCH,
            slotsPerPeriod: SLOTS_PER_PERIOD,
            sourceChainId: SOURCE_CHAIN_ID,
            syncCommitteeHash: INITIAL_SYNC_COMMITTEE_HASH,
            verifier: address(mockVerifier)
        });

        helios = new SP1Helios(params);
    }

    function testInitialization() public view {
        assertEq(helios.GENESIS_VALIDATORS_ROOT(), GENESIS_VALIDATORS_ROOT);
        assertEq(helios.GENESIS_TIME(), GENESIS_TIME);
        assertEq(helios.SECONDS_PER_SLOT(), SECONDS_PER_SLOT);
        assertEq(helios.SLOTS_PER_EPOCH(), SLOTS_PER_EPOCH);
        assertEq(helios.SLOTS_PER_PERIOD(), SLOTS_PER_PERIOD);
        assertEq(helios.SOURCE_CHAIN_ID(), SOURCE_CHAIN_ID);
        assertEq(helios.heliosProgramVkey(), HELIOS_PROGRAM_VKEY);
        assertEq(helios.head(), INITIAL_HEAD);
        assertEq(helios.headers(INITIAL_HEAD), INITIAL_HEADER);
        assertEq(helios.executionStateRoots(INITIAL_HEAD), INITIAL_EXECUTION_STATE_ROOT);
        assertEq(
            helios.syncCommittees(helios.getSyncCommitteePeriod(INITIAL_HEAD)),
            INITIAL_SYNC_COMMITTEE_HASH
        );
        assertEq(helios.guardian(), guardian);
        assertEq(helios.verifier(), address(mockVerifier));
    }

    function testGetSyncCommitteePeriod() public view {
        uint256 slot = 16384; // 2 * SLOTS_PER_PERIOD
        assertEq(helios.getSyncCommitteePeriod(slot), 2);

        slot = 8191; // SLOTS_PER_PERIOD - 1
        assertEq(helios.getSyncCommitteePeriod(slot), 0);

        slot = 8192; // SLOTS_PER_PERIOD
        assertEq(helios.getSyncCommitteePeriod(slot), 1);
    }

    function testGetCurrentEpoch() public view {
        // Initial head is 100
        assertEq(helios.getCurrentEpoch(), 3); // 100 / 32 = 3.125, truncated to 3
    }

    function testSlotTimestamp() public view {
        uint256 slot = 1000;
        assertEq(helios.slotTimestamp(slot), GENESIS_TIME + slot * SECONDS_PER_SLOT);
    }

    function validateSlotMath() public view {
        uint256 slot1 = 5000000;
        assertEq(helios.slotTimestamp(slot1), 1666824023);

        uint256 slot2 = 10000000;
        assertEq(helios.slotTimestamp(slot2), 1726824023);

        assertEq(
            helios.slotTimestamp(slot2) - helios.slotTimestamp(slot1),
            (slot2 - slot1) * SECONDS_PER_SLOT
        );
    }

    function testHeadTimestamp() public view {
        assertEq(helios.headTimestamp(), GENESIS_TIME + INITIAL_HEAD * SECONDS_PER_SLOT);
    }

    function testComputeStorageKey() public view {
        uint256 blockNumber = 123;
        address contractAddress = address(0xabc);
        bytes32 slot = bytes32(uint256(456));

        bytes32 expectedKey = keccak256(abi.encodePacked(blockNumber, contractAddress, slot));
        assertEq(helios.computeStorageKey(blockNumber, contractAddress, slot), expectedKey);
    }

    function testGetStorageSlot() public {
        uint256 blockNumber = 123;
        address contractAddress = address(0xabc);
        bytes32 slot = bytes32(uint256(456));
        bytes32 value = bytes32(uint256(789));

        bytes32 storageKey = helios.computeStorageKey(blockNumber, contractAddress, slot);

        // Simulate an update that would set this storage value
        vm.startPrank(address(helios));
        vm.store(
            address(helios),
            keccak256(abi.encodePacked(storageKey, uint256(4))), // 4 is the slot of storageValues mapping in the contract
            value
        );
        vm.stopPrank();

        assertEq(helios.getStorageSlot(blockNumber, contractAddress, slot), value);
    }

    function testUpdateHeliosProgramVkey() public {
        bytes32 newVkey = bytes32(uint256(123));

        // Should revert when called by non-guardian
        vm.expectRevert("Caller is not the guardian");
        helios.updateHeliosProgramVkey(newVkey);

        // Should succeed when called by guardian
        vm.prank(guardian);
        helios.updateHeliosProgramVkey(newVkey);

        assertEq(helios.heliosProgramVkey(), newVkey);
    }

    function testUpdate() public {
        uint256 newHead = INITIAL_HEAD + 100;
        bytes32 newHeader = bytes32(uint256(10));
        bytes32 newExecutionStateRoot = bytes32(uint256(11));
        bytes32 syncCommitteeHash = INITIAL_SYNC_COMMITTEE_HASH;
        bytes32 nextSyncCommitteeHash = bytes32(uint256(12));

        // Create multiple storage slots to be set
        SP1Helios.StorageSlot[] memory slots = new SP1Helios.StorageSlot[](3);

        // Slot 1: ERC20 token balance
        slots[0] = SP1Helios.StorageSlot({
            key: bytes32(uint256(100)),
            value: bytes32(uint256(200)),
            contractAddress: address(0xdef)
        });

        // Slot 2: NFT ownership mapping
        slots[1] = SP1Helios.StorageSlot({
            key: keccak256(abi.encode(address(0xabc), uint256(123))),
            value: bytes32(uint256(1)),
            contractAddress: address(0xbbb)
        });

        // Slot 3: Governance proposal state
        slots[2] = SP1Helios.StorageSlot({
            key: keccak256(abi.encode("proposal", uint256(5))),
            value: bytes32(uint256(2)), // 2 might represent "approved" state
            contractAddress: address(0xccc)
        });

        // Create proof outputs
        SP1Helios.ProofOutputs memory po = SP1Helios.ProofOutputs({
            executionStateRoot: newExecutionStateRoot,
            newHeader: newHeader,
            nextSyncCommitteeHash: nextSyncCommitteeHash,
            newHead: newHead,
            prevHeader: INITIAL_HEADER,
            prevHead: INITIAL_HEAD,
            syncCommitteeHash: syncCommitteeHash,
            startSyncCommitteeHash: INITIAL_SYNC_COMMITTEE_HASH,
            slots: slots
        });

        bytes memory publicValues = abi.encode(po);
        bytes memory proof = new bytes(0); // MockVerifier will accept empty proof

        // Set block timestamp to be valid for the update
        vm.warp(helios.slotTimestamp(INITIAL_HEAD) + 1 hours);

        // Test successful update
        vm.expectEmit(true, true, false, true);
        emit SP1Helios.HeadUpdate(newHead, newHeader);

        // Expect events for all storage slots
        for (uint256 i = 0; i < slots.length; i++) {
            vm.expectEmit(true, true, false, true);
            emit SP1Helios.StorageSlotVerified(
                newHead, slots[i].key, slots[i].value, slots[i].contractAddress
            );
        }

        helios.update(proof, publicValues, INITIAL_HEAD);

        // Verify state updates
        assertEq(helios.head(), newHead);
        assertEq(helios.headers(newHead), newHeader);
        assertEq(helios.executionStateRoots(newHead), newExecutionStateRoot);

        // Verify all storage slots were set correctly
        for (uint256 i = 0; i < slots.length; i++) {
            assertEq(
                helios.getStorageSlot(newHead, slots[i].contractAddress, slots[i].key),
                slots[i].value,
                string(abi.encodePacked("Storage slot ", i, " was not set correctly"))
            );
        }

        // Verify sync committee updates
        uint256 period = helios.getSyncCommitteePeriod(newHead);
        uint256 nextPeriod = period + 1;
        assertEq(helios.syncCommittees(nextPeriod), nextSyncCommitteeHash);
    }

    function testUpdateWithNonexistentFromHead() public {
        uint256 nonExistentHead = 999999;

        SP1Helios.StorageSlot[] memory slots = new SP1Helios.StorageSlot[](0);

        SP1Helios.ProofOutputs memory po = SP1Helios.ProofOutputs({
            executionStateRoot: bytes32(0),
            newHeader: bytes32(0),
            nextSyncCommitteeHash: bytes32(0),
            newHead: nonExistentHead + 1,
            prevHeader: bytes32(0),
            prevHead: nonExistentHead,
            syncCommitteeHash: bytes32(0),
            startSyncCommitteeHash: bytes32(0),
            slots: slots
        });

        bytes memory publicValues = abi.encode(po);
        bytes memory proof = new bytes(0);

        vm.expectRevert(
            abi.encodeWithSelector(SP1Helios.PreviousHeadNotSet.selector, nonExistentHead)
        );
        helios.update(proof, publicValues, nonExistentHead);
    }

    function testUpdateWithTooOldFromHead() public {
        // Set block timestamp to be more than MAX_SLOT_AGE after the initial head timestamp
        vm.warp(helios.slotTimestamp(INITIAL_HEAD) + helios.MAX_SLOT_AGE() + 1);

        SP1Helios.StorageSlot[] memory slots = new SP1Helios.StorageSlot[](0);

        SP1Helios.ProofOutputs memory po = SP1Helios.ProofOutputs({
            executionStateRoot: bytes32(0),
            newHeader: bytes32(0),
            nextSyncCommitteeHash: bytes32(0),
            newHead: INITIAL_HEAD + 1,
            prevHeader: INITIAL_HEADER,
            prevHead: INITIAL_HEAD,
            syncCommitteeHash: bytes32(0),
            startSyncCommitteeHash: INITIAL_SYNC_COMMITTEE_HASH,
            slots: slots
        });

        bytes memory publicValues = abi.encode(po);
        bytes memory proof = new bytes(0);

        vm.expectRevert(abi.encodeWithSelector(SP1Helios.PreviousHeadTooOld.selector, INITIAL_HEAD));
        helios.update(proof, publicValues, INITIAL_HEAD);
    }

    function testUpdateWithNewHeadBehindFromHead() public {
        uint256 newHead = INITIAL_HEAD - 1; // Less than INITIAL_HEAD

        SP1Helios.StorageSlot[] memory slots = new SP1Helios.StorageSlot[](0);

        SP1Helios.ProofOutputs memory po = SP1Helios.ProofOutputs({
            executionStateRoot: bytes32(0),
            newHeader: bytes32(0),
            nextSyncCommitteeHash: bytes32(0),
            newHead: newHead,
            prevHeader: INITIAL_HEADER,
            prevHead: INITIAL_HEAD,
            syncCommitteeHash: bytes32(0),
            startSyncCommitteeHash: INITIAL_SYNC_COMMITTEE_HASH,
            slots: slots
        });

        bytes memory publicValues = abi.encode(po);
        bytes memory proof = new bytes(0);

        // Set block timestamp to be valid for the update
        vm.warp(helios.slotTimestamp(INITIAL_HEAD) + 1 hours);

        vm.expectRevert(abi.encodeWithSelector(SP1Helios.SlotBehindHead.selector, newHead));
        helios.update(proof, publicValues, INITIAL_HEAD);
    }

    function testUpdateWithIncorrectSyncCommitteeHash() public {
        bytes32 wrongSyncCommitteeHash = bytes32(uint256(999));

        SP1Helios.StorageSlot[] memory slots = new SP1Helios.StorageSlot[](0);

        SP1Helios.ProofOutputs memory po = SP1Helios.ProofOutputs({
            executionStateRoot: bytes32(0),
            newHeader: bytes32(0),
            nextSyncCommitteeHash: bytes32(0),
            newHead: INITIAL_HEAD + 1,
            prevHeader: INITIAL_HEADER,
            prevHead: INITIAL_HEAD,
            syncCommitteeHash: bytes32(0),
            startSyncCommitteeHash: wrongSyncCommitteeHash, // Wrong hash
            slots: slots
        });

        bytes memory publicValues = abi.encode(po);
        bytes memory proof = new bytes(0);

        // Set block timestamp to be valid for the update
        vm.warp(helios.slotTimestamp(INITIAL_HEAD) + 1 hours);

        vm.expectRevert(
            abi.encodeWithSelector(
                SP1Helios.SyncCommitteeStartMismatch.selector,
                wrongSyncCommitteeHash,
                INITIAL_SYNC_COMMITTEE_HASH
            )
        );
        helios.update(proof, publicValues, INITIAL_HEAD);
    }

    function testUpdateWithConflictingHeader() public {
        uint256 newHead = INITIAL_HEAD + 100;
        bytes32 existingHeader = bytes32(uint256(888));
        bytes32 conflictingHeader = bytes32(uint256(999));

        // Set up an existing header
        vm.store(
            address(helios),
            keccak256(abi.encode(newHead, uint256(1))), // 1 is the slot of headers mapping
            existingHeader
        );

        SP1Helios.StorageSlot[] memory slots = new SP1Helios.StorageSlot[](0);

        SP1Helios.ProofOutputs memory po = SP1Helios.ProofOutputs({
            executionStateRoot: bytes32(0),
            newHeader: conflictingHeader, // Different from existing header
            nextSyncCommitteeHash: bytes32(0),
            newHead: newHead,
            prevHeader: INITIAL_HEADER,
            prevHead: INITIAL_HEAD,
            syncCommitteeHash: bytes32(0),
            startSyncCommitteeHash: INITIAL_SYNC_COMMITTEE_HASH,
            slots: slots
        });

        bytes memory publicValues = abi.encode(po);
        bytes memory proof = new bytes(0);

        vm.warp(helios.slotTimestamp(INITIAL_HEAD) + 1 hours); // Set valid timestamp

        vm.expectRevert(abi.encodeWithSelector(SP1Helios.InvalidHeaderRoot.selector, newHead));
        helios.update(proof, publicValues, INITIAL_HEAD);
    }

    function testUpdateWithConflictingStateRoot() public {
        uint256 newHead = INITIAL_HEAD + 100;
        bytes32 existingStateRoot = bytes32(uint256(888));
        bytes32 conflictingStateRoot = bytes32(uint256(999));

        // Set up an existing state root
        vm.store(
            address(helios),
            keccak256(abi.encode(newHead, uint256(2))), // 2 is the slot of executionStateRoots mapping
            existingStateRoot
        );

        SP1Helios.StorageSlot[] memory slots = new SP1Helios.StorageSlot[](0);

        SP1Helios.ProofOutputs memory po = SP1Helios.ProofOutputs({
            executionStateRoot: conflictingStateRoot, // Different from existing state root
            newHeader: bytes32(uint256(123)),
            nextSyncCommitteeHash: bytes32(0),
            newHead: newHead,
            prevHeader: INITIAL_HEADER,
            prevHead: INITIAL_HEAD,
            syncCommitteeHash: bytes32(0),
            startSyncCommitteeHash: INITIAL_SYNC_COMMITTEE_HASH,
            slots: slots
        });

        bytes memory publicValues = abi.encode(po);
        bytes memory proof = new bytes(0);

        vm.warp(helios.slotTimestamp(INITIAL_HEAD) + 1 hours); // Set valid timestamp

        vm.expectRevert(abi.encodeWithSelector(SP1Helios.InvalidStateRoot.selector, newHead));
        helios.update(proof, publicValues, INITIAL_HEAD);
    }

    function testUpdateWithExistingSyncCommittee() public {
        uint256 newHead = INITIAL_HEAD + SLOTS_PER_PERIOD; // Put new head in the next period
        uint256 nextPeriod = helios.getSyncCommitteePeriod(newHead) + 1;
        bytes32 existingSyncCommitteeHash = bytes32(uint256(888));
        bytes32 newSyncCommitteeHash = bytes32(uint256(999));

        // Set up an existing sync committee for the next period
        vm.store(
            address(helios),
            keccak256(abi.encode(nextPeriod, uint256(3))), // 3 is the slot of syncCommittees mapping
            existingSyncCommitteeHash
        );

        SP1Helios.StorageSlot[] memory slots = new SP1Helios.StorageSlot[](0);

        SP1Helios.ProofOutputs memory po = SP1Helios.ProofOutputs({
            executionStateRoot: bytes32(uint256(123)),
            newHeader: bytes32(uint256(456)),
            nextSyncCommitteeHash: newSyncCommitteeHash, // Trying to set a different hash
            newHead: newHead,
            prevHeader: INITIAL_HEADER,
            prevHead: INITIAL_HEAD,
            syncCommitteeHash: INITIAL_SYNC_COMMITTEE_HASH,
            startSyncCommitteeHash: INITIAL_SYNC_COMMITTEE_HASH,
            slots: slots
        });

        bytes memory publicValues = abi.encode(po);
        bytes memory proof = new bytes(0);

        vm.warp(helios.slotTimestamp(INITIAL_HEAD) + 1 hours); // Set valid timestamp

        vm.expectRevert(
            abi.encodeWithSelector(SP1Helios.SyncCommitteeAlreadySet.selector, nextPeriod)
        );
        helios.update(proof, publicValues, INITIAL_HEAD);
    }

    function testUpdateThroughMultipleSyncCommittees() public {
        // We'll move forward by more than one sync committee period
        uint256 initialPeriod = helios.getSyncCommitteePeriod(INITIAL_HEAD);
        uint256 nextPeriod = initialPeriod + 1;
        uint256 futurePeriod = initialPeriod + 2;

        // First update values
        uint256 nextPeriodHead = INITIAL_HEAD + SLOTS_PER_PERIOD / 2; // Middle of next period
        bytes32 nextHeader = bytes32(uint256(10));
        bytes32 nextExecutionStateRoot = bytes32(uint256(11));
        bytes32 nextSyncCommitteeHash = bytes32(uint256(12));

        // Perform first update (to next period)
        performFirstUpdate(
            nextPeriodHead, nextHeader, nextExecutionStateRoot, nextSyncCommitteeHash, nextPeriod
        );

        // Future update values
        uint256 futurePeriodHead = INITIAL_HEAD + (SLOTS_PER_PERIOD * 2) - 10; // Close to end of second period
        bytes32 futureHeader = bytes32(uint256(20));
        bytes32 futureExecutionStateRoot = bytes32(uint256(21));
        bytes32 futureSyncCommitteeHash = bytes32(uint256(22));
        bytes32 futureNextSyncCommitteeHash = bytes32(uint256(13));

        // Perform second update (to future period)
        performSecondUpdate(
            nextPeriodHead,
            nextHeader,
            futurePeriodHead,
            futureHeader,
            futureExecutionStateRoot,
            futureSyncCommitteeHash,
            futureNextSyncCommitteeHash,
            futurePeriod
        );

        // Make sure we've gone through multiple periods
        assertNotEq(initialPeriod, helios.getSyncCommitteePeriod(futurePeriodHead));
        assertEq(futurePeriod, helios.getSyncCommitteePeriod(futurePeriodHead));
    }

    // Helper function for the first update in testUpdateThroughMultipleSyncCommittees
    function performFirstUpdate(
        uint256 nextPeriodHead,
        bytes32 nextHeader,
        bytes32 nextExecutionStateRoot,
        bytes32 nextSyncCommitteeHash,
        uint256 nextPeriod
    ) internal {
        SP1Helios.StorageSlot[] memory emptySlots = new SP1Helios.StorageSlot[](0);

        SP1Helios.ProofOutputs memory po1 = SP1Helios.ProofOutputs({
            executionStateRoot: nextExecutionStateRoot,
            newHeader: nextHeader,
            nextSyncCommitteeHash: nextSyncCommitteeHash, // For the next period
            newHead: nextPeriodHead,
            prevHeader: INITIAL_HEADER,
            prevHead: INITIAL_HEAD,
            syncCommitteeHash: INITIAL_SYNC_COMMITTEE_HASH,
            startSyncCommitteeHash: INITIAL_SYNC_COMMITTEE_HASH,
            slots: emptySlots
        });

        bytes memory publicValues1 = abi.encode(po1);
        bytes memory proof = new bytes(0);

        // Set block timestamp to be valid for the update
        vm.warp(helios.slotTimestamp(INITIAL_HEAD) + 1 hours);

        // Expect event emissions for head update and sync committee update
        vm.expectEmit(true, true, false, true);
        emit SP1Helios.HeadUpdate(nextPeriodHead, nextHeader);

        vm.expectEmit(true, true, false, true);
        emit SP1Helios.SyncCommitteeUpdate(nextPeriod, nextSyncCommitteeHash);

        helios.update(proof, publicValues1, INITIAL_HEAD);

        // Verify the updates
        assertEq(helios.head(), nextPeriodHead);
        assertEq(helios.headers(nextPeriodHead), nextHeader);
        assertEq(helios.executionStateRoots(nextPeriodHead), nextExecutionStateRoot);
        assertEq(helios.syncCommittees(nextPeriod), nextSyncCommitteeHash);
    }

    // Helper function for the second update in testUpdateThroughMultipleSyncCommittees
    function performSecondUpdate(
        uint256 prevHead,
        bytes32 prevHeader,
        uint256 newHead,
        bytes32 newHeader,
        bytes32 newExecutionStateRoot,
        bytes32 newSyncCommitteeHash,
        bytes32 nextSyncCommitteeHash,
        uint256 period
    ) internal {
        SP1Helios.StorageSlot[] memory emptySlots = new SP1Helios.StorageSlot[](0);

        SP1Helios.ProofOutputs memory po2 = SP1Helios.ProofOutputs({
            executionStateRoot: newExecutionStateRoot,
            newHeader: newHeader,
            nextSyncCommitteeHash: nextSyncCommitteeHash, // For the period after futurePeriod
            newHead: newHead,
            prevHeader: prevHeader,
            prevHead: prevHead,
            syncCommitteeHash: newSyncCommitteeHash,
            startSyncCommitteeHash: INITIAL_SYNC_COMMITTEE_HASH, // This must match the sync committee from the initial setup
            slots: emptySlots
        });

        bytes memory publicValues2 = abi.encode(po2);
        bytes memory proof = new bytes(0);

        // Set block timestamp to be valid for the next update
        vm.warp(helios.slotTimestamp(prevHead) + 1 hours);

        // Expect event emissions for the second update
        vm.expectEmit(true, true, false, true);
        emit SP1Helios.HeadUpdate(newHead, newHeader);

        vm.expectEmit(true, true, false, true);
        emit SP1Helios.SyncCommitteeUpdate(period, newSyncCommitteeHash);

        vm.expectEmit(true, true, false, true);
        emit SP1Helios.SyncCommitteeUpdate(period + 1, nextSyncCommitteeHash);

        helios.update(proof, publicValues2, prevHead);

        // Verify the second update
        assertEq(helios.head(), newHead);
        assertEq(helios.headers(newHead), newHeader);
        assertEq(helios.executionStateRoots(newHead), newExecutionStateRoot);
        assertEq(helios.syncCommittees(period), newSyncCommitteeHash);
        assertEq(helios.syncCommittees(period + 1), nextSyncCommitteeHash);
    }
}
