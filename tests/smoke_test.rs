//! Basic smoke test - just verify network starts

use freenet_test_network::{FreenetBinary, TestNetwork};

#[tokio::test]
async fn test_network_starts() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let network = TestNetwork::builder()
        .gateways(1)
        .peers(2)
        .binary(FreenetBinary::Installed)
        .build()
        .await?;

    let ring_viz_path = network.run_root().join("ring_viz.html");
    network
        .write_ring_visualization(&ring_viz_path)
        .await
        .expect("ring visualization should be generated");
    assert!(
        ring_viz_path.exists(),
        "ring visualization file should exist"
    );

    // Verify network is accessible
    assert!(!network.gateway(0).ws_url().is_empty());
    assert!(!network.peer(0).ws_url().is_empty());

    println!("✓ Network started successfully");
    println!("  Gateway: {}", network.gateway(0).ws_url());
    println!("  Peers: {}", network.peer_ws_urls().len());

    Ok(())
}
