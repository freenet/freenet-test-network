use crate::{peer::TestPeer, Error, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A test network consisting of gateways and peer nodes
pub struct TestNetwork {
    pub(crate) gateways: Vec<TestPeer>,
    pub(crate) peers: Vec<TestPeer>,
    pub(crate) min_connectivity: f64,
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

        tracing::info!(
            "Waiting for network connectivity (timeout: {}s, required: {}%)",
            timeout.as_secs(),
            (self.min_connectivity * 100.0) as u8
        );

        loop {
            if start.elapsed() > timeout {
                return Err(Error::ConnectivityFailed(format!(
                    "Network did not reach {}% connectivity within {}s",
                    (self.min_connectivity * 100.0) as u8,
                    timeout.as_secs()
                )));
            }

            // Check connectivity by querying peers for their connections
            match self.check_connectivity().await {
                Ok(ratio) if ratio >= self.min_connectivity => {
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
        let all_peers: Vec<_> = self.gateways.iter().chain(self.peers.iter()).collect();
        let total = all_peers.len();

        if total == 0 {
            return Ok(1.0);
        }

        let mut connected_count = 0;

        for peer in &all_peers {
            match self.query_peer_connections(peer).await {
                Ok(0) => {
                    tracing::trace!("{} has no connections (isolated)", peer.id());
                }
                Ok(connections) => {
                    connected_count += 1;
                    tracing::trace!("{} has {} connections", peer.id(), connections);
                }
                Err(e) => {
                    tracing::debug!("Failed to query {}: {}", peer.id(), e);
                }
            }
        }

        let ratio = connected_count as f64 / total as f64;
        Ok(ratio)
    }

    /// Query a single peer for its connection count
    async fn query_peer_connections(&self, peer: &TestPeer) -> Result<usize> {
        use tokio_tungstenite::connect_async;

        let url = peer.ws_url();
        let (ws_stream, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            connect_async(&url)
        ).await
            .map_err(|_| Error::ConnectivityFailed(format!("Timeout connecting to {}", url)))?
            .map_err(|e| Error::ConnectivityFailed(format!("Failed to connect to {}: {}", url, e)))?;

        use futures::stream::StreamExt;
        use futures::sink::SinkExt;
        let (mut write, mut read) = ws_stream.split();

        // Send ConnectedPeers query
        let query = serde_json::json!({
            "NodeQueries": {
                "ConnectedPeers": {}
            }
        });

        let msg = tokio_tungstenite::tungstenite::Message::Text(query.to_string());
        write.send(msg).await
            .map_err(|e| Error::ConnectivityFailed(format!("Failed to send query: {}", e)))?;

        // Read response
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            read.next()
        ).await
            .map_err(|_| Error::ConnectivityFailed("Timeout waiting for response".into()))?
            .ok_or_else(|| Error::ConnectivityFailed("No response received".into()))?
            .map_err(|e| Error::ConnectivityFailed(format!("WebSocket error: {}", e)))?;

        // Parse response
        let text = response.to_text()
            .map_err(|e| Error::ConnectivityFailed(format!("Invalid response: {}", e)))?;

        let resp: serde_json::Value = serde_json::from_str(text)
            .map_err(|e| Error::ConnectivityFailed(format!("JSON parse error: {}", e)))?;

        // Extract peer count from response
        if let Some(peers) = resp.get("QueryResponse")
            .and_then(|q| q.get("ConnectedPeers"))
            .and_then(|c| c.get("peers"))
            .and_then(|p| p.as_array())
        {
            Ok(peers.len())
        } else {
            Err(Error::ConnectivityFailed(format!("Unexpected response format: {}", text)))
        }
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
    pub(crate) fn new(gateways: Vec<TestPeer>, peers: Vec<TestPeer>, min_connectivity: f64) -> Self {
        Self { gateways, peers, min_connectivity }
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
