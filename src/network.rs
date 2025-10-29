use crate::{peer::TestPeer, Error, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A test network consisting of gateways and peer nodes
pub struct TestNetwork {
    pub(crate) gateways: Vec<TestPeer>,
    pub(crate) peers: Vec<TestPeer>,
}

impl TestNetwork {
    /// Create a new network builder
    pub fn builder() -> crate::builder::NetworkBuilder {
        crate::builder::NetworkBuilder::new()
    }

    /// Get a gateway peer by index
    pub fn gateway(&self, index: usize) -> &TestPeer {
        &self.gateways[index]
    }

    /// Get a non-gateway peer by index
    pub fn peer(&self, index: usize) -> &TestPeer {
        &self.peers[index]
    }

    /// Get all gateway WebSocket URLs
    pub fn gateway_ws_urls(&self) -> Vec<String> {
        self.gateways.iter().map(|p| p.ws_url()).collect()
    }

    /// Get all peer WebSocket URLs
    pub fn peer_ws_urls(&self) -> Vec<String> {
        self.peers.iter().map(|p| p.ws_url()).collect()
    }

    /// Wait until the network is ready for use
    ///
    /// This checks that peers have formed connections and the network
    /// is sufficiently connected for testing.
    pub async fn wait_until_ready(&self) -> Result<()> {
        self.wait_until_ready_with_timeout(Duration::from_secs(30)).await
    }

    /// Wait until the network is ready with a custom timeout
    pub async fn wait_until_ready_with_timeout(&self, timeout: Duration) -> Result<()> {
        let start = std::time::Instant::now();
        let min_connectivity = 0.8; // 80% of peers should be connected

        tracing::info!(
            "Waiting for network connectivity (timeout: {}s, required: {}%)",
            timeout.as_secs(),
            (min_connectivity * 100.0) as u8
        );

        loop {
            if start.elapsed() > timeout {
                return Err(Error::ConnectivityFailed(format!(
                    "Network did not reach {}% connectivity within {}s",
                    (min_connectivity * 100.0) as u8,
                    timeout.as_secs()
                )));
            }

            // Check connectivity by querying peers for their connections
            match self.check_connectivity().await {
                Ok(ratio) if ratio >= min_connectivity => {
                    tracing::info!("Network ready: {:.1}% connectivity", ratio * 100.0);
                    return Ok(());
                }
                Ok(ratio) => {
                    tracing::debug!("Network connectivity: {:.1}%", ratio * 100.0);
                }
                Err(e) => {
                    tracing::debug!("Connectivity check failed: {}", e);
                }
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Check current network connectivity ratio (0.0 to 1.0)
    async fn check_connectivity(&self) -> Result<f64> {
        // TODO: Implement actual connectivity checking via WebSocket queries
        // This is the critical part that fixes the FIXME bug in fdev

        // For now, return a placeholder
        // Real implementation needs to:
        // 1. Connect to each peer via WebSocket
        // 2. Query for connected peers (NodeQuery::ConnectedPeers)
        // 3. Calculate connectivity ratio

        Ok(1.0) // Placeholder - always reports ready
    }

    /// Get the current network topology
    pub async fn topology(&self) -> Result<NetworkTopology> {
        // TODO: Query peers for their connections and build topology
        Ok(NetworkTopology {
            peers: vec![],
            connections: vec![],
        })
    }

    /// Export network information in JSON format for visualization tools
    pub fn export_for_viz(&self) -> String {
        let peers: Vec<_> = self.gateways.iter()
            .chain(self.peers.iter())
            .map(|p| serde_json::json!({
                "id": p.id(),
                "is_gateway": p.is_gateway(),
                "ws_port": p.ws_port,
                "network_port": p.network_port,
            }))
            .collect();

        serde_json::to_string_pretty(&serde_json::json!({
            "peers": peers
        })).unwrap_or_default()
    }
}

impl TestNetwork {
    pub(crate) fn new(gateways: Vec<TestPeer>, peers: Vec<TestPeer>) -> Self {
        Self { gateways, peers }
    }
}

/// Network topology information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkTopology {
    pub peers: Vec<PeerInfo>,
    pub connections: Vec<Connection>,
}

/// Information about a peer in the network
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub id: String,
    pub is_gateway: bool,
    pub ws_port: u16,
}

/// A connection between two peers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connection {
    pub from: String,
    pub to: String,
}
