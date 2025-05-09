# SP1 Helios

## Overview

*This fork*:
Extends SP1 Helios to support proving source chain contract storage slots, in addition to consensus transitions. Messages can be stored on the source chain and proved on the destination chain, effectively creating a ZK-powered message bridge.

*Original*:
SP1 Helios verifies the consensus of a source chain in the execution environment of a destination chain. For example, you can run an SP1 Helios light client on Polygon that verifies Ethereum Mainnet's consensus.

[Docs](https://succinctlabs.github.io/sp1-helios/)
