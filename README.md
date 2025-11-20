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

**Alpha** - Core functionality working, including remote SSH peer support for realistic NAT/firewall testing.

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

## Remote Peer Support (SSH)

**New in v0.2**: Spawn peers on remote Linux machines via SSH for realistic NAT/firewall testing.

### Why Remote Peers?

Testing on localhost (127.0.0.1) can't catch bugs related to:
- NAT traversal
- Address observation (e.g., issue #2087 - loopback address propagation)
- Real network latency and routing
- Firewall behavior

### Quick Start

```rust
use freenet_test_network::{TestNetwork, PeerLocation, RemoteMachine};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Configure remote machine
    let remote = RemoteMachine::new("192.168.1.100")
        .user("testuser")
        .identity_file("/home/user/.ssh/id_rsa");

    // Build network with remote peer
    let network = TestNetwork::builder()
        .gateways(1)                                        // Local gateway
        .peers(2)                                            // 2 regular peers
        .peer_location(2, PeerLocation::Remote(remote))     // Make peer #2 remote
        .build()
        .await?;

    // Use network as normal
    let ws_url = network.peer(1).ws_url();

    Ok(())
}
```

### Setup Requirements

1. **SSH Access**: Password-less SSH via key or agent
2. **Remote Machine**: Linux with SSH server (OpenSSH recommended)
3. **Binary Deployment**: Freenet binary is automatically uploaded via SCP
4. **Ports**: Remote peer will bind to OS-allocated ports (tests default port selection)

### Configuration Options

```rust
let remote = RemoteMachine::new("example.com")
    .user("testuser")                               // SSH username (default: current user)
    .port(2222)                                     // SSH port (default: 22)
    .identity_file("/path/to/key")                  // SSH private key
    .freenet_binary("/usr/local/bin/freenet")       // Pre-installed binary path (optional)
    .work_dir("/tmp/freenet-tests");                // Working directory (default: /tmp/freenet-test-network)
```

### Advanced: Multiple Remote Machines

```rust
let remote1 = RemoteMachine::new("192.168.1.100");
let remote2 = RemoteMachine::new("192.168.1.101");

let network = TestNetwork::builder()
    .gateways(1)
    .peers(3)
    .peer_location(1, PeerLocation::Remote(remote1))
    .peer_location(2, PeerLocation::Remote(remote2))
    // peer 0 and gateway remain local
    .build()
    .await?;
```

### Distribute Across Remotes

Round-robin distribution across multiple machines:

```rust
let machines = vec![
    RemoteMachine::new("192.168.1.100"),
    RemoteMachine::new("192.168.1.101"),
];

let network = TestNetwork::builder()
    .gateways(2)
    .peers(4)
    .distribute_across_remotes(machines)  // Alternates peers across machines
    .build()
    .await?;
```

### How It Works

1. **Binary Deployment**: Local freenet binary is uploaded to remote machine via SCP
2. **Process Spawning**: Peer started via `nohup` over SSH, PID captured
3. **Address Discovery**: Remote peer's public IP is auto-detected
4. **Gateway Keys**: Gateway public keys are uploaded to remote peers
5. **Log Collection**: Logs are downloaded via SCP on demand
6. **Cleanup**: Remote processes are killed via SSH on Drop

### Limitations

- **GitHub Actions**: Outbound SSH likely blocked, use for local testing
- **Platform**: Remote machines must be Linux (SSH + standard utilities)
- **Network**: Remote machines must be reachable from test runner

### Example

See `examples/remote_peer.rs` for a complete example.

```bash
export REMOTE_HOST=192.168.1.100
export REMOTE_USER=testuser
export SSH_KEY_PATH=$HOME/.ssh/id_rsa
cargo run --example remote_peer
```

### TODO
- [ ] Implement proper connectivity detection (fix FIXME from fdev)
- [ ] Add WebSocket connection helpers
- [ ] Add integration tests
- [ ] Optimize startup time
- [ ] Add remote peer health monitoring
- [ ] Support custom SSH connection pooling

## License

LGPL-3.0-only
