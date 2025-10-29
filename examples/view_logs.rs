//! Example showing unified log viewing from all peers

use freenet_test_network::TestNetwork;
use std::sync::LazyLock;

static NETWORK: LazyLock<TestNetwork> = LazyLock::new(|| {
    TestNetwork::builder()
        .gateways(1)
        .peers(3)
        .build_sync()
        .unwrap()
});

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let network = &*NETWORK;

    println!("Network started. Waiting for some activity...");
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    println!("\n=== Unified Log View (Chronological) ===\n");

    for entry in network.read_logs()? {
        println!(
            "[{}] {} | {}",
            entry.peer_id,
            entry.level.unwrap_or_else(|| "INFO".to_string()),
            entry.message
        );
    }

    Ok(())
}
