//! Basic example of starting a test network

use freenet_test_network::TestNetwork;
use std::sync::LazyLock;

// Shared network for efficient reuse
static NETWORK: LazyLock<TestNetwork> = LazyLock::new(|| {
    TestNetwork::builder()
        .gateways(1)
        .peers(5)
        .build_sync()
        .unwrap()
});

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let network = &*NETWORK;

    println!("Network started!");
    println!("Gateway WebSocket: {}", network.gateway(0).ws_url());
    println!("\nAll peer URLs:");
    for url in network.peer_ws_urls() {
        println!("  {}", url);
    }

    println!("\nNetwork info (JSON):");
    println!("{}", network.export_for_viz());

    println!("\nPress Ctrl+C to stop...");
    tokio::signal::ctrl_c().await?;

    Ok(())
}
