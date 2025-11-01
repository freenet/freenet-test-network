# freenet-test-network

Reliable test network infrastructure for Freenet integration testing.

## Problem

Testing Freenet requires spinning up multiple local peers, which has been unreliable and flaky. This crate provides a **simple, reliable way** to create test networks.

## Quick Start

```rust
use freenet_test_network::TestNetwork;
use std::sync::LazyLock;

// Shared network for all tests (starts once)
static NETWORK: LazyLock<TestNetwork> = LazyLock::new(|| {
    TestNetwork::builder()
        .gateways(1)
        .peers(5)
        .build_sync()
        .unwrap()
});

#[tokio::test]
async fn my_test() -> anyhow::Result<()> {
    let network = &*NETWORK;

    // Each test gets its own WebSocket connection
    let ws_url = network.gateway(0).ws_url();

    // Use freenet_stdlib::client_api::WebApi to connect
    // ...

    Ok(())
}
```

## Status

**Alpha** - Core functionality working, connectivity detection still uses placeholder.

## Diagnostics

When a network fails to reach the desired connectivity threshold the builder now:

- Prints the most recent log entries across **all peers in chronological order**
- Can optionally preserve each peer's data directory (including `peer.log`, configs, and keys) for later inspection

Enable preservation with:

```rust
let network = TestNetwork::builder()
    .gateways(1)
    .peers(2)
    .preserve_temp_dirs_on_failure(true)
    .build()
    .await?;
```

If startup fails, the preserved artifacts are placed under `/tmp/freenet-test-network-<timestamp>/`.

### TODO
- [ ] Implement proper connectivity detection (fix FIXME from fdev)
- [ ] Add WebSocket connection helpers
- [ ] Add integration tests
- [ ] Optimize startup time

## License

LGPL-3.0-only
