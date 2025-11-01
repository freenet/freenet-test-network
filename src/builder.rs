use crate::{
    binary::FreenetBinary,
    network::TestNetwork,
    peer::{get_free_port, TestPeer},
    Error, Result,
};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

struct GatewayInfo {
    address: String,
    public_key_path: PathBuf,
}

/// Builder for configuring and creating a test network
pub struct NetworkBuilder {
    gateways: usize,
    peers: usize,
    binary: FreenetBinary,
    min_connectivity: f64,
    connectivity_timeout: Duration,
    preserve_data_on_failure: bool,
}

impl Default for NetworkBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkBuilder {
    pub fn new() -> Self {
        Self {
            gateways: 1,
            peers: 3,
            binary: FreenetBinary::default(),
            min_connectivity: 1.0, // Default: require all peers connected
            connectivity_timeout: Duration::from_secs(30),
            preserve_data_on_failure: false,
        }
    }

    /// Set the number of gateway peers
    pub fn gateways(mut self, n: usize) -> Self {
        self.gateways = n;
        self
    }

    /// Set the number of regular peers
    pub fn peers(mut self, n: usize) -> Self {
        self.peers = n;
        self
    }

    /// Set which freenet binary to use
    pub fn binary(mut self, binary: FreenetBinary) -> Self {
        self.binary = binary;
        self
    }

    /// Set minimum connectivity ratio required (0.0 to 1.0)
    pub fn require_connectivity(mut self, ratio: f64) -> Self {
        self.min_connectivity = ratio;
        self
    }

    /// Set timeout for connectivity check
    pub fn connectivity_timeout(mut self, timeout: Duration) -> Self {
        self.connectivity_timeout = timeout;
        self
    }

    /// Preserve peer data directories in `/tmp` when network startup fails
    pub fn preserve_temp_dirs_on_failure(mut self, preserve: bool) -> Self {
        self.preserve_data_on_failure = preserve;
        self
    }

    /// Build and start the network (async)
    pub async fn build(self) -> Result<TestNetwork> {
        let binary_path = self.binary.resolve()?;

        tracing::info!(
            "Starting test network: {} gateways, {} peers",
            self.gateways,
            self.peers
        );

        // Start gateways first
        let mut gateways = Vec::new();
        for i in 0..self.gateways {
            let peer = self.start_peer(&binary_path, i, true).await?;
            gateways.push(peer);
        }

        // Collect gateway info for peers to connect to
        let gateway_info: Vec<_> = gateways
            .iter()
            .map(|gw| GatewayInfo {
                address: format!("127.0.0.1:{}", gw.network_port),
                public_key_path: gw
                    .public_key_path
                    .clone()
                    .expect("Gateway must have public key"),
            })
            .collect();

        // Start regular peers
        let mut peers = Vec::new();
        for i in 0..self.peers {
            let peer = self
                .start_peer_with_gateways(&binary_path, i + self.gateways, false, &gateway_info)
                .await?;
            peers.push(peer);
        }

        let network = TestNetwork::new(gateways, peers, self.min_connectivity);

        // Wait for network to be ready
        match network
            .wait_until_ready_with_timeout(self.connectivity_timeout)
            .await
        {
            Ok(()) => Ok(network),
            Err(err) => {
                if let Err(log_err) = dump_recent_logs(&network) {
                    eprintln!("Failed to dump logs after connectivity error: {}", log_err);
                }
                if self.preserve_data_on_failure {
                    match preserve_network_state(&network) {
                        Ok(path) => {
                            eprintln!("Network data directories preserved at {}", path.display());
                        }
                        Err(copy_err) => {
                            eprintln!("Failed to preserve network data directories: {}", copy_err);
                        }
                    }
                }
                Err(err)
            }
        }
    }

    /// Build the network synchronously (for use in LazyLock)
    pub fn build_sync(self) -> Result<TestNetwork> {
        tokio::runtime::Runtime::new()?.block_on(self.build())
    }

    async fn start_peer(
        &self,
        binary_path: &PathBuf,
        index: usize,
        is_gateway: bool,
    ) -> Result<TestPeer> {
        self.start_peer_with_gateways(binary_path, index, is_gateway, &[])
            .await
    }

    async fn start_peer_with_gateways(
        &self,
        binary_path: &PathBuf,
        index: usize,
        is_gateway: bool,
        gateway_info: &[GatewayInfo],
    ) -> Result<TestPeer> {
        let ws_port = get_free_port()?;
        let network_port = get_free_port()?;
        let data_dir = TempDir::new()?;

        let id = if is_gateway {
            format!("gw{}", index)
        } else {
            format!("peer{}", index)
        };

        tracing::debug!(
            "Starting {} {} - ws:{} net:{}",
            if is_gateway { "gateway" } else { "peer" },
            id,
            ws_port,
            network_port
        );

        // Generate keypair if gateway
        let (keypair_path, public_key_path) = if is_gateway {
            let keypair = data_dir.path().join("keypair.pem");
            let pubkey = data_dir.path().join("public_key.pem");
            generate_keypair(&keypair, &pubkey)?;
            (Some(keypair), Some(pubkey))
        } else {
            (None, None)
        };

        // Build command
        let mut cmd = Command::new(binary_path);
        cmd.arg("network")
            .arg("--data-dir")
            .arg(data_dir.path())
            .arg("--config-dir")
            .arg(data_dir.path())
            .arg("--ws-api-port")
            .arg(ws_port.to_string())
            .arg("--network-port")
            .arg(network_port.to_string())
            .arg("--skip-load-from-network");

        if is_gateway {
            cmd.arg("--is-gateway");
            if let Some(keypair) = &keypair_path {
                cmd.arg("--transport-keypair").arg(keypair);
            }
            cmd.arg("--public-network-address").arg("127.0.0.1");
            cmd.arg("--public-network-port")
                .arg(network_port.to_string());
        }

        // Add gateway addresses for regular peers
        if !is_gateway && !gateway_info.is_empty() {
            // Write gateways config file with proper TOML format
            let gateways_toml = data_dir.path().join("gateways.toml");
            let mut content = String::new();
            for gw in gateway_info {
                content.push_str(&format!(
                    "[[gateways]]\n\
                     address = {{ hostname = \"{}\" }}\n\
                     public_key = \"{}\"\n\n",
                    gw.address,
                    gw.public_key_path.display()
                ));
            }
            std::fs::write(&gateways_toml, content)?;
        }

        // Set environment and spawn
        let log_file = std::fs::File::create(data_dir.path().join("peer.log"))?;
        cmd.env("RUST_LOG", "info")
            .env("RUST_BACKTRACE", "1")
            .stdout(Stdio::from(log_file.try_clone()?))
            .stderr(Stdio::from(log_file));

        let process = cmd
            .spawn()
            .map_err(|e| Error::PeerStartupFailed(format!("Failed to spawn process: {}", e)))?;

        // Give it a moment to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        Ok(TestPeer {
            id,
            is_gateway,
            ws_port,
            network_port,
            data_dir,
            process: Some(process),
            public_key_path,
        })
    }
}

fn generate_keypair(
    private_key_path: &std::path::Path,
    public_key_path: &std::path::Path,
) -> Result<()> {
    // Generate private key
    let output = Command::new("openssl")
        .args([
            "genpkey",
            "-algorithm",
            "RSA",
            "-out",
            private_key_path.to_str().unwrap(),
            "-pkeyopt",
            "rsa_keygen_bits:2048",
        ])
        .output()?;

    if !output.status.success() {
        return Err(Error::Other(anyhow::anyhow!(
            "Failed to generate private key: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    // Extract public key
    let output = Command::new("openssl")
        .args([
            "rsa",
            "-pubout",
            "-in",
            private_key_path.to_str().unwrap(),
            "-out",
            public_key_path.to_str().unwrap(),
        ])
        .output()?;

    if !output.status.success() {
        return Err(Error::Other(anyhow::anyhow!(
            "Failed to extract public key: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    Ok(())
}

fn dump_recent_logs(network: &TestNetwork) -> Result<()> {
    const MAX_LOG_LINES: usize = 200;

    let mut logs = network.read_logs()?;
    let total = logs.len();
    if total > MAX_LOG_LINES {
        logs.drain(0..(total - MAX_LOG_LINES));
    }

    eprintln!(
        "\n--- Network connectivity check failed; showing {} of {} log entries ---",
        logs.len(),
        total
    );

    for entry in logs {
        let level = entry.level.as_deref().unwrap_or("INFO");
        let ts_display = entry
            .timestamp_raw
            .clone()
            .or_else(|| entry.timestamp.map(|ts| ts.to_rfc3339()));

        if let Some(ts) = ts_display {
            eprintln!("[{}] [{}] {}: {}", entry.peer_id, ts, level, entry.message);
        } else {
            eprintln!("[{}] {}: {}", entry.peer_id, level, entry.message);
        }
    }

    eprintln!("--- End of network logs ---\n");

    Ok(())
}

fn preserve_network_state(network: &TestNetwork) -> Result<PathBuf> {
    let timestamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let root = std::env::temp_dir().join(format!("freenet-test-network-{}", timestamp));
    std::fs::create_dir_all(&root)?;

    for peer in network.gateways.iter().chain(network.peers.iter()) {
        let dest = root.join(peer.id());
        copy_dir_recursive(peer.data_dir_path(), &dest)?;
    }

    Ok(root)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), dest_path)?;
        }
    }

    Ok(())
}
