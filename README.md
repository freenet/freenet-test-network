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

### TODO
- [ ] Implement proper connectivity detection (fix FIXME from fdev)
- [ ] Add WebSocket connection helpers
- [ ] Add integration tests
- [ ] Optimize startup time

## License

LGPL-3.0-only
