# SP1 Helios

## Overview

_This fork_:
Extends SP1 Helios to support proving source chain contract storage slots, in addition to consensus transitions. Messages can be stored on the source chain and proved on the destination chain, effectively creating a ZK-powered message bridge.

_Original_:
SP1 Helios verifies the consensus of a source chain in the execution environment of a destination chain. For example, you can run an SP1 Helios light client on Polygon that verifies Ethereum Mainnet's consensus.

[Docs](https://succinctlabs.github.io/sp1-helios/)

## Deploying a new SP1Helios contract

```
# (from root) Load environment variables
source .env

cd contracts

# Install dependencies
forge install

# Deploy contract
forge script script/Deploy.s.sol --ffi --rpc-url bsc --broadcast --verify
```

You can also pass the RPC URL and etherscan API key as arguments to the script:

```
forge script script/Deploy.s.sol --ffi --rpc-url $DEST_RPC_URL --etherscan-api-key $ETHERSCAN_API_KEY --broadcast --verify
```
