//! Example showing unified log viewing from all peers

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let network = freenet_test_network::TestNetwork::builder()
        .gateways(1)
        .peers(3)
        .build()
        .await?;

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
