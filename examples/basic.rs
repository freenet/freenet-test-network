//! Basic example of starting a test network

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let network = freenet_test_network::TestNetwork::builder()
        .gateways(1)
        .peers(5)
        .build()
        .await?;

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
