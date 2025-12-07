//! NAT Hole-Punch Diagnostic Test
//!
//! This example diagnoses peer-to-peer NAT traversal issues by:
//! 1. Starting a minimal Docker NAT network (1 gateway, 2 peers)
//! 2. Capturing UDP packets on NAT routers
//! 3. Checking if hole-punch packets are exchanged
//! 4. Testing with different NAT configurations
//!
//! Run with: cargo run --example nat_hole_punch_diagnostic
//!
//! For verbose output: RUST_LOG=debug cargo run --example nat_hole_punch_diagnostic

use anyhow::{Context, Result};
use freenet_test_network::{Backend, BuildProfile, DockerNatConfig, FreenetBinary, TestNetwork};
use std::process::Command;
use std::time::Duration;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,freenet_test_network=debug".into()),
        )
        .init();

    info!("=== NAT Hole-Punch Diagnostic ===");
    info!("This test diagnoses peer-to-peer connectivity through NAT");
    info!("");

    // Use FREENET_CORE_PATH env var if set
    let freenet_core_path = std::env::var("FREENET_CORE_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("../freenet-core/main"));

    let binary = if freenet_core_path.exists() {
        info!(
            "Using freenet-core workspace at: {}",
            freenet_core_path.display()
        );
        FreenetBinary::Workspace {
            path: freenet_core_path,
            profile: BuildProfile::Release,
        }
    } else {
        info!("freenet-core workspace not found, using installed binary");
        FreenetBinary::Installed
    };

    info!("Starting network: 1 gateway + 2 peers (each behind separate NAT)...");
    let network = TestNetwork::builder()
        .gateways(1)
        .peers(2)
        .binary(binary)
        .backend(Backend::DockerNat(DockerNatConfig::default()))
        .require_connectivity(0.5) // Lower threshold - we're testing peer-to-peer
        .connectivity_timeout(Duration::from_secs(60))
        .preserve_temp_dirs_on_failure(true)
        .build()
        .await
        .context("Failed to start test network")?;

    info!("Network started. Run root: {}", network.run_root().display());
    info!("");

    // Print network topology
    info!("=== Network Topology ===");
    info!(
        "Gateway: {} (public IP)",
        network.gateway(0).network_address()
    );
    for i in 0..2 {
        info!(
            "Peer {}: {} (behind NAT)",
            i,
            network.peer(i).network_address()
        );
    }
    info!("");

    // List containers
    info!("=== Docker Containers ===");
    let containers = Command::new("docker")
        .args(["ps", "--filter", "name=freenet", "--format", "{{.Names}}\t{{.Status}}"])
        .output()
        .context("Failed to list containers")?;
    info!("{}", String::from_utf8_lossy(&containers.stdout));

    // Find NAT router containers
    let router_output = Command::new("docker")
        .args([
            "ps",
            "--filter",
            "name=router",
            "--format",
            "{{.Names}}",
        ])
        .output()
        .context("Failed to find NAT routers")?;
    let router_str = String::from_utf8_lossy(&router_output.stdout).to_string();
    let routers: Vec<&str> = router_str
        .trim()
        .split('\n')
        .filter(|s| !s.is_empty() && s.contains("router"))
        .collect();

    if routers.is_empty() {
        error!("No NAT router containers found!");
        return Ok(());
    }

    info!("=== NAT Router iptables Rules ===");
    for router in &routers {
        info!("--- {} ---", router);
        let iptables = Command::new("docker")
            .args(["exec", router, "iptables", "-t", "nat", "-L", "-n", "-v"])
            .output()
            .context("Failed to get iptables rules")?;
        info!("{}", String::from_utf8_lossy(&iptables.stdout));

        // Also show conntrack table
        info!("--- {} conntrack entries ---", router);
        let _ = Command::new("docker")
            .args(["exec", router, "apk", "add", "--no-cache", "conntrack-tools"])
            .output();
        let conntrack = Command::new("docker")
            .args(["exec", router, "conntrack", "-L"])
            .output();
        if let Ok(ct) = conntrack {
            let output = String::from_utf8_lossy(&ct.stdout);
            if output.trim().is_empty() {
                info!("  (no conntrack entries)");
            } else {
                for line in output.lines().take(20) {
                    info!("  {}", line);
                }
            }
        }
    }

    // Wait for stabilization and check peer connections
    info!("");
    info!("=== Waiting 30s for P2P connections to establish ===");
    tokio::time::sleep(Duration::from_secs(30)).await;

    // Query debug info from each peer
    info!("");
    info!("=== Peer Connection Status ===");

    for i in 0..2 {
        info!("--- Peer {} ---", i);
        let ws_url = network.peer(i).ws_url();
        info!("  WS URL: {}", ws_url);

        // Read some logs to see connection attempts
        match network.peer(i).read_logs() {
            Ok(logs) => {
                let relevant: Vec<_> = logs
                    .iter()
                    .filter(|e| {
                        e.message.contains("NAT")
                            || e.message.contains("hole")
                            || e.message.contains("ConnectPeer")
                            || e.message.contains("connect_peer")
                            || e.message.contains("connected to")
                    })
                    .collect();

                if relevant.is_empty() {
                    warn!("  No NAT/connection logs found");
                } else {
                    for entry in relevant.iter().take(10) {
                        info!("  [LOG] {}", entry.message);
                    }
                }
            }
            Err(e) => warn!("  Failed to read logs: {:?}", e),
        }
    }

    // Check gateway connections
    info!("");
    info!("--- Gateway 0 ---");
    match network.gateway(0).read_logs() {
        Ok(logs) => {
            let relevant: Vec<_> = logs
                .iter()
                .filter(|e| {
                    e.message.contains("connected")
                        || e.message.contains("connection")
                        || e.message.contains("peer")
                })
                .collect();

            for entry in relevant.iter().take(10) {
                info!("  [LOG] {}", entry.message);
            }
        }
        Err(e) => warn!("  Failed to read logs: {:?}", e),
    }

    // Final conntrack state
    info!("");
    info!("=== Final NAT Conntrack State ===");
    for router in &routers {
        info!("--- {} ---", router);
        let conntrack = Command::new("docker")
            .args(["exec", router, "conntrack", "-L", "-p", "udp"])
            .output();
        if let Ok(ct) = conntrack {
            let output = String::from_utf8_lossy(&ct.stdout);
            if output.trim().is_empty() {
                info!("  (no UDP conntrack entries)");
            } else {
                for line in output.lines() {
                    info!("  {}", line);
                }
            }
        }
    }

    // Capture packets briefly
    info!("");
    info!("=== Capturing UDP packets for 10s ===");
    info!("(Looking for hole-punch traffic on port 31337)");

    for router in &routers {
        let router_owned = router.to_string();
        tokio::spawn(async move {
            // Install tcpdump
            let _ = Command::new("docker")
                .args(["exec", &router_owned, "apk", "add", "--no-cache", "tcpdump"])
                .output();

            // Capture packets
            let capture = Command::new("docker")
                .args([
                    "exec",
                    &router_owned,
                    "timeout",
                    "10",
                    "tcpdump",
                    "-i",
                    "any",
                    "-n",
                    "udp",
                    "-c",
                    "50",
                ])
                .output();

            if let Ok(cap) = capture {
                let output = String::from_utf8_lossy(&cap.stdout);
                if output.trim().is_empty() {
                    println!("  {}: No UDP packets captured", router_owned);
                } else {
                    println!("  --- {} packets ---", router_owned);
                    for line in output.lines().take(20) {
                        println!("  {}", line);
                    }
                }
            }
        });
    }

    // Wait for captures
    tokio::time::sleep(Duration::from_secs(12)).await;

    info!("");
    info!("=== Diagnostic Complete ===");
    info!("Logs saved to: {}", network.run_root().display());
    info!("");
    info!("Key questions to answer:");
    info!("1. Do peers connect directly, or only through gateway?");
    info!("2. Are hole-punch UDP packets being sent/received?");
    info!("3. Does conntrack show entries for peer-to-peer traffic?");

    Ok(())
}
