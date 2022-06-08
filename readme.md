# Deep Space demo

[Deep Space](https://github.com/althea-net/deep_space) is a Rust based client for Cosmos SDK blockchains built exclusively on top of the gRPC api.

This demo leverages Rust async/await, combined with the gRPC api to demonstrate just how ridiculously fast Deep Space can be.

Here's a simple test run of this program, against the `gravitychain.io` node cluster. This cluster is three 8vcore 16gb ram $80/mo Digital Ocean VM's. Nothing special, furthermore it's over 80ms away from me, so hardly ideal latency conditions.

```text
This node has 405455 blocks to download, starting clock now
Successfully downloaded 313255 blocks in 186 seconds
```

1684 blocks / second download speed. Between 300-500mbps download and Rust isn't even using more than one core.

At that speed you could download all 10,804,593 blocks in the Cosmos Hub history in about 109 minutes.

Storing the downloaded block data locally would be a pretty straight forward extension using the RocksDB or Sled crates.
