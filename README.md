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

## Docker NAT Simulation

**New**: Test Freenet peers behind simulated NAT routers using Docker containers.

### Why Docker NAT Testing?

Testing on localhost can't catch bugs related to:
- NAT traversal and port mapping
- Peers on different private networks
- Real network isolation
- Gateway discovery from behind NAT

### Quick Start

```rust
use freenet_test_network::{TestNetwork, Backend, DockerNatConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let network = TestNetwork::builder()
        .gateways(1)                                    // Gateway on public network
        .peers(2)                                        // Peers behind NAT
        .backend(Backend::DockerNat(DockerNatConfig::default()))
        .build()
        .await?;

    // Each peer runs in its own container behind a NAT router
    println!("Gateway: {}", network.gateway(0).ws_url());
    println!("Peer 0 (NAT): {}", network.peer(0).ws_url());

    Ok(())
}
```

### Automatic Cleanup

The Docker NAT backend automatically cleans up resources:

1. **On normal exit**: Containers and networks are removed via Drop trait
2. **On ctrl-c**: Signal handler ensures graceful cleanup
3. **On panic**: Drop trait still executes, cleaning up resources
4. **Stale resources**: Automatically removed at startup (older than 5 minutes)

Resource names include timestamps (e.g., `freenet-nat-20251126-211500-13135`) for easy identification.

### Manual Cleanup

If resources are somehow left behind, you can manually clean them:

```bash
# List freenet-nat resources
docker ps -a | grep freenet-nat
docker network ls | grep freenet-nat

# Remove all freenet-nat containers
docker ps -a | grep freenet-nat | awk '{print $1}' | xargs -r docker rm -f

# Remove all freenet-nat networks
docker network ls | grep freenet-nat | awk '{print $1}' | xargs -r docker network rm
```

### Example

See `examples/docker_nat.rs` for a complete example:

```bash
cargo run --example docker_nat
# Or with custom peer count:
PEER_COUNT=4 cargo run --example docker_nat
```

## Per-Node Binaries, Arguments and Hosts

By default one binary (set with `.binary(..)`) runs every node. For tests that
mix versions, individual nodes can override the binary, add command-line
arguments, and add `/etc/hosts` entries.

```rust
use freenet_test_network::{Backend, FreenetBinary, NetworkBuilder};
use std::path::PathBuf;

let network = NetworkBuilder::new()
    .backend(Backend::DockerNat(Default::default()))
    .gateways(1)
    .peers(3)
    // Default binary for nodes without an override.
    .binary(FreenetBinary::Path(PathBuf::from("/bin/freenet-new")))
    // Peer 1 runs an older release.
    .peer_binary(1, FreenetBinary::Path(PathBuf::from("/bin/freenet-old")))
    // Keep every node from reaching GitHub, so an older node does not
    // take the auto-update path and replace the binary under test.
    .extra_hosts_all(["github.com:127.0.0.1", "api.github.com:127.0.0.1"])
    // Extra CLI arguments for a single node.
    .peer_extra_args(2, ["--some-flag", "value"])
    .build()
    .await?;
```

| Method | Effect |
|---|---|
| `gateway_binary(i, bin)` / `peer_binary(i, bin)` | Binary for one node |
| `gateway_extra_args(i, args)` / `peer_extra_args(i, args)` | Arguments appended to that node's `freenet network` command |
| `extra_args_all(args)` | Arguments appended to every node, before per-node ones |
| `gateway_extra_hosts(i, hosts)` / `peer_extra_hosts(i, hosts)` | `/etc/hosts` entries for one container |
| `extra_hosts_all(hosts)` | `/etc/hosts` entries for every container |

Notes:

- Indices count within the role: `peer_binary(0, ..)` is the first regular peer,
  not the first node. (`peer_location` still uses the combined index.) Docker
  container names also use the combined index: with one gateway, regular peer 0
  is the container `<prefix>-peer-1`.
- Host entries use Docker's `host:ip` form (the value may be an IPv6 address or
  `host-gateway`) and are Docker NAT only; a local-process network refuses them
  at build time. Whitespace and empty parts are rejected.
- Extra arguments are appended after the harness arguments. Do not repeat a
  flag the harness already sets (`--min-number-of-connections` when you also
  call `min_connections`, `--ws-api-port`, `--is-gateway`, and so on): the node
  rejects the duplicate and fails at startup.
- Use `FreenetBinary::Path` for per-node overrides. The other variants build or
  look up a binary once per node that uses them.
- Per-node binaries are not supported on remote (SSH) peers, because remote
  deployment stores every binary under `bin/<file name>` and
  `RemoteMachine::freenet_binary` replaces all of them. `build()` returns an
  error for that combination.
- Overrides for a node that does not exist, or a malformed host entry, fail
  `build()` instead of being ignored.
- The default binary is resolved only if some node uses it, so a network where
  every node is overridden never builds it.

`examples/per_node_check.rs` is a manual end-to-end check of these options.
It needs Docker, a `freenet` on PATH (the gateway) and a second, different
binary in `FREENET_OLD_BINARY` (the peer):
`FREENET_OLD_BINARY=/path/to/older/freenet cargo run --example per_node_check`.
It confirms from inside the containers which binary each one runs, the host
entry, and the extra argument. Its command-line check reads
`/proc/1/cmdline`, so it assumes the node is PID 1 (no entrypoint wrapper).

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
