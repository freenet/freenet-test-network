//! Example demonstrating Docker NAT simulation
//!
//! Run with: cargo run --example docker_nat

use freenet_test_network::{Backend, DockerNatConfig, FreenetBinary, TestNetwork};
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    println!("Starting Docker NAT test network...");
    println!("This will create:");
    println!("  - 1 gateway on public network (172.20.0.10)");
    println!("  - 2 peers, each behind their own NAT router");
    println!();

    let network = TestNetwork::builder()
        .gateways(1)
        .peers(2)
        .binary(FreenetBinary::Installed) // Use system freenet binary
        .backend(Backend::DockerNat(DockerNatConfig::default()))
        .require_connectivity(1.0)
        .connectivity_timeout(Duration::from_secs(120))
        .preserve_temp_dirs_on_failure(true)
        .build()
        .await?;

    println!("\n=== Network Started Successfully ===\n");

    println!("Gateway:");
    println!("  WebSocket URL: {}", network.gateway(0).ws_url());
    println!("  Network Address: {}", network.gateway(0).network_address());

    for i in 0..2 {
        println!("\nPeer {}:", i);
        println!("  WebSocket URL: {}", network.peer(i).ws_url());
        println!("  Network Address: {} (behind NAT)", network.peer(i).network_address());
    }

    println!("\n=== Network Topology ===");
    println!("
    ┌─────────────────────────────────────────────────────┐
    │              PUBLIC NETWORK (172.20.0.0/24)         │
    │                                                      │
    │    ┌───────────┐                                    │
    │    │  Gateway  │                                    │
    │    │ 172.20.   │                                    │
    │    │  0.10     │                                    │
    │    └─────┬─────┘                                    │
    └──────────┼──────────────────────────────────────────┘
               │
    ┌──────────┼────────────────────────────────┐
    │          │                                │
┌───┴────┐  ┌──┴─────┐     ┌─────────┐  ┌──────┴──┐
│NAT     │  │NAT     │     │NAT      │  │NAT      │
│Router 1│  │Router 2│     │         │  │         │
│172.20. │  │172.20. │     │         │  │         │
│ 0.101  │  │ 0.102  │     │         │  │         │
└───┬────┘  └───┬────┘     └────┬────┘  └────┬────┘
    │           │               │            │
┌───┴────┐  ┌───┴────┐     ┌────┴───┐  ┌─────┴───┐
│ Peer 1 │  │ Peer 2 │     │        │  │         │
│ 10.2.  │  │ 10.3.  │     │        │  │         │
│ 0.2    │  │ 0.2    │     │        │  │         │
└────────┘  └────────┘     └────────┘  └─────────┘
");

    println!("Press Ctrl+C to stop the network...");

    // Keep running until interrupted
    tokio::signal::ctrl_c().await?;

    println!("\nShutting down...");

    Ok(())
}
