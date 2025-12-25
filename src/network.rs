use crate::{docker::DockerNatBackend, peer::TestPeer, Error, Result};
use chrono::Utc;
use freenet_stdlib::{
    client_api::{
        ClientRequest, ConnectedPeerInfo, HostResponse, NodeDiagnosticsConfig, NodeQuery,
        QueryResponse, SystemMetrics, WebApi,
    },
    prelude::{CodeHash, ContractInstanceId, ContractKey},
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::Duration,
};

/// Gracefully close a WebSocket client connection.
///
/// This sends a Disconnect message and waits briefly for the close handshake to complete,
/// preventing "Connection reset without closing handshake" errors on the server.
async fn graceful_disconnect(client: WebApi, reason: &'static str) {
    client.disconnect(reason).await;
    // Brief delay to allow the close handshake to complete
    tokio::time::sleep(Duration::from_millis(50)).await;
}

/// Detailed connectivity status for a single peer
#[derive(Debug, Clone)]
pub struct PeerConnectivityStatus {
    pub peer_id: String,
    pub connections: Option<usize>,
    pub error: Option<String>,
}

/// Detailed connectivity check result
#[derive(Debug)]
pub struct ConnectivityStatus {
    pub total_peers: usize,
    pub connected_peers: usize,
    pub ratio: f64,
    pub peer_status: Vec<PeerConnectivityStatus>,
}

/// A test network consisting of gateways and peer nodes
pub struct TestNetwork {
    pub(crate) gateways: Vec<TestPeer>,
    pub(crate) peers: Vec<TestPeer>,
    pub(crate) min_connectivity: f64,
    pub(crate) run_root: PathBuf,
    /// Docker backend for NAT simulation (if used)
    pub(crate) docker_backend: Option<DockerNatBackend>,
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
        self.wait_until_ready_with_timeout(Duration::from_secs(30))
            .await
    }

    /// Wait until the network is ready with a custom timeout
    pub async fn wait_until_ready_with_timeout(&self, timeout: Duration) -> Result<()> {
        let start = std::time::Instant::now();
        let mut last_progress_log = std::time::Instant::now();
        let progress_interval = Duration::from_secs(10);

        tracing::info!(
            "Waiting for network connectivity (timeout: {}s, required: {}%)",
            timeout.as_secs(),
            (self.min_connectivity * 100.0) as u8
        );

        loop {
            if start.elapsed() > timeout {
                // Log final detailed status on failure
                let status = self.check_connectivity_detailed().await;
                let details = Self::format_connectivity_status(&status);
                tracing::error!(
                    "Connectivity timeout: {}/{} peers connected ({:.1}%) - {}",
                    status.connected_peers,
                    status.total_peers,
                    status.ratio * 100.0,
                    details
                );
                return Err(Error::ConnectivityFailed(format!(
                    "Network did not reach {}% connectivity within {}s",
                    (self.min_connectivity * 100.0) as u8,
                    timeout.as_secs()
                )));
            }

            // Check connectivity with detailed status
            let status = self.check_connectivity_detailed().await;

            if status.ratio >= self.min_connectivity {
                tracing::info!("Network ready: {:.1}% connectivity", status.ratio * 100.0);
                return Ok(());
            }

            // Log progress periodically (every 10 seconds)
            if last_progress_log.elapsed() >= progress_interval {
                let elapsed = start.elapsed().as_secs();
                let details = Self::format_connectivity_status(&status);
                tracing::info!(
                    "[{}s] Connectivity: {}/{} ({:.0}%) - {}",
                    elapsed,
                    status.connected_peers,
                    status.total_peers,
                    status.ratio * 100.0,
                    details
                );
                last_progress_log = std::time::Instant::now();
            } else {
                tracing::debug!(
                    "Network connectivity: {}/{} ({:.1}%)",
                    status.connected_peers,
                    status.total_peers,
                    status.ratio * 100.0
                );
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Check current network connectivity with detailed status
    pub async fn check_connectivity_detailed(&self) -> ConnectivityStatus {
        let all_peers: Vec<_> = self.gateways.iter().chain(self.peers.iter()).collect();
        let total = all_peers.len();

        if total == 0 {
            return ConnectivityStatus {
                total_peers: 0,
                connected_peers: 0,
                ratio: 1.0,
                peer_status: vec![],
            };
        }

        let mut connected_count = 0;
        let mut peer_status = Vec::with_capacity(total);

        for peer in &all_peers {
            match self.query_peer_connections(peer).await {
                Ok(0) => {
                    peer_status.push(PeerConnectivityStatus {
                        peer_id: peer.id().to_string(),
                        connections: Some(0),
                        error: None,
                    });
                }
                Ok(connections) => {
                    connected_count += 1;
                    peer_status.push(PeerConnectivityStatus {
                        peer_id: peer.id().to_string(),
                        connections: Some(connections),
                        error: None,
                    });
                }
                Err(e) => {
                    peer_status.push(PeerConnectivityStatus {
                        peer_id: peer.id().to_string(),
                        connections: None,
                        error: Some(e.to_string()),
                    });
                }
            }
        }

        let ratio = connected_count as f64 / total as f64;
        ConnectivityStatus {
            total_peers: total,
            connected_peers: connected_count,
            ratio,
            peer_status,
        }
    }

    /// Check current network connectivity ratio (0.0 to 1.0)
    async fn check_connectivity(&self) -> Result<f64> {
        let status = self.check_connectivity_detailed().await;
        Ok(status.ratio)
    }

    /// Format connectivity status for logging
    fn format_connectivity_status(status: &ConnectivityStatus) -> String {
        let mut parts: Vec<String> = status
            .peer_status
            .iter()
            .map(|p| match (&p.connections, &p.error) {
                (Some(c), _) => format!("{}:{}", p.peer_id, c),
                (None, Some(_)) => format!("{}:err", p.peer_id),
                (None, None) => format!("{}:?", p.peer_id),
            })
            .collect();
        parts.sort();
        parts.join(", ")
    }

    /// Query a single peer for its connection count
    async fn query_peer_connections(&self, peer: &TestPeer) -> Result<usize> {
        use tokio_tungstenite::connect_async;

        let url = format!("{}?encodingProtocol=native", peer.ws_url());
        let (ws_stream, _) =
            tokio::time::timeout(std::time::Duration::from_secs(5), connect_async(&url))
                .await
                .map_err(|_| Error::ConnectivityFailed(format!("Timeout connecting to {}", url)))?
                .map_err(|e| {
                    Error::ConnectivityFailed(format!("Failed to connect to {}: {}", url, e))
                })?;

        let mut client = WebApi::start(ws_stream);

        client
            .send(ClientRequest::NodeQueries(NodeQuery::ConnectedPeers))
            .await
            .map_err(|e| Error::ConnectivityFailed(format!("Failed to send query: {}", e)))?;

        let response = tokio::time::timeout(std::time::Duration::from_secs(5), client.recv())
            .await
            .map_err(|_| Error::ConnectivityFailed("Timeout waiting for response".into()))?;

        let result = match response {
            Ok(HostResponse::QueryResponse(QueryResponse::ConnectedPeers { peers })) => {
                Ok(peers.len())
            }
            Ok(other) => Err(Error::ConnectivityFailed(format!(
                "Unexpected response: {:?}",
                other
            ))),
            Err(e) => Err(Error::ConnectivityFailed(format!("Query failed: {}", e))),
        };

        graceful_disconnect(client, "connectivity probe").await;

        result
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
        let peers: Vec<_> = self
            .gateways
            .iter()
            .chain(self.peers.iter())
            .map(|p| {
                serde_json::json!({
                    "id": p.id(),
                    "is_gateway": p.is_gateway(),
                    "ws_port": p.ws_port,
                    "network_port": p.network_port,
                })
            })
            .collect();

        serde_json::to_string_pretty(&serde_json::json!({
            "peers": peers
        }))
        .unwrap_or_default()
    }

    /// Collect diagnostics from every peer, returning a snapshot that can be serialized to JSON
    /// for offline analysis.
    pub async fn collect_diagnostics(&self) -> Result<NetworkDiagnosticsSnapshot> {
        let mut peers = Vec::with_capacity(self.gateways.len() + self.peers.len());
        for peer in self.gateways.iter().chain(self.peers.iter()) {
            peers.push(self.query_peer_diagnostics(peer).await);
        }
        peers.sort_by(|a, b| a.peer_id.cmp(&b.peer_id));
        Ok(NetworkDiagnosticsSnapshot {
            collected_at: Utc::now(),
            peers,
        })
    }

    async fn query_peer_diagnostics(&self, peer: &TestPeer) -> PeerDiagnosticsSnapshot {
        use tokio_tungstenite::connect_async;

        let mut snapshot = PeerDiagnosticsSnapshot::new(peer);
        let url = format!("{}?encodingProtocol=native", peer.ws_url());
        match tokio::time::timeout(std::time::Duration::from_secs(10), connect_async(&url)).await {
            Ok(Ok((ws_stream, _))) => {
                let mut client = WebApi::start(ws_stream);
                let config = NodeDiagnosticsConfig {
                    include_node_info: true,
                    include_network_info: true,
                    include_subscriptions: false,
                    contract_keys: vec![],
                    include_system_metrics: true,
                    include_detailed_peer_info: true,
                    include_subscriber_peer_ids: false,
                };
                if let Err(err) = client
                    .send(ClientRequest::NodeQueries(NodeQuery::NodeDiagnostics {
                        config,
                    }))
                    .await
                {
                    snapshot.error = Some(format!("failed to send diagnostics request: {err}"));
                    graceful_disconnect(client, "diagnostics send error").await;
                    return snapshot;
                }
                match tokio::time::timeout(std::time::Duration::from_secs(10), client.recv()).await
                {
                    Ok(Ok(HostResponse::QueryResponse(QueryResponse::NodeDiagnostics(
                        response,
                    )))) => {
                        let node_info = response.node_info;
                        let network_info = response.network_info;
                        snapshot.peer_id = node_info
                            .as_ref()
                            .map(|info| info.peer_id.clone())
                            .unwrap_or_else(|| peer.id().to_string());
                        snapshot.is_gateway = node_info
                            .as_ref()
                            .map(|info| info.is_gateway)
                            .unwrap_or_else(|| peer.is_gateway());
                        snapshot.location =
                            node_info.as_ref().and_then(|info| info.location.clone());
                        snapshot.listening_address = node_info
                            .as_ref()
                            .and_then(|info| info.listening_address.clone());
                        if let Some(info) = network_info {
                            snapshot.active_connections = Some(info.active_connections);
                            snapshot.connected_peer_ids = info
                                .connected_peers
                                .into_iter()
                                .map(|(peer_id, _)| peer_id)
                                .collect();
                        }
                        snapshot.connected_peers_detailed = response.connected_peers_detailed;
                        snapshot.system_metrics = response.system_metrics;
                    }
                    Ok(Ok(other)) => {
                        snapshot.error =
                            Some(format!("unexpected diagnostics response: {:?}", other));
                    }
                    Ok(Err(err)) => {
                        snapshot.error = Some(format!("diagnostics channel error: {err}"));
                    }
                    Err(_) => {
                        snapshot.error = Some("timeout waiting for diagnostics response".into());
                    }
                }
                graceful_disconnect(client, "diagnostics complete").await;
            }
            Ok(Err(err)) => {
                snapshot.error = Some(format!("failed to connect websocket: {err}"));
            }
            Err(_) => {
                snapshot.error = Some("timeout establishing diagnostics websocket".into());
            }
        }
        snapshot
    }

    /// Collect per-peer ring data (locations + adjacency) for visualization/debugging.
    pub async fn ring_snapshot(&self) -> Result<Vec<RingPeerSnapshot>> {
        self.collect_ring_snapshot(None).await
    }

    /// Collect per-peer ring data with optional contract-specific subscription info.
    ///
    /// When `instance_id` is provided, each `RingPeerSnapshot` will include
    /// `PeerContractStatus` with subscriber information for that contract.
    pub async fn collect_ring_snapshot(
        &self,
        instance_id: Option<&ContractInstanceId>,
    ) -> Result<Vec<RingPeerSnapshot>> {
        let mut snapshots = Vec::with_capacity(self.gateways.len() + self.peers.len());
        for peer in self.gateways.iter().chain(self.peers.iter()) {
            snapshots.push(query_ring_snapshot(peer, instance_id).await?);
        }
        snapshots.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(snapshots)
    }

    /// Generate an interactive HTML ring visualization for the current network.
    pub async fn write_ring_visualization<P: AsRef<Path>>(&self, output_path: P) -> Result<()> {
        self.write_ring_visualization_internal(output_path, None)
            .await
    }

    /// Generate an interactive HTML ring visualization for a specific contract.
    ///
    /// Note: This function now takes a ContractKey directly instead of a string,
    /// since freenet-stdlib 0.1.27 no longer supports ContractKey::from_id.
    /// The contract_id string is used for display purposes.
    pub async fn write_ring_visualization_for_contract<P: AsRef<Path>>(
        &self,
        output_path: P,
        contract_key: &ContractKey,
        contract_id: &str,
    ) -> Result<()> {
        self.write_ring_visualization_internal(output_path, Some((contract_key.id(), contract_id)))
            .await
    }

    async fn write_ring_visualization_internal<P: AsRef<Path>>(
        &self,
        output_path: P,
        contract: Option<(&ContractInstanceId, &str)>,
    ) -> Result<()> {
        let (snapshots, contract_viz) = if let Some((instance_id, contract_id)) = contract {
            let snapshots = self.collect_ring_snapshot(Some(instance_id)).await?;
            let caching_peers = snapshots
                .iter()
                .filter(|peer| {
                    peer.contract
                        .as_ref()
                        .map(|state| state.stores_contract)
                        .unwrap_or(false)
                })
                .map(|peer| peer.id.clone())
                .collect::<Vec<_>>();
            let contract_location = contract_location_from_instance_id(instance_id);
            let flow = self.collect_contract_flow(contract_id, &snapshots)?;
            let viz = ContractVizData {
                key: contract_id.to_string(),
                location: contract_location,
                caching_peers,
                put: OperationPath {
                    edges: flow.put_edges,
                    completion_peer: flow.put_completion_peer,
                },
                update: OperationPath {
                    edges: flow.update_edges,
                    completion_peer: flow.update_completion_peer,
                },
                errors: flow.errors,
            };
            (snapshots, Some(viz))
        } else {
            (self.collect_ring_snapshot(None).await?, None)
        };

        let metrics = compute_ring_metrics(&snapshots);
        let payload = json!({
            "generated_at": Utc::now().to_rfc3339(),
            "run_root": self.run_root.display().to_string(),
            "nodes": snapshots,
            "metrics": metrics,
            "contract": contract_viz,
        });
        let data_json = serde_json::to_string(&payload).map_err(|e| Error::Other(e.into()))?;
        let html = render_ring_template(&data_json);
        let out_path = output_path.as_ref();
        if let Some(parent) = out_path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent)?;
            }
        }
        fs::write(out_path, html)?;
        tracing::info!(path = %out_path.display(), "Wrote ring visualization");
        Ok(())
    }

    fn collect_contract_flow(
        &self,
        contract_id: &str,
        peers: &[RingPeerSnapshot],
    ) -> Result<ContractFlowData> {
        let mut data = ContractFlowData::default();
        let logs = self.read_logs()?;
        for entry in logs {
            if !entry.message.contains(contract_id) {
                continue;
            }
            if let Some(caps) = PUT_REQUEST_RE.captures(&entry.message) {
                if &caps["key"] != contract_id {
                    continue;
                }
                data.put_edges.push(ContractOperationEdge {
                    from: caps["from"].to_string(),
                    to: caps["to"].to_string(),
                    timestamp: entry.timestamp.map(|ts| ts.to_rfc3339()),
                    log_level: entry.level.clone(),
                    log_source: Some(entry.peer_id.clone()),
                    message: entry.message.clone(),
                });
                continue;
            }
            if let Some(caps) = PUT_COMPLETION_RE.captures(&entry.message) {
                if &caps["key"] != contract_id {
                    continue;
                }
                data.put_completion_peer = Some(caps["peer"].to_string());
                continue;
            }
            if let Some(caps) = UPDATE_PROPAGATION_RE.captures(&entry.message) {
                if &caps["contract"] != contract_id {
                    continue;
                }
                let from = caps["from"].to_string();
                let ts = entry.timestamp.map(|ts| ts.to_rfc3339());
                let level = entry.level.clone();
                let source = Some(entry.peer_id.clone());
                let targets = caps["targets"].trim();
                if !targets.is_empty() {
                    for prefix in targets.split(',').filter(|s| !s.is_empty()) {
                        if let Some(resolved) = resolve_peer_id(prefix, peers) {
                            data.update_edges.push(ContractOperationEdge {
                                from: from.clone(),
                                to: resolved,
                                timestamp: ts.clone(),
                                log_level: level.clone(),
                                log_source: source.clone(),
                                message: entry.message.clone(),
                            });
                        }
                    }
                }
                continue;
            }
            if let Some(caps) = UPDATE_NO_TARGETS_RE.captures(&entry.message) {
                if &caps["contract"] != contract_id {
                    continue;
                }
                data.errors.push(entry.message.clone());
                continue;
            }
            if entry.message.contains("update will not propagate") {
                data.errors.push(entry.message.clone());
            }
        }
        Ok(data)
    }

    /// Dump logs from all peers, optionally filtered by a pattern
    ///
    /// If `filter` is Some, only logs containing the pattern (case-insensitive) are printed.
    /// Logs are printed in chronological order with peer ID prefix.
    ///
    /// This is useful for debugging test failures - call it before assertions or in error handlers.
    pub fn dump_logs(&self, filter: Option<&str>) {
        match self.read_logs() {
            Ok(mut entries) => {
                // Sort by timestamp
                entries.sort_by(|a, b| match (&a.timestamp, &b.timestamp) {
                    (Some(ta), Some(tb)) => ta.cmp(tb),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                });

                let filter_lower = filter.map(|f| f.to_lowercase());
                let mut count = 0;

                println!("--- Peer Logs {} ---",
                    filter.map(|f| format!("(filtered: '{}')", f)).unwrap_or_default());

                for entry in &entries {
                    let matches = filter_lower
                        .as_ref()
                        .map(|f| entry.message.to_lowercase().contains(f))
                        .unwrap_or(true);

                    if matches {
                        let ts = entry.timestamp_raw.as_deref().unwrap_or("?");
                        let level = entry.level.as_deref().unwrap_or("?");
                        println!("[{}] {} [{}] {}", entry.peer_id, ts, level, entry.message);
                        count += 1;
                    }
                }

                println!("--- End Peer Logs ({} entries) ---", count);
            }
            Err(e) => {
                println!("--- Failed to read logs: {} ---", e);
            }
        }
    }

    /// Dump logs related to connection establishment and NAT traversal
    ///
    /// Filters for: hole punch, NAT, connect, acceptor, joiner, handshake
    pub fn dump_connection_logs(&self) {
        match self.read_logs() {
            Ok(mut entries) => {
                entries.sort_by(|a, b| match (&a.timestamp, &b.timestamp) {
                    (Some(ta), Some(tb)) => ta.cmp(tb),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                });

                let keywords = [
                    "hole", "punch", "nat", "traverse", "acceptor", "joiner",
                    "handshake", "outbound", "inbound", "connect:", "connection",
                ];

                println!("--- Connection/NAT Logs ---");
                let mut count = 0;

                for entry in &entries {
                    let msg_lower = entry.message.to_lowercase();
                    let matches = keywords.iter().any(|kw| msg_lower.contains(kw));

                    if matches {
                        let ts = entry.timestamp_raw.as_deref().unwrap_or("?");
                        let level = entry.level.as_deref().unwrap_or("?");
                        println!("[{}] {} [{}] {}", entry.peer_id, ts, level, entry.message);
                        count += 1;
                    }
                }

                println!("--- End Connection/NAT Logs ({} entries) ---", count);
            }
            Err(e) => {
                println!("--- Failed to read logs: {} ---", e);
            }
        }
    }

    /// Dump iptables counters from all NAT routers (Docker NAT only)
    ///
    /// Prints NAT table rules and FORWARD chain counters for debugging.
    pub async fn dump_iptables(&self) {
        if let Some(backend) = &self.docker_backend {
            match backend.dump_iptables_counters().await {
                Ok(results) => {
                    println!("--- NAT Router iptables ---");
                    for (peer_idx, output) in results.iter() {
                        println!("=== Peer {} NAT Router ===", peer_idx);
                        println!("{}", output);
                    }
                    println!("--- End NAT Router iptables ---");
                }
                Err(e) => {
                    println!("--- Failed to dump iptables: {} ---", e);
                }
            }
        } else {
            println!("--- No Docker NAT backend (iptables not available) ---");
        }
    }

    /// Dump conntrack table from all NAT routers (Docker NAT only)
    ///
    /// Shows active UDP connection tracking entries for debugging NAT issues.
    pub async fn dump_conntrack(&self) {
        if let Some(backend) = &self.docker_backend {
            match backend.dump_conntrack_table().await {
                Ok(results) => {
                    println!("--- NAT Router conntrack ---");
                    for (peer_idx, output) in results.iter() {
                        println!("=== Peer {} NAT Router ===", peer_idx);
                        println!("{}", output);
                    }
                    println!("--- End NAT Router conntrack ---");
                }
                Err(e) => {
                    println!("--- Failed to dump conntrack: {} ---", e);
                }
            }
        } else {
            println!("--- No Docker NAT backend (conntrack not available) ---");
        }
    }

    /// Dump routing tables from all peer containers (Docker NAT only)
    ///
    /// Shows ip route output for debugging routing issues.
    pub async fn dump_peer_routes(&self) {
        if let Some(backend) = &self.docker_backend {
            match backend.dump_peer_routes().await {
                Ok(results) => {
                    println!("--- Peer routing tables ---");
                    for (peer_idx, output) in results.iter() {
                        println!("=== Peer {} routes ===", peer_idx);
                        println!("{}", output);
                    }
                    println!("--- End peer routing tables ---");
                }
                Err(e) => {
                    println!("--- Failed to dump peer routes: {} ---", e);
                }
            }
        } else {
            println!("--- No Docker NAT backend (peer routes not available) ---");
        }
    }
}

impl TestNetwork {
    pub(crate) fn new(
        gateways: Vec<TestPeer>,
        peers: Vec<TestPeer>,
        min_connectivity: f64,
        run_root: PathBuf,
    ) -> Self {
        Self {
            gateways,
            peers,
            min_connectivity,
            run_root,
            docker_backend: None,
        }
    }

    pub(crate) fn new_with_docker(
        gateways: Vec<TestPeer>,
        peers: Vec<TestPeer>,
        min_connectivity: f64,
        run_root: PathBuf,
        docker_backend: Option<DockerNatBackend>,
    ) -> Self {
        Self {
            gateways,
            peers,
            min_connectivity,
            run_root,
            docker_backend,
        }
    }

    /// Directory containing all peer state/logs for this test network run.
    pub fn run_root(&self) -> &std::path::Path {
        &self.run_root
    }
}

/// Snapshot describing diagnostics collected across the network at a moment in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkDiagnosticsSnapshot {
    pub collected_at: chrono::DateTime<Utc>,
    pub peers: Vec<PeerDiagnosticsSnapshot>,
}

/// Diagnostic information for a single peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerDiagnosticsSnapshot {
    pub peer_id: String,
    pub is_gateway: bool,
    pub ws_url: String,
    pub location: Option<String>,
    pub listening_address: Option<String>,
    pub connected_peer_ids: Vec<String>,
    pub connected_peers_detailed: Vec<ConnectedPeerInfo>,
    pub active_connections: Option<usize>,
    pub system_metrics: Option<SystemMetrics>,
    pub error: Option<String>,
}

impl PeerDiagnosticsSnapshot {
    fn new(peer: &TestPeer) -> Self {
        Self {
            peer_id: peer.id().to_string(),
            is_gateway: peer.is_gateway(),
            ws_url: peer.ws_url(),
            location: None,
            listening_address: None,
            connected_peer_ids: Vec::new(),
            connected_peers_detailed: Vec::new(),
            active_connections: None,
            system_metrics: None,
            error: None,
        }
    }
}

/// Snapshot describing a peer's ring metadata and adjacency.
#[derive(Debug, Clone, Serialize)]
pub struct RingPeerSnapshot {
    pub id: String,
    pub is_gateway: bool,
    pub ws_port: u16,
    pub network_port: u16,
    pub network_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<f64>,
    pub connections: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract: Option<PeerContractStatus>,
}

/// Convert a diagnostics snapshot into the ring snapshot format used by the visualization.
pub fn ring_nodes_from_diagnostics(snapshot: &NetworkDiagnosticsSnapshot) -> Vec<RingPeerSnapshot> {
    snapshot
        .peers
        .iter()
        .map(|peer| {
            let location = peer
                .location
                .as_deref()
                .and_then(|loc| loc.parse::<f64>().ok());
            let (network_address, network_port) =
                parse_listening_address(peer.listening_address.as_ref(), &peer.ws_url);
            let ws_port = parse_ws_port(&peer.ws_url);
            let mut connections = peer.connected_peer_ids.clone();
            connections.retain(|id| id != &peer.peer_id);
            connections.sort();
            connections.dedup();

            RingPeerSnapshot {
                id: peer.peer_id.clone(),
                is_gateway: peer.is_gateway,
                ws_port,
                network_port,
                network_address,
                location,
                connections,
                contract: None,
            }
        })
        .collect()
}

/// Render a ring visualization from a saved diagnostics snapshot (e.g. a large soak run).
pub fn write_ring_visualization_from_diagnostics<P: AsRef<Path>, Q: AsRef<Path>>(
    snapshot: &NetworkDiagnosticsSnapshot,
    run_root: P,
    output_path: Q,
) -> Result<()> {
    let nodes = ring_nodes_from_diagnostics(snapshot);
    let metrics = compute_ring_metrics(&nodes);
    let payload = json!({
        "generated_at": snapshot.collected_at.to_rfc3339(),
        "run_root": run_root.as_ref().display().to_string(),
        "nodes": nodes,
        "metrics": metrics,
    });
    let data_json = serde_json::to_string(&payload).map_err(|e| Error::Other(e.into()))?;
    let html = render_ring_template(&data_json);
    let out_path = output_path.as_ref();
    if let Some(parent) = out_path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(out_path, html)?;
    tracing::info!(path = %out_path.display(), "Wrote ring visualization from diagnostics");
    Ok(())
}

/// Contract-specific status for a peer when a contract key is provided.
#[derive(Debug, Clone, Serialize)]
pub struct PeerContractStatus {
    pub stores_contract: bool,
    pub subscribed_locally: bool,
    pub subscriber_peer_ids: Vec<String>,
    pub subscriber_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContractVizData {
    pub key: String,
    pub location: f64,
    pub caching_peers: Vec<String>,
    pub put: OperationPath,
    pub update: OperationPath,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OperationPath {
    pub edges: Vec<ContractOperationEdge>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_peer: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContractOperationEdge {
    pub from: String,
    pub to: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_source: Option<String>,
    pub message: String,
}

/// Aggregated metrics that help reason about small-world properties.
#[derive(Debug, Clone, Serialize)]
pub struct RingVizMetrics {
    pub node_count: usize,
    pub gateway_count: usize,
    pub edge_count: usize,
    pub average_degree: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_ring_distance: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_ring_distance: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_ring_distance: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pct_edges_under_5pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pct_edges_under_10pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_over_long_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub distance_histogram: Vec<RingDistanceBucket>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RingDistanceBucket {
    pub upper_bound: f64,
    pub count: usize,
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

#[derive(Default)]
struct ContractFlowData {
    put_edges: Vec<ContractOperationEdge>,
    put_completion_peer: Option<String>,
    update_edges: Vec<ContractOperationEdge>,
    update_completion_peer: Option<String>,
    errors: Vec<String>,
}

async fn query_ring_snapshot(
    peer: &TestPeer,
    instance_id: Option<&ContractInstanceId>,
) -> Result<RingPeerSnapshot> {
    use tokio_tungstenite::connect_async;

    let url = format!("{}?encodingProtocol=native", peer.ws_url());
    let (ws_stream, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(&url))
        .await
        .map_err(|_| Error::ConnectivityFailed(format!("Timeout connecting to {}", peer.id())))?
        .map_err(|e| {
            Error::ConnectivityFailed(format!("Failed to connect to {}: {}", peer.id(), e))
        })?;

    let mut client = WebApi::start(ws_stream);
    let diag_config = if let Some(id) = instance_id {
        // Create a ContractKey with placeholder code hash for the diagnostics API.
        // The code hash is not used for contract lookup/filtering, only the instance ID matters.
        let placeholder_key =
            ContractKey::from_id_and_code(*id, CodeHash::new([0u8; 32]));
        NodeDiagnosticsConfig {
            include_node_info: true,
            include_network_info: true,
            include_subscriptions: true,
            contract_keys: vec![placeholder_key],
            include_system_metrics: false,
            include_detailed_peer_info: true,
            include_subscriber_peer_ids: true,
        }
    } else {
        NodeDiagnosticsConfig::basic_status()
    };

    client
        .send(ClientRequest::NodeQueries(NodeQuery::NodeDiagnostics {
            config: diag_config,
        }))
        .await
        .map_err(|e| {
            Error::ConnectivityFailed(format!(
                "Failed to send diagnostics to {}: {}",
                peer.id(),
                e
            ))
        })?;

    let response = tokio::time::timeout(Duration::from_secs(5), client.recv())
        .await
        .map_err(|_| {
            Error::ConnectivityFailed(format!(
                "Timeout waiting for diagnostics response from {}",
                peer.id()
            ))
        })?;

    let diag = match response {
        Ok(HostResponse::QueryResponse(QueryResponse::NodeDiagnostics(diag))) => diag,
        Ok(other) => {
            graceful_disconnect(client, "ring snapshot error").await;
            return Err(Error::ConnectivityFailed(format!(
                "Unexpected diagnostics response from {}: {:?}",
                peer.id(),
                other
            )));
        }
        Err(e) => {
            graceful_disconnect(client, "ring snapshot error").await;
            return Err(Error::ConnectivityFailed(format!(
                "Diagnostics query failed for {}: {}",
                peer.id(),
                e
            )));
        }
    };

    let node_info = diag.node_info.ok_or_else(|| {
        Error::ConnectivityFailed(format!("{} did not return node_info", peer.id()))
    })?;

    let location = node_info
        .location
        .as_deref()
        .and_then(|value| value.parse::<f64>().ok());

    let mut connections: Vec<String> = diag
        .connected_peers_detailed
        .into_iter()
        .map(|info| info.peer_id)
        .collect();

    if connections.is_empty() {
        if let Some(network_info) = diag.network_info {
            connections = network_info
                .connected_peers
                .into_iter()
                .map(|(peer_id, _)| peer_id)
                .collect();
        }
    }

    connections.retain(|conn| conn != &node_info.peer_id);
    connections.sort();
    connections.dedup();

    let contract_status = instance_id.and_then(|id| {
        // Create placeholder key for contract_states lookup (keyed by ContractKey)
        let placeholder_key = ContractKey::from_id_and_code(*id, CodeHash::new([0u8; 32]));
        let (stores_contract, subscriber_count, subscriber_peer_ids) =
            if let Some(state) = diag.contract_states.get(&placeholder_key) {
                (
                    true,
                    state.subscribers as usize,
                    state.subscriber_peer_ids.clone(),
                )
            } else {
                (false, 0, Vec::new())
            };
        // SubscriptionInfo.contract_key is now ContractInstanceId
        let subscribed_locally = diag
            .subscriptions
            .iter()
            .any(|sub| &sub.contract_key == id);
        if stores_contract || subscribed_locally {
            Some(PeerContractStatus {
                stores_contract,
                subscribed_locally,
                subscriber_peer_ids,
                subscriber_count,
            })
        } else {
            None
        }
    });

    graceful_disconnect(client, "ring snapshot complete").await;

    Ok(RingPeerSnapshot {
        id: node_info.peer_id,
        is_gateway: node_info.is_gateway,
        ws_port: peer.ws_port,
        network_port: peer.network_port,
        network_address: peer.network_address.clone(),
        location,
        connections,
        contract: contract_status,
    })
}

fn compute_ring_metrics(nodes: &[RingPeerSnapshot]) -> RingVizMetrics {
    let node_count = nodes.len();
    let gateway_count = nodes.iter().filter(|peer| peer.is_gateway).count();
    let mut total_degree = 0usize;
    let mut unique_edges: HashSet<(String, String)> = HashSet::new();

    for node in nodes {
        total_degree += node.connections.len();
        for neighbor in &node.connections {
            if neighbor == &node.id {
                continue;
            }
            let edge = if node.id < *neighbor {
                (node.id.clone(), neighbor.clone())
            } else {
                (neighbor.clone(), node.id.clone())
            };
            unique_edges.insert(edge);
        }
    }

    let average_degree = if node_count == 0 {
        0.0
    } else {
        total_degree as f64 / node_count as f64
    };

    let mut location_lookup = HashMap::new();
    for node in nodes {
        if let Some(loc) = node.location {
            location_lookup.insert(node.id.clone(), loc);
        }
    }

    let mut distances = Vec::new();
    for (a, b) in &unique_edges {
        if let (Some(loc_a), Some(loc_b)) = (location_lookup.get(a), location_lookup.get(b)) {
            let mut distance = (loc_a - loc_b).abs();
            if distance > 0.5 {
                distance = 1.0 - distance;
            }
            distances.push(distance);
        }
    }

    let average_ring_distance = if distances.is_empty() {
        None
    } else {
        Some(distances.iter().sum::<f64>() / distances.len() as f64)
    };
    let min_ring_distance = distances.iter().cloned().reduce(f64::min);
    let max_ring_distance = distances.iter().cloned().reduce(f64::max);
    let pct_edges_under_5pct = calculate_percentage(&distances, 0.05);
    let pct_edges_under_10pct = calculate_percentage(&distances, 0.10);

    let short_edges = distances.iter().filter(|d| **d <= 0.10).count();
    let long_edges = distances
        .iter()
        .filter(|d| **d > 0.10 && **d <= 0.50)
        .count();
    let short_over_long_ratio = if short_edges == 0 || long_edges == 0 {
        None
    } else {
        Some(short_edges as f64 / long_edges as f64)
    };

    let mut distance_histogram = Vec::new();
    let bucket_bounds: Vec<f64> = (1..=5).map(|i| i as f64 * 0.10).collect(); // 0.1 buckets up to 0.5
    let mut cumulative = 0usize;
    for bound in bucket_bounds {
        let up_to_bound = distances.iter().filter(|d| **d <= bound).count();
        let count = up_to_bound.saturating_sub(cumulative);
        cumulative = up_to_bound;
        distance_histogram.push(RingDistanceBucket {
            upper_bound: bound,
            count,
        });
    }

    RingVizMetrics {
        node_count,
        gateway_count,
        edge_count: unique_edges.len(),
        average_degree,
        average_ring_distance,
        min_ring_distance,
        max_ring_distance,
        pct_edges_under_5pct,
        pct_edges_under_10pct,
        short_over_long_ratio,
        distance_histogram,
    }
}

fn calculate_percentage(distances: &[f64], threshold: f64) -> Option<f64> {
    if distances.is_empty() {
        return None;
    }
    let matching = distances.iter().filter(|value| **value < threshold).count();
    Some((matching as f64 / distances.len() as f64) * 100.0)
}

fn parse_ws_port(ws_url: &str) -> u16 {
    ws_url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|host_port| host_port.split(':').nth(1))
        .and_then(|port| port.parse().ok())
        .unwrap_or(0)
}

fn parse_listening_address(addr: Option<&String>, ws_url: &str) -> (String, u16) {
    if let Some(addr) = addr {
        let mut parts = addr.split(':');
        let host = parts.next().unwrap_or("").to_string();
        let port = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        (host, port)
    } else {
        let host = ws_url
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .and_then(|host_port| host_port.split(':').next())
            .unwrap_or_default()
            .to_string();
        (host, 0)
    }
}

fn resolve_peer_id(prefix: &str, peers: &[RingPeerSnapshot]) -> Option<String> {
    let needle = prefix.trim().trim_matches(|c| c == '"' || c == '\'');
    peers
        .iter()
        .find(|peer| peer.id.starts_with(needle))
        .map(|peer| peer.id.clone())
}

fn contract_location_from_instance_id(id: &ContractInstanceId) -> f64 {
    let mut value = 0.0;
    let mut divisor = 256.0;
    for byte in id.as_bytes() {
        value += f64::from(*byte) / divisor;
        divisor *= 256.0;
    }
    value.fract()
}

static PUT_REQUEST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Requesting put for contract (?P<key>\S+) from (?P<from>\S+) to (?P<to>\S+)")
        .expect("valid regex")
});
static PUT_COMPLETION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Peer completed contract value put,.*key: (?P<key>\S+),.*this_peer: (?P<peer>\S+)")
        .expect("valid regex")
});
static UPDATE_PROPAGATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"UPDATE_PROPAGATION: contract=(?P<contract>\S+) from=(?P<from>\S+) targets=(?P<targets>[^ ]*)\s+count=",
    )
    .expect("valid regex")
});
static UPDATE_NO_TARGETS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"UPDATE_PROPAGATION: contract=(?P<contract>\S+) from=(?P<from>\S+) NO_TARGETS")
        .expect("valid regex")
});

const HTML_TEMPLATE: &str = r###"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <title>Freenet Ring Topology</title>
  <style>
    :root {
      color-scheme: dark;
      font-family: "Inter", "Helvetica Neue", Arial, sans-serif;
    }
    body {
      background: #020617;
      color: #e2e8f0;
      margin: 0;
      padding: 32px;
      display: flex;
      justify-content: center;
      min-height: 100vh;
    }
    #container {
      max-width: 1000px;
      width: 100%;
      background: #0f172a;
      border-radius: 18px;
      padding: 28px 32px 40px;
      box-shadow: 0 40px 120px rgba(2, 6, 23, 0.85);
    }
    h1 {
      margin: 0 0 8px;
      font-size: 26px;
      letter-spacing: 0.4px;
      color: #f8fafc;
    }
    #meta {
      margin: 0 0 20px;
      color: #94a3b8;
      font-size: 14px;
    }
    canvas {
      width: 100%;
      max-width: 900px;
      height: auto;
      background: radial-gradient(circle, #020617 0%, #0f172a 70%, #020617 100%);
      border-radius: 16px;
      border: 1px solid rgba(148, 163, 184, 0.1);
      display: block;
      margin: 0 auto 24px;
    }
    #metrics {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
      gap: 12px;
      margin-bottom: 20px;
      font-size: 14px;
    }
    #metrics div {
      background: #1e293b;
      padding: 12px 14px;
      border-radius: 12px;
      border: 1px solid rgba(148, 163, 184, 0.15);
    }
    .legend {
      display: flex;
      flex-wrap: wrap;
      gap: 18px;
      margin-bottom: 24px;
      font-size: 14px;
      color: #cbd5f5;
    }
    .legend span {
      display: flex;
      align-items: center;
      gap: 6px;
    }
    .legend .line {
      width: 30px;
      height: 6px;
      border-radius: 3px;
      display: inline-block;
    }
    .legend .put-line {
      background: #22c55e;
    }
    .legend .update-line {
      background: #f59e0b;
    }
    .legend .contract-dot {
      background: #a855f7;
      box-shadow: 0 0 6px rgba(168, 85, 247, 0.7);
    }
    .legend .cached-dot {
      background: #38bdf8;
    }
    .legend .peer-dot {
      background: #64748b;
    }
    .legend .gateway-dot {
      background: #f97316;
    }
    .dot {
      width: 14px;
      height: 14px;
      border-radius: 50%;
      display: inline-block;
    }
    #contract-info {
      background: rgba(15, 23, 42, 0.7);
      border-radius: 12px;
      border: 1px solid rgba(148, 163, 184, 0.15);
      padding: 14px;
      margin-bottom: 20px;
      font-size: 13px;
      color: #cbd5f5;
    }
    #contract-info h2 {
      margin: 0 0 8px;
      font-size: 16px;
      color: #f8fafc;
    }
    #contract-info .op-block {
      margin-top: 10px;
    }
    #contract-info ol {
      padding-left: 20px;
      margin: 6px 0;
    }
    #contract-info .errors {
      margin-top: 10px;
      color: #f97316;
    }
    #peer-list {
      border-top: 1px solid rgba(148, 163, 184, 0.15);
      padding-top: 18px;
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
      gap: 14px;
      font-size: 13px;
    }
    .peer {
      background: rgba(15, 23, 42, 0.75);
      padding: 12px 14px;
      border-radius: 12px;
      border: 1px solid rgba(148, 163, 184, 0.2);
    }
    .peer strong {
      display: block;
      font-size: 13px;
      color: #f1f5f9;
      margin-bottom: 6px;
      word-break: break-all;
    }
    .peer span {
      display: block;
      color: #94a3b8;
    }
    .chart-card {
      background: rgba(15, 23, 42, 0.75);
      padding: 12px 14px;
      border-radius: 12px;
      border: 1px solid rgba(148, 163, 184, 0.2);
      margin-top: 12px;
    }
    .chart-card h3 {
      margin: 0 0 8px;
      font-size: 15px;
      color: #e2e8f0;
    }
  </style>
  <script src="https://cdn.jsdelivr.net/npm/chart.js@4.4.1/dist/chart.umd.min.js"></script>
</head>
<body>
  <div id="container">
    <h1>Freenet Ring Topology</h1>
    <p id="meta"></p>
    <canvas id="ring" width="900" height="900"></canvas>
    <div id="metrics"></div>
    <div class="chart-card" style="height: 280px;">
      <h3>Ring distance distribution</h3>
      <canvas id="histogram-chart" style="height: 220px;"></canvas>
    </div>
    <div class="legend">
      <span><span class="dot peer-dot"></span>Peer</span>
      <span><span class="dot gateway-dot"></span>Gateway</span>
      <span><span class="dot cached-dot"></span>Cached peer</span>
      <span><span class="dot contract-dot"></span>Contract</span>
      <span><span class="line put-line"></span>PUT path</span>
      <span><span class="line update-line"></span>UPDATE path</span>
      <span><span class="dot" style="background:#475569"></span>Connection</span>
    </div>
    <div id="contract-info"></div>
    <div id="peer-list"></div>
  </div>
  <script>
    const vizData = __DATA__;
    const contractData = vizData.contract ?? null;
    const metaEl = document.getElementById("meta");
    metaEl.textContent = vizData.nodes.length
      ? `Captured ${vizData.nodes.length} nodes · ${vizData.metrics.edge_count} edges · Generated ${vizData.generated_at} · Run root: ${vizData.run_root}`
      : "No peers reported diagnostics data.";

    const metricsEl = document.getElementById("metrics");
    const fmtNumber = (value, digits = 2) => (typeof value === "number" ? value.toFixed(digits) : "n/a");
    const fmtPercent = (value, digits = 1) => (typeof value === "number" ? `${value.toFixed(digits)}%` : "n/a");

    metricsEl.innerHTML = `
      <div><strong>Total nodes</strong><br/>${vizData.metrics.node_count} (gateways: ${vizData.metrics.gateway_count})</div>
      <div><strong>Edges</strong><br/>${vizData.metrics.edge_count}</div>
      <div><strong>Average degree</strong><br/>${fmtNumber(vizData.metrics.average_degree)}</div>
      <div><strong>Average ring distance</strong><br/>${fmtNumber(vizData.metrics.average_ring_distance, 3)}</div>
      <div><strong>Min / Max ring distance</strong><br/>${fmtNumber(vizData.metrics.min_ring_distance, 3)} / ${fmtNumber(vizData.metrics.max_ring_distance, 3)}</div>
      <div><strong>Edges &lt;5% / &lt;10%</strong><br/>${fmtPercent(vizData.metrics.pct_edges_under_5pct)} / ${fmtPercent(vizData.metrics.pct_edges_under_10pct)}</div>
      <div><strong>Short/long edge ratio (≤0.1 / 0.1–0.5)</strong><br/>${fmtNumber(vizData.metrics.short_over_long_ratio, 2)}</div>
    `;

    const histogram = vizData.metrics.distance_histogram ?? [];
    const labels = histogram.map((b, idx) => {
      const lower = idx === 0 ? 0 : histogram[idx - 1].upper_bound;
      return `${lower.toFixed(1)}–${b.upper_bound.toFixed(1)}`;
    });
    const counts = histogram.map((b) => b.count);

    if (histogram.length && window.Chart) {
      const ctx = document.getElementById("histogram-chart").getContext("2d");
      new Chart(ctx, {
        data: {
          labels,
          datasets: [
            {
              type: "bar",
              label: "Edges per bucket",
              data: counts,
              backgroundColor: "rgba(56, 189, 248, 0.75)",
              borderColor: "#38bdf8",
              borderWidth: 1,
            },
          ],
        },
        options: {
          responsive: true,
          maintainAspectRatio: false,
          animation: false,
          interaction: { mode: "index", intersect: false },
          plugins: {
            legend: { position: "bottom" },
            tooltip: {
              callbacks: {
                label: (ctx) => {
                  const label = ctx.dataset.label || "";
                  const value = ctx.parsed.y;
                  return ctx.dataset.type === "line"
                    ? `${label}: ${value.toFixed(1)}%`
                    : `${label}: ${value}`;
                },
              },
            },
          },
          scales: {
            x: { title: { display: true, text: "Ring distance (fraction of circumference)" } },
            y: {
              beginAtZero: true,
              title: { display: true, text: "Edge count" },
              grid: { color: "rgba(148,163,184,0.15)" },
            },
          },
        },
      });
    }

    const peersEl = document.getElementById("peer-list");
    peersEl.innerHTML = vizData.nodes
      .map((node) => {
        const role = node.is_gateway ? "Gateway" : "Peer";
        const loc = typeof node.location === "number" ? node.location.toFixed(6) : "unknown";
        return `<div class="peer">
          <strong>${node.id}</strong>
          <span>${role}</span>
          <span>Location: ${loc}</span>
          <span>Degree: ${node.connections.length}</span>
        </div>`;
      })
      .join("");

    const contractInfoEl = document.getElementById("contract-info");
    const canvas = document.getElementById("ring");
    const ctx = canvas.getContext("2d");
    const width = canvas.width;
    const height = canvas.height;
    const center = { x: width / 2, y: height / 2 };
    const radius = Math.min(width, height) * 0.37;
    const cachingPeers = new Set(contractData?.caching_peers ?? []);
    const putEdges = contractData?.put?.edges ?? [];
    const updateEdges = contractData?.update?.edges ?? [];
    const contractLocation = typeof (contractData?.location) === "number" ? contractData.location : null;

    function shortId(id) {
      return id.slice(-6);
    }

    function angleFromLocation(location) {
      return (location % 1) * Math.PI * 2;
    }

    function polarToCartesian(angle, r = radius) {
      const theta = angle - Math.PI / 2;
      return {
        x: center.x + r * Math.cos(theta),
        y: center.y + r * Math.sin(theta),
      };
    }

    const nodes = vizData.nodes.map((node, idx) => {
      const angle = typeof node.location === "number"
        ? angleFromLocation(node.location)
        : (idx / vizData.nodes.length) * Math.PI * 2;
      const coords = polarToCartesian(angle);
      return {
        ...node,
        angle,
        x: coords.x,
        y: coords.y,
      };
    });
    const nodeMap = new Map(nodes.map((node) => [node.id, node]));

    function drawRingBase() {
      ctx.save();
      ctx.strokeStyle = "#475569";
      ctx.lineWidth = 2;
      ctx.beginPath();
      ctx.arc(center.x, center.y, radius, 0, Math.PI * 2);
      ctx.stroke();
      ctx.setLineDash([4, 8]);
      ctx.strokeStyle = "rgba(148,163,184,0.3)";
      [0, 0.25, 0.5, 0.75].forEach((loc) => {
        const angle = angleFromLocation(loc);
        const inner = polarToCartesian(angle, radius * 0.9);
        const outer = polarToCartesian(angle, radius * 1.02);
        ctx.beginPath();
        ctx.moveTo(inner.x, inner.y);
        ctx.lineTo(outer.x, outer.y);
        ctx.stroke();
        ctx.fillStyle = "#94a3b8";
        ctx.font = "11px 'Fira Code', monospace";
        ctx.textAlign = "center";
        ctx.fillText(loc.toFixed(2), outer.x, outer.y - 6);
      });
      ctx.restore();
    }

    function drawConnections() {
      ctx.save();
      ctx.strokeStyle = "rgba(148, 163, 184, 0.35)";
      ctx.lineWidth = 1.2;
      const drawn = new Set();
      nodes.forEach((node) => {
        node.connections.forEach((neighborId) => {
          const neighbor = nodeMap.get(neighborId);
          if (!neighbor) return;
          const key = node.id < neighborId ? `${node.id}|${neighborId}` : `${neighborId}|${node.id}`;
          if (drawn.has(key)) return;
          drawn.add(key);
          ctx.beginPath();
          ctx.moveTo(node.x, node.y);
          ctx.lineTo(neighbor.x, neighbor.y);
          ctx.stroke();
        });
      });
      ctx.restore();
    }

    function drawContractMarker() {
      if (typeof contractLocation !== "number") return;
      const pos = polarToCartesian(angleFromLocation(contractLocation));
      ctx.save();
      ctx.fillStyle = "#a855f7";
      ctx.beginPath();
      ctx.arc(pos.x, pos.y, 9, 0, Math.PI * 2);
      ctx.fill();
      ctx.lineWidth = 2;
      ctx.strokeStyle = "#f3e8ff";
      ctx.stroke();
      ctx.fillStyle = "#f3e8ff";
      ctx.font = "10px 'Fira Code', monospace";
      ctx.textAlign = "center";
      ctx.fillText("contract", pos.x, pos.y - 16);
      ctx.restore();
    }

    function drawPeers() {
      nodes.forEach((node) => {
        const baseColor = node.is_gateway ? "#f97316" : "#64748b";
        const fill = cachingPeers.has(node.id) ? "#38bdf8" : baseColor;
        ctx.save();
        ctx.beginPath();
        ctx.fillStyle = fill;
        ctx.arc(node.x, node.y, 6.5, 0, Math.PI * 2);
        ctx.fill();
        ctx.lineWidth = 1.6;
        ctx.strokeStyle = "#0f172a";
        ctx.stroke();
        ctx.fillStyle = "#f8fafc";
        ctx.font = "12px 'Fira Code', monospace";
        ctx.textAlign = "center";
        ctx.fillText(shortId(node.id), node.x, node.y - 14);
        const locText = typeof node.location === "number" ? node.location.toFixed(3) : "n/a";
        ctx.fillStyle = "#94a3b8";
        ctx.font = "10px 'Fira Code', monospace";
        ctx.fillText(locText, node.x, node.y + 20);
        ctx.restore();
      });
    }

    function drawOperationEdges(edges, color, dashed = false) {
      if (!edges.length) return;
      ctx.save();
      ctx.strokeStyle = color;
      ctx.lineWidth = 3;
      if (dashed) ctx.setLineDash([10, 8]);
      edges.forEach((edge, idx) => {
        const from = nodeMap.get(edge.from);
        const to = nodeMap.get(edge.to);
        if (!from || !to) return;
        ctx.beginPath();
        ctx.moveTo(from.x, from.y);
        ctx.lineTo(to.x, to.y);
        ctx.stroke();
        drawArrowhead(from, to, color);
        const midX = (from.x + to.x) / 2;
        const midY = (from.y + to.y) / 2;
        ctx.fillStyle = color;
        ctx.font = "11px 'Fira Code', monospace";
        ctx.fillText(`#${idx + 1}`, midX, midY - 4);
      });
      ctx.restore();
    }

    function drawArrowhead(from, to, color) {
      const angle = Math.atan2(to.y - from.y, to.x - from.x);
      const length = 12;
      const spread = Math.PI / 6;
      ctx.save();
      ctx.fillStyle = color;
      ctx.beginPath();
      ctx.moveTo(to.x, to.y);
      ctx.lineTo(
        to.x - length * Math.cos(angle - spread),
        to.y - length * Math.sin(angle - spread)
      );
      ctx.lineTo(
        to.x - length * Math.cos(angle + spread),
        to.y - length * Math.sin(angle + spread)
      );
      ctx.closePath();
      ctx.fill();
      ctx.restore();
    }

    function renderOperationList(title, edges) {
      if (!edges.length) {
        return `<div class="op-block"><strong>${title}:</strong> none recorded</div>`;
      }
      const items = edges
        .map((edge, idx) => {
          const ts = edge.timestamp
            ? new Date(edge.timestamp).toLocaleTimeString()
            : "no-ts";
          return `<li>#${idx + 1}: ${shortId(edge.from)} → ${shortId(edge.to)} (${ts})</li>`;
        })
        .join("");
      return `<div class="op-block"><strong>${title}:</strong><ol>${items}</ol></div>`;
    }

    function renderContractInfo(data) {
      if (!data) {
        contractInfoEl.textContent = "No contract-specific data collected for this snapshot.";
        return;
      }
      const errors = data.errors?.length
        ? `<div class="errors"><strong>Notable events:</strong><ul>${data.errors
            .map((msg) => `<li>${msg}</li>`)
            .join("")}</ul></div>`
        : "";
      const putSummary = renderOperationList("PUT path", data.put.edges);
      const updateSummary = renderOperationList("UPDATE path", data.update.edges);
      contractInfoEl.innerHTML = `
        <h2>Contract ${data.key}</h2>
        <div><strong>Cached peers:</strong> ${data.caching_peers.length}</div>
        <div><strong>PUT completion:</strong> ${
          data.put.completion_peer ?? "pending"
        }</div>
        <div><strong>UPDATE completion:</strong> ${
          data.update.completion_peer ?? "pending"
        }</div>
        ${putSummary}
        ${updateSummary}
        ${errors}
      `;
    }

    renderContractInfo(contractData);

    if (nodes.length) {
      drawRingBase();
      drawConnections();
      drawContractMarker();
      drawOperationEdges(putEdges, "#22c55e", false);
      drawOperationEdges(updateEdges, "#f59e0b", true);
      drawPeers();
    } else {
      ctx.fillStyle = "#94a3b8";
      ctx.font = "16px Inter, sans-serif";
      ctx.fillText("No diagnostics available to render ring.", center.x - 140, center.y);
    }
  </script>
</body>
</html>
"###;

fn render_ring_template(data_json: &str) -> String {
    HTML_TEMPLATE.replace("__DATA__", data_json)
}

#[cfg(test)]
mod tests {
    use super::{compute_ring_metrics, RingPeerSnapshot};

    #[test]
    fn ring_metrics_basic() {
        let nodes = vec![
            RingPeerSnapshot {
                id: "a".into(),
                is_gateway: false,
                ws_port: 0,
                network_port: 0,
                network_address: "127.1.0.1".into(),
                location: Some(0.1),
                connections: vec!["b".into(), "c".into()],
                contract: None,
            },
            RingPeerSnapshot {
                id: "b".into(),
                is_gateway: false,
                ws_port: 0,
                network_port: 0,
                network_address: "127.2.0.1".into(),
                location: Some(0.2),
                connections: vec!["a".into()],
                contract: None,
            },
            RingPeerSnapshot {
                id: "c".into(),
                is_gateway: true,
                ws_port: 0,
                network_port: 0,
                network_address: "127.3.0.1".into(),
                location: Some(0.8),
                connections: vec!["a".into()],
                contract: None,
            },
        ];

        let metrics = compute_ring_metrics(&nodes);
        assert_eq!(metrics.node_count, 3);
        assert_eq!(metrics.gateway_count, 1);
        assert_eq!(metrics.edge_count, 2);
        assert!((metrics.average_degree - (4.0 / 3.0)).abs() < f64::EPSILON);
        assert!(metrics.average_ring_distance.is_some());
    }
}
