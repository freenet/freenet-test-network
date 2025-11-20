//! Example of creating a test network with remote peers via SSH
//!
//! This demonstrates how to spawn peers on remote Linux machines to test
//! NAT traversal, address observation, and other network behaviors.
//!
//! # Setup Requirements
//!
//! 1. SSH access to a remote Linux machine
//! 2. SSH key authentication configured (or SSH agent)
//! 3. freenet binary will be deployed automatically to the remote machine
//!
//! # Usage
//!
//! ```bash
//! # Set up environment variables for your remote machine
//! export REMOTE_HOST="192.168.1.100"
//! export REMOTE_USER="testuser"
//! export SSH_KEY_PATH="$HOME/.ssh/id_rsa"
//!
//! cargo run --example remote_peer
//! ```

use freenet_test_network::{PeerLocation, RemoteMachine, TestNetwork};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    println!("=== Remote Peer Test Network Example ===\n");

    // Configure remote machine from environment or defaults
    let remote_host = std::env::var("REMOTE_HOST").unwrap_or_else(|_| "localhost".to_string());
    let remote_user = std::env::var("REMOTE_USER").ok();
    let ssh_key = std::env::var("SSH_KEY_PATH")
        .ok()
        .map(std::path::PathBuf::from);

    println!("Remote machine configuration:");
    println!("  Host: {}", remote_host);
    println!(
        "  User: {}",
        remote_user.as_deref().unwrap_or("<current user>")
    );
    println!(
        "  SSH key: {}",
        ssh_key
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<SSH agent>".to_string())
    );
    println!();

    // Create remote machine configuration
    let mut remote = RemoteMachine::new(&remote_host);
    if let Some(user) = remote_user {
        remote = remote.user(user);
    }
    if let Some(key) = ssh_key {
        remote = remote.identity_file(key);
    }

    // Test SSH connection first
    println!("Testing SSH connection to {}...", remote_host);
    match remote.connect() {
        Ok(_) => println!("✓ SSH connection successful\n"),
        Err(e) => {
            eprintln!("✗ SSH connection failed: {}", e);
            eprintln!("\nPlease ensure:");
            eprintln!("  1. Remote host is accessible");
            eprintln!("  2. SSH key authentication is configured");
            eprintln!("  3. REMOTE_HOST environment variable is set");
            eprintln!("\nExample:");
            eprintln!("  export REMOTE_HOST=192.168.1.100");
            eprintln!("  export REMOTE_USER=myuser");
            eprintln!("  export SSH_KEY_PATH=$HOME/.ssh/id_rsa");
            return Err(e.into());
        }
    }

    println!("Starting test network:");
    println!("  - 1 gateway (local)");
    println!("  - 1 regular peer (local)");
    println!("  - 1 regular peer (REMOTE on {})", remote_host);
    println!();

    // Build network with mixed local and remote peers
    let network = TestNetwork::builder()
        .gateways(1)
        .peers(2)
        .peer_location(2, PeerLocation::Remote(remote.clone())) // Third peer (index 2) is remote
        .preserve_temp_dirs_on_failure(true)
        .build()
        .await?;

    println!("✓ Network started successfully!\n");

    println!("Gateway:");
    println!("  WebSocket: {}", network.gateway(0).ws_url());
    println!("  Address: {}", network.gateway(0).network_address());
    println!("  Location: {:?}", network.gateway(0).location());
    println!();

    println!("Peer 0 (local):");
    println!("  WebSocket: {}", network.peer(0).ws_url());
    println!("  Address: {}", network.peer(0).network_address());
    println!("  Location: {:?}", network.peer(0).location());
    println!();

    println!("Peer 1 (REMOTE):");
    println!("  WebSocket: {}", network.peer(1).ws_url());
    println!("  Address: {}", network.peer(1).network_address());
    println!("  Location: {:?}", network.peer(1).location());
    println!();

    // Check connectivity
    println!("Network topology:");
    println!(
        "  Gateway at {} (local loopback)",
        network.gateway(0).network_address()
    );
    println!(
        "  Peer 0 at {} (local loopback)",
        network.peer(0).network_address()
    );
    println!(
        "  Peer 1 at {} (REMOTE - real IP)",
        network.peer(1).network_address()
    );
    println!();

    println!("This demonstrates realistic NAT/firewall testing:");
    println!("  ✓ Local peers use 127.x.y.z addresses");
    println!("  ✓ Remote peer uses actual network IP");
    println!("  ✓ Tests address observation and propagation");
    println!("  ✓ Would have caught issue #2087 (loopback bug)");
    println!();

    println!("Press Ctrl+C to stop...");
    tokio::signal::ctrl_c().await?;

    println!("\nShutting down network...");
    drop(network);
    println!("✓ All peers stopped");

    Ok(())
}
