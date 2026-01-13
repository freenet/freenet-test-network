//! Docker-based NAT simulation backend for testing Freenet in isolated networks.
//!
//! This module provides infrastructure to run Freenet peers in Docker containers
//! behind simulated NAT routers, allowing detection of bugs that only manifest
//! when peers are on different networks.

use crate::{logs::LogEntry, process::PeerProcess, Error, Result};
use bollard::{
    container::{
        Config, CreateContainerOptions, LogOutput, LogsOptions, RemoveContainerOptions,
        StartContainerOptions, StopContainerOptions, UploadToContainerOptions,
    },
    exec::{CreateExecOptions, StartExecResults},
    image::BuildImageOptions,
    network::CreateNetworkOptions,
    secret::{ContainerStateStatusEnum, HostConfig, Ipam, IpamConfig, PortBinding},
    Docker,
};
use futures::StreamExt;
use ipnetwork::Ipv4Network;
use rand::Rng;
use std::{
    collections::HashMap,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    time::Duration,
};

/// Configuration for Docker NAT simulation
#[derive(Debug, Clone)]
pub struct DockerNatConfig {
    /// NAT topology configuration
    pub topology: NatTopology,
    /// Base subnet for public network (gateway network)
    pub public_subnet: Ipv4Network,
    /// Base for private network subnets (each NAT gets one)
    pub private_subnet_base: Ipv4Addr,
    /// Whether to remove containers on drop
    pub cleanup_on_drop: bool,
    /// Prefix for container and network names
    pub name_prefix: String,
    /// Network emulation settings (latency, jitter, packet loss)
    /// If None, no network emulation is applied
    pub network_emulation: Option<NetworkEmulation>,
}

/// Network emulation settings using Linux tc netem.
///
/// These settings simulate realistic network conditions like latency and packet loss.
/// Useful for testing congestion control behavior under real-world conditions.
///
/// # Example
///
/// ```
/// use freenet_test_network::docker::NetworkEmulation;
///
/// // Simulate intercontinental latency (100-150ms) with 1% packet loss
/// let emulation = NetworkEmulation {
///     delay_ms: 125,
///     jitter_ms: 25,
///     loss_percent: 1.0,
///     ..Default::default()
/// };
/// ```
#[derive(Debug, Clone)]
pub struct NetworkEmulation {
    /// Base latency in milliseconds
    pub delay_ms: u32,
    /// Latency jitter (+/- this value) in milliseconds
    pub jitter_ms: u32,
    /// Packet loss percentage (0.0 - 100.0)
    pub loss_percent: f64,
    /// Correlation percentage for loss (consecutive losses are correlated)
    pub loss_correlation: f64,
}

impl Default for NetworkEmulation {
    fn default() -> Self {
        Self {
            delay_ms: 0,
            jitter_ms: 0,
            loss_percent: 0.0,
            loss_correlation: 25.0, // 25% correlation is typical
        }
    }
}

impl NetworkEmulation {
    /// Create settings for LAN-like conditions (minimal latency)
    pub fn lan() -> Self {
        Self {
            delay_ms: 1,
            jitter_ms: 1,
            loss_percent: 0.0,
            ..Default::default()
        }
    }

    /// Create settings for regional network (US coast-to-coast)
    pub fn regional() -> Self {
        Self {
            delay_ms: 40,
            jitter_ms: 10,
            loss_percent: 0.1,
            ..Default::default()
        }
    }

    /// Create settings for intercontinental network (US to Europe)
    pub fn intercontinental() -> Self {
        Self {
            delay_ms: 125,
            jitter_ms: 25,
            loss_percent: 0.5,
            ..Default::default()
        }
    }

    /// Create settings for high-latency network (US to Asia-Pacific)
    /// This is the scenario that triggered the BBR timeout storm bug.
    pub fn high_latency() -> Self {
        Self {
            delay_ms: 200,
            jitter_ms: 30,
            loss_percent: 1.0,
            ..Default::default()
        }
    }

    /// Create settings for challenging network conditions
    pub fn challenging() -> Self {
        Self {
            delay_ms: 150,
            jitter_ms: 50,
            loss_percent: 3.0,
            loss_correlation: 50.0,
        }
    }
}

impl Default for DockerNatConfig {
    fn default() -> Self {
        // Generate timestamp-based prefix for easier identification of stale resources
        let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
        let random_id = rand::thread_rng().gen::<u16>();
        let name_prefix = format!("freenet-nat-{}-{}", timestamp, random_id);

        // Randomize the second octet (16-31) to avoid subnet overlap when running
        // multiple tests sequentially. Docker cannot create networks with overlapping
        // subnets, so each test run needs a unique subnet range.
        // Using 172.16.0.0/12 private range: 172.16-31.x.x
        // Use /16 subnet to allow peers in different /24s (different ring locations)
        let second_octet = rand::thread_rng().gen_range(16..=31);
        let public_subnet = format!("172.{}.0.0/16", second_octet).parse().unwrap();

        // Also randomize the private subnet base to avoid conflicts
        // Using 10.x.0.0 range with random first octet portion
        let private_first_octet = rand::thread_rng().gen_range(1..=250);

        // Check for network emulation environment variable
        let network_emulation =
            if let Ok(emulation) = std::env::var("FREENET_TEST_NETWORK_EMULATION") {
                match emulation.to_lowercase().as_str() {
                    "lan" => Some(NetworkEmulation::lan()),
                    "regional" => Some(NetworkEmulation::regional()),
                    "intercontinental" => Some(NetworkEmulation::intercontinental()),
                    "high_latency" => Some(NetworkEmulation::high_latency()),
                    "challenging" => Some(NetworkEmulation::challenging()),
                    other => {
                        tracing::warn!(
                            "Unknown FREENET_TEST_NETWORK_EMULATION value '{}', ignoring. \
                             Valid options: lan, regional, intercontinental, high_latency, challenging",
                            other
                        );
                        None
                    }
                }
            } else {
                None
            };

        Self {
            topology: NatTopology::OnePerNat,
            public_subnet,
            private_subnet_base: Ipv4Addr::new(10, private_first_octet, 0, 0),
            cleanup_on_drop: true,
            name_prefix,
            network_emulation,
        }
    }
}

/// How peers are distributed across NAT networks
#[derive(Debug, Clone)]
pub enum NatTopology {
    /// Each peer (except gateways) gets its own NAT network
    OnePerNat,
    /// Specific assignment of peers to NAT networks
    Custom(Vec<NatNetwork>),
}

/// A NAT network containing one or more peers
#[derive(Debug, Clone)]
pub struct NatNetwork {
    pub name: String,
    pub peer_indices: Vec<usize>,
    pub nat_type: NatType,
}

/// Type of NAT simulation
#[derive(Debug, Clone, Default)]
pub enum NatType {
    /// Outbound MASQUERADE only - most common residential NAT
    #[default]
    RestrictedCone,
    /// MASQUERADE + port forwarding for specified ports
    FullCone { forwarded_ports: Option<Vec<u16>> },
}

/// Manages Docker resources for NAT simulation
pub struct DockerNatBackend {
    docker: Docker,
    config: DockerNatConfig,
    /// Network IDs created by this backend
    networks: Vec<String>,
    /// Container IDs created by this backend (NAT routers + peers)
    containers: Vec<String>,
    /// Mapping from peer index to container info
    peer_containers: HashMap<usize, DockerPeerInfo>,
    /// ID of the public network
    public_network_id: Option<String>,
}

/// Information about a peer running in a Docker container
#[derive(Debug, Clone)]
pub struct DockerPeerInfo {
    pub container_id: String,
    pub container_name: String,
    /// IP address on private network (behind NAT)
    pub private_ip: Ipv4Addr,
    /// IP address on public network (for gateways) or NAT router's public IP (for peers)
    pub public_ip: Ipv4Addr,
    /// Port mapped to host for WebSocket API access
    pub host_ws_port: u16,
    /// Network port inside container
    pub network_port: u16,
    /// Whether this is a gateway (not behind NAT)
    pub is_gateway: bool,
    /// NAT router container ID (None for gateways)
    pub nat_router_id: Option<String>,
}

/// A peer process running in a Docker container
pub struct DockerProcess {
    docker: Docker,
    container_id: String,
    container_name: String,
    local_log_cache: PathBuf,
}

impl PeerProcess for DockerProcess {
    fn is_running(&self) -> bool {
        // Use blocking runtime to check container status
        let docker = self.docker.clone();
        let id = self.container_id.clone();

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                match docker.inspect_container(&id, None).await {
                    Ok(info) => info
                        .state
                        .and_then(|s| s.status)
                        .map(|s| s == ContainerStateStatusEnum::RUNNING)
                        .unwrap_or(false),
                    Err(_) => false,
                }
            })
        })
    }

    fn kill(&mut self) -> Result<()> {
        let docker = self.docker.clone();
        let id = self.container_id.clone();

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                // Stop container with timeout
                let _ = docker
                    .stop_container(&id, Some(StopContainerOptions { t: 5 }))
                    .await;
                Ok(())
            })
        })
    }

    fn log_path(&self) -> PathBuf {
        self.local_log_cache.clone()
    }

    fn read_logs(&self) -> Result<Vec<LogEntry>> {
        let docker = self.docker.clone();
        let id = self.container_id.clone();
        let cache_path = self.local_log_cache.clone();

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                // Fetch logs from container
                let options = LogsOptions::<String> {
                    stdout: true,
                    stderr: true,
                    timestamps: true,
                    ..Default::default()
                };

                let mut logs = docker.logs(&id, Some(options));
                let mut log_content = String::new();

                while let Some(log_result) = logs.next().await {
                    match log_result {
                        Ok(LogOutput::StdOut { message }) | Ok(LogOutput::StdErr { message }) => {
                            log_content.push_str(&String::from_utf8_lossy(&message));
                        }
                        _ => {}
                    }
                }

                // Write to cache file
                if let Some(parent) = cache_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&cache_path, &log_content)?;

                // Parse logs
                crate::logs::read_log_file(&cache_path)
            })
        })
    }
}

impl Drop for DockerProcess {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

impl DockerNatBackend {
    /// Create a new Docker NAT backend
    pub async fn new(config: DockerNatConfig) -> Result<Self> {
        let docker = Docker::connect_with_local_defaults()
            .map_err(|e| Error::Other(anyhow::anyhow!("Failed to connect to Docker: {}", e)))?;

        // Verify Docker is accessible
        docker
            .ping()
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("Docker ping failed: {}", e)))?;

        // Clean up stale resources before creating new ones.
        // Use a short max_age (10 seconds) to remove resources from previous test runs
        // while preserving any resources created in the current session. This prevents
        // "Pool overlaps with other one on this address space" errors when tests
        // run sequentially in the same process.
        Self::cleanup_stale_resources(&docker, Duration::from_secs(10)).await?;

        Ok(Self {
            docker,
            config,
            networks: Vec::new(),
            containers: Vec::new(),
            peer_containers: HashMap::new(),
            public_network_id: None,
        })
    }

    /// Clean up stale Docker resources older than the specified duration
    ///
    /// This removes containers and networks matching the "freenet-nat-" prefix
    /// that are older than `max_age`. Pass `Duration::ZERO` to clean up ALL
    /// matching resources regardless of age.
    async fn cleanup_stale_resources(docker: &Docker, max_age: Duration) -> Result<()> {
        use bollard::container::ListContainersOptions;
        use bollard::network::ListNetworksOptions;

        let now = std::time::SystemTime::now();
        let now_secs = now.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        // If max_age is zero, set cutoff to future to match everything
        let cutoff = if max_age.is_zero() {
            i64::MAX // Match everything
        } else {
            now_secs - max_age.as_secs() as i64
        };

        if max_age.is_zero() {
            tracing::debug!("Cleaning up ALL freenet-nat resources");
        } else {
            tracing::debug!(
                "Cleaning up freenet-nat resources older than {} seconds",
                max_age.as_secs()
            );
        }

        // Clean up stale containers
        let mut filters = HashMap::new();
        filters.insert("name".to_string(), vec!["freenet-nat-".to_string()]);

        let options = ListContainersOptions {
            all: true,
            filters,
            ..Default::default()
        };

        match docker.list_containers(Some(options)).await {
            Ok(containers) => {
                let mut removed_count = 0;
                for container in containers {
                    // Parse timestamp from container name
                    if let Some(name) = container.names.and_then(|n| n.first().cloned()) {
                        if let Some(created) = container.created {
                            if created < cutoff {
                                if let Some(id) = container.id {
                                    tracing::info!(
                                        "Removing stale container: {} (age: {}s)",
                                        name,
                                        now.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
                                            as i64
                                            - created
                                    );
                                    let _ = docker
                                        .stop_container(&id, Some(StopContainerOptions { t: 2 }))
                                        .await;
                                    let _ = docker
                                        .remove_container(
                                            &id,
                                            Some(RemoveContainerOptions {
                                                force: true,
                                                ..Default::default()
                                            }),
                                        )
                                        .await;
                                    removed_count += 1;
                                }
                            }
                        }
                    }
                }
                if removed_count > 0 {
                    tracing::info!("Removed {} stale container(s)", removed_count);
                }
            }
            Err(e) => {
                tracing::warn!("Failed to list containers for cleanup: {}", e);
            }
        }

        // Clean up stale networks
        let mut filters = HashMap::new();
        filters.insert("name".to_string(), vec!["freenet-nat-".to_string()]);

        let options = ListNetworksOptions { filters };

        match docker.list_networks(Some(options)).await {
            Ok(networks) => {
                let mut removed_count = 0;
                for network in networks {
                    if let Some(name) = &network.name {
                        if name.starts_with("freenet-nat-") {
                            // Parse timestamp from network name (format: freenet-nat-YYYYMMDD-HHMMSS-xxxxx)
                            if let Some(timestamp_str) = name.strip_prefix("freenet-nat-") {
                                // Extract YYYYMMDD-HHMMSS part
                                let parts: Vec<&str> = timestamp_str.split('-').collect();
                                if parts.len() >= 2 {
                                    let date_time = format!("{}-{}", parts[0], parts[1]);
                                    if let Ok(created_time) = chrono::NaiveDateTime::parse_from_str(
                                        &date_time,
                                        "%Y%m%d-%H%M%S",
                                    ) {
                                        let created_timestamp = created_time.and_utc().timestamp();
                                        if created_timestamp < cutoff {
                                            if let Some(id) = &network.id {
                                                tracing::info!(
                                                    "Removing stale network: {} (age: {}s)",
                                                    name,
                                                    now.duration_since(std::time::UNIX_EPOCH)
                                                        .unwrap()
                                                        .as_secs()
                                                        as i64
                                                        - created_timestamp
                                                );
                                                let _ = docker.remove_network(id).await;
                                                removed_count += 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if removed_count > 0 {
                    tracing::info!("Removed {} stale network(s)", removed_count);
                }
            }
            Err(e) => {
                tracing::warn!("Failed to list networks for cleanup: {}", e);
            }
        }

        Ok(())
    }

    /// Create the public network where gateways live
    ///
    /// If the initially chosen subnet conflicts with an existing Docker network,
    /// this will retry with a different random subnet up to MAX_SUBNET_RETRIES times.
    pub async fn create_public_network(&mut self) -> Result<String> {
        const MAX_SUBNET_RETRIES: usize = 10;

        for attempt in 0..MAX_SUBNET_RETRIES {
            let network_name = format!("{}-public", self.config.name_prefix);

            let options = CreateNetworkOptions {
                name: network_name.clone(),
                driver: "bridge".to_string(),
                ipam: Ipam {
                    config: Some(vec![IpamConfig {
                        subnet: Some(self.config.public_subnet.to_string()),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
                ..Default::default()
            };

            match self.docker.create_network(options).await {
                Ok(response) => {
                    let network_id = response.id;
                    self.networks.push(network_id.clone());
                    self.public_network_id = Some(network_id.clone());
                    tracing::info!(
                        "Created public network: {} ({}) with subnet {}",
                        network_name,
                        network_id,
                        self.config.public_subnet
                    );
                    return Ok(network_id);
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    if error_msg.contains("Pool overlaps") {
                        // Subnet conflict - pick a new random subnet and retry
                        let old_subnet = self.config.public_subnet;
                        let new_second_octet = rand::thread_rng().gen_range(16..=31);
                        self.config.public_subnet =
                            format!("172.{}.0.0/16", new_second_octet).parse().unwrap();
                        tracing::warn!(
                            "Subnet {} conflicts with existing network, retrying with {} (attempt {}/{})",
                            old_subnet,
                            self.config.public_subnet,
                            attempt + 1,
                            MAX_SUBNET_RETRIES
                        );
                        continue;
                    }
                    return Err(Error::Other(anyhow::anyhow!(
                        "Failed to create public network: {}",
                        e
                    )));
                }
            }
        }

        Err(Error::Other(anyhow::anyhow!(
            "Failed to create public network after {} attempts due to subnet conflicts. \
             This may indicate stale Docker networks. Try running: \
             docker network ls | grep freenet-nat | awk '{{print $1}}' | xargs -r docker network rm",
            MAX_SUBNET_RETRIES
        )))
    }

    /// Create a private network behind NAT for a peer
    pub async fn create_nat_network(
        &mut self,
        peer_index: usize,
    ) -> Result<(String, String, Ipv4Addr)> {
        // Create private network using randomized base to avoid subnet conflicts
        // between concurrent test runs. Each peer gets its own /24 subnet.
        let network_name = format!("{}-nat-{}", self.config.name_prefix, peer_index);
        let base = self.config.private_subnet_base.octets();
        let subnet = Ipv4Network::new(
            Ipv4Addr::new(base[0], base[1].wrapping_add(peer_index as u8), 0, 0),
            24,
        )
        .map_err(|e| Error::Other(anyhow::anyhow!("Invalid subnet: {}", e)))?;

        let options = CreateNetworkOptions {
            name: network_name.clone(),
            driver: "bridge".to_string(),
            internal: true, // No direct external access
            ipam: Ipam {
                config: Some(vec![IpamConfig {
                    subnet: Some(subnet.to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            ..Default::default()
        };

        let response =
            self.docker.create_network(options).await.map_err(|e| {
                Error::Other(anyhow::anyhow!("Failed to create NAT network: {}", e))
            })?;

        let network_id = response.id;
        self.networks.push(network_id.clone());

        // Create NAT router container
        let router_name = format!("{}-router-{}", self.config.name_prefix, peer_index);
        let public_network_id = self
            .public_network_id
            .as_ref()
            .ok_or_else(|| Error::Other(anyhow::anyhow!("Public network not created yet")))?;

        // NAT router IP addresses
        // Each peer gets an IP in a different /24 subnet to ensure different ring locations
        // E.g., peer 0 -> 172.X.0.100, peer 1 -> 172.X.1.100, peer 2 -> 172.X.2.100
        // This way, Location::from_address (which masks last byte) gives each peer a different location
        let router_public_ip = Ipv4Addr::new(
            self.config.public_subnet.ip().octets()[0],
            self.config.public_subnet.ip().octets()[1],
            peer_index as u8, // Different /24 per peer for unique ring locations
            100,              // Fixed host part within each /24
        );
        // Use .254 for router to avoid conflict with Docker's default gateway at .1
        let router_private_ip =
            Ipv4Addr::new(base[0], base[1].wrapping_add(peer_index as u8), 0, 254);

        // Create router container with iptables NAT rules
        // Create without network first, then connect to both networks before starting
        // Build patterns for matching the public and private networks
        let public_octets = self.config.public_subnet.ip().octets();
        let public_pattern = format!("172\\.{}\\.", public_octets[1]);
        let private_pattern = format!(" {}\\.", base[0]);
        // Calculate peer's private IP (matches what create_peer will use)
        let peer_private_ip = Ipv4Addr::new(base[0], base[1].wrapping_add(peer_index as u8), 0, 2);

        // Build iptables rules based on NAT type
        //
        // NAT Types (from most permissive to most restrictive):
        // 1. Full Cone: Any external host can send to mapped port (like port forwarding)
        // 2. Address-Restricted Cone: Only hosts the peer has contacted can send back
        // 3. Port-Restricted Cone: Only host:port pairs the peer has contacted can send back
        // 4. Symmetric: Different mapping for each destination (breaks hole punching)
        //
        // Default: Port-Restricted Cone NAT - the most common residential NAT type
        // This requires proper UDP hole-punching: peer must send packet to remote's public
        // IP:port first, which creates a NAT mapping that allows return traffic.
        //
        // The key insight: Linux conntrack already provides port-restricted cone behavior
        // by default with MASQUERADE - it allows return traffic from the exact IP:port
        // that received outbound traffic. We just need to NOT add blanket DNAT rules.
        let dnat_rules = if std::env::var("FREENET_TEST_FULL_CONE_NAT").is_ok() {
            // Full Cone NAT: Add DNAT rules to forward all traffic on port 31337 to peer
            // This simulates port forwarding / UPnP - unrealistic for testing hole punching
            format!(
                "iptables -t nat -A PREROUTING -i $PUBLIC_IF -p udp --dport 31337 -j DNAT --to-destination {}:31337 && \
                 echo 'Full Cone NAT: DNAT rule added for port 31337 -> {}:31337' && ",
                peer_private_ip, peer_private_ip
            )
        } else if std::env::var("FREENET_TEST_SYMMETRIC_NAT").is_ok() {
            // Symmetric NAT: Use random source ports for each destination
            // This breaks UDP hole punching entirely
            format!(
                "iptables -t nat -A POSTROUTING -o $PUBLIC_IF -p udp -j MASQUERADE --random && \
                 echo 'Symmetric NAT: Random port mapping enabled (hole punching will fail)' && "
            )
        } else {
            // Port-Restricted Cone NAT (default): Realistic residential NAT
            //
            // Residential NATs typically have two properties (RFC 4787):
            // 1. Endpoint-Independent Mapping (EIM): Same internal IP:port maps to same
            //    external port regardless of destination
            // 2. Port-Restricted Cone Filtering: Only allow inbound from IP:port pairs
            //    that we've previously sent to (handled by conntrack)
            //
            // Implementation:
            // - DNAT: Forward incoming UDP/31337 to internal peer (enables hole punching)
            // - SNAT: Preserve port 31337 on outbound (EIM behavior)
            // - Conntrack handles port-restricted filtering automatically
            //
            // Note: Without DNAT, incoming packets to the NAT's public IP are delivered
            // to the router itself (INPUT chain) rather than forwarded to the internal peer.
            format!(
                "echo 'Port-Restricted Cone NAT: EIM + port-restricted filtering' && \
                 iptables -t nat -A PREROUTING -i $PUBLIC_IF -p udp --dport 31337 -j DNAT --to-destination {}:31337 && \
                 iptables -t nat -A POSTROUTING -o $PUBLIC_IF -p udp --sport 31337 -j SNAT --to-source $PUBLIC_IP:31337 && ",
                peer_private_ip
            )
        };

        let router_config = Config {
            image: Some("alpine:latest".to_string()),
            hostname: Some(router_name.clone()),
            cmd: Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                // Set up NAT (IP forwarding enabled via sysctl in host_config)
                // Find interfaces dynamically by IP address since Docker doesn't guarantee interface order
                // PUBLIC_IF: interface with 172.X.x.x (public network, X varies)
                // PRIVATE_IF: interface with 10.x.x.x (private network)
                format!(
                    "apk add --no-cache iptables iproute2 > /dev/null 2>&1 && \
                     PUBLIC_IF=$(ip -o addr show | grep '{}' | awk '{{print $2}}') && \
                     PRIVATE_IF=$(ip -o addr show | grep '{}' | awk '{{print $2}}') && \
                     PUBLIC_IP=$(ip -o addr show dev $PUBLIC_IF | awk '/inet / {{split($4,a,\"/\"); print a[1]}}') && \
                     echo \"Public interface: $PUBLIC_IF ($PUBLIC_IP), Private interface: $PRIVATE_IF\" && \
                     {}iptables -t nat -A POSTROUTING -o $PUBLIC_IF -j MASQUERADE && \
                     iptables -A FORWARD -i $PRIVATE_IF -o $PUBLIC_IF -j ACCEPT && \
                     iptables -A FORWARD -i $PUBLIC_IF -o $PRIVATE_IF -j ACCEPT && \
                     echo 'NAT router ready' && \
                     tail -f /dev/null",
                    public_pattern, private_pattern, dnat_rules
                ),
            ]),
            host_config: Some(HostConfig {
                cap_add: Some(vec!["NET_ADMIN".to_string()]),
                sysctls: Some(HashMap::from([
                    ("net.ipv4.ip_forward".to_string(), "1".to_string()),
                ])),
                ..Default::default()
            }),
            ..Default::default()
        };

        let router_id = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: router_name.clone(),
                    ..Default::default()
                }),
                router_config,
            )
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("Failed to create NAT router: {}", e)))?
            .id;

        self.containers.push(router_id.clone());

        // Disconnect from default bridge network
        let _ = self
            .docker
            .disconnect_network(
                "bridge",
                bollard::network::DisconnectNetworkOptions {
                    container: router_id.clone(),
                    force: true,
                },
            )
            .await;

        // Connect router to public network (becomes eth0 after starting)
        self.docker
            .connect_network(
                public_network_id,
                bollard::network::ConnectNetworkOptions {
                    container: router_id.clone(),
                    endpoint_config: bollard::secret::EndpointSettings {
                        ipam_config: Some(bollard::secret::EndpointIpamConfig {
                            ipv4_address: Some(router_public_ip.to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                },
            )
            .await
            .map_err(|e| {
                Error::Other(anyhow::anyhow!(
                    "Failed to connect router to public network: {}",
                    e
                ))
            })?;

        // Connect router to private network (becomes eth1 after starting)
        self.docker
            .connect_network(
                &network_id,
                bollard::network::ConnectNetworkOptions {
                    container: router_id.clone(),
                    endpoint_config: bollard::secret::EndpointSettings {
                        ipam_config: Some(bollard::secret::EndpointIpamConfig {
                            ipv4_address: Some(router_private_ip.to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                },
            )
            .await
            .map_err(|e| {
                Error::Other(anyhow::anyhow!(
                    "Failed to connect router to private network: {}",
                    e
                ))
            })?;

        // Start the router
        self.docker
            .start_container(&router_id, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("Failed to start NAT router: {}", e)))?;

        // Wait for router to be ready
        tokio::time::sleep(Duration::from_secs(2)).await;

        tracing::info!(
            "Created NAT network {} with router {} (public: {}, private: {})",
            network_name,
            router_name,
            router_public_ip,
            router_private_ip
        );

        Ok((network_id, router_id, router_public_ip))
    }

    /// Build the base Freenet peer Docker image
    pub async fn ensure_base_image(&self) -> Result<String> {
        let image_name = "freenet-test-peer:latest";

        // Check if image already exists
        if self.docker.inspect_image(image_name).await.is_ok() {
            tracing::debug!("Base image {} already exists", image_name);
            return Ok(image_name.to_string());
        }

        tracing::info!("Building base image {}...", image_name);

        // Create a minimal Dockerfile - use Ubuntu 24.04 to match host glibc version
        let dockerfile = r#"
FROM ubuntu:24.04
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        libssl3 \
        ca-certificates \
        iproute2 \
        && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /data /config
WORKDIR /app
"#;

        // Create tar archive with Dockerfile
        let mut tar_builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_path("Dockerfile")?;
        header.set_size(dockerfile.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar_builder.append(&header, dockerfile.as_bytes())?;
        let tar_data = tar_builder.into_inner()?;

        // Build image
        let options = BuildImageOptions {
            dockerfile: "Dockerfile",
            t: image_name,
            rm: true,
            ..Default::default()
        };

        let mut build_stream = self
            .docker
            .build_image(options, None, Some(tar_data.into()));

        while let Some(result) = build_stream.next().await {
            match result {
                Ok(info) => {
                    if let Some(stream) = info.stream {
                        tracing::debug!("Build: {}", stream.trim());
                    }
                    if let Some(error) = info.error {
                        return Err(Error::Other(anyhow::anyhow!(
                            "Image build error: {}",
                            error
                        )));
                    }
                }
                Err(e) => {
                    return Err(Error::Other(anyhow::anyhow!("Image build failed: {}", e)));
                }
            }
        }

        tracing::info!("Built base image {}", image_name);
        Ok(image_name.to_string())
    }

    /// Copy binary into a container
    pub async fn copy_binary_to_container(
        &self,
        container_id: &str,
        binary_path: &Path,
    ) -> Result<()> {
        // Read binary
        let binary_data = std::fs::read(binary_path)?;

        // Create tar archive with the binary
        let mut tar_builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_path("freenet")?;
        header.set_size(binary_data.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar_builder.append(&header, binary_data.as_slice())?;
        let tar_data = tar_builder.into_inner()?;

        // Upload to container
        self.docker
            .upload_to_container(
                container_id,
                Some(UploadToContainerOptions {
                    path: "/app",
                    ..Default::default()
                }),
                tar_data.into(),
            )
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("Failed to copy binary: {}", e)))?;

        Ok(())
    }

    /// Create a gateway container (on public network, no NAT)
    pub async fn create_gateway(
        &mut self,
        index: usize,
        binary_path: &Path,
        keypair_path: &Path,
        public_key_path: &Path,
        ws_port: u16,
        network_port: u16,
        run_root: &Path,
    ) -> Result<(DockerPeerInfo, DockerProcess)> {
        let container_name = format!("{}-gw-{}", self.config.name_prefix, index);
        let image = self.ensure_base_image().await?;

        let public_network_id = self
            .public_network_id
            .as_ref()
            .ok_or_else(|| Error::Other(anyhow::anyhow!("Public network not created yet")))?;

        // Gateway IP on public network
        let gateway_ip = Ipv4Addr::new(
            self.config.public_subnet.ip().octets()[0],
            self.config.public_subnet.ip().octets()[1],
            0,
            10 + index as u8,
        );

        // Create container - let Docker auto-allocate host port to avoid TOCTOU race
        let config = Config {
            image: Some(image),
            hostname: Some(container_name.clone()),
            exposed_ports: Some(HashMap::from([(
                format!("{}/tcp", ws_port),
                HashMap::new(),
            )])),
            host_config: Some(HostConfig {
                port_bindings: Some(HashMap::from([(
                    format!("{}/tcp", ws_port),
                    Some(vec![PortBinding {
                        host_ip: Some("0.0.0.0".to_string()),
                        host_port: None, // Let Docker auto-allocate to avoid port conflicts
                    }]),
                )])),
                cap_add: Some(vec!["NET_ADMIN".to_string()]),
                ..Default::default()
            }),
            env: Some(vec![
                format!("RUST_LOG={}", std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string())),
                "RUST_BACKTRACE=1".to_string(),
            ]),
            cmd: Some(vec![
                "/app/freenet".to_string(),
                "network".to_string(),
                "--data-dir".to_string(),
                "/data".to_string(),
                "--config-dir".to_string(),
                "/config".to_string(),
                "--ws-api-address".to_string(),
                "0.0.0.0".to_string(),
                "--ws-api-port".to_string(),
                ws_port.to_string(),
                "--network-address".to_string(),
                "0.0.0.0".to_string(),
                "--network-port".to_string(),
                network_port.to_string(),
                "--public-network-address".to_string(),
                gateway_ip.to_string(),
                "--public-network-port".to_string(),
                network_port.to_string(),
                "--is-gateway".to_string(),
                "--skip-load-from-network".to_string(),
                "--transport-keypair".to_string(),
                "/config/keypair.pem".to_string(),
            ]),
            ..Default::default()
        };

        let container_id = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: container_name.clone(),
                    ..Default::default()
                }),
                config,
            )
            .await
            .map_err(|e| {
                Error::Other(anyhow::anyhow!("Failed to create gateway container: {}", e))
            })?
            .id;

        self.containers.push(container_id.clone());

        // Connect to public network with specific IP
        self.docker
            .connect_network(
                public_network_id,
                bollard::network::ConnectNetworkOptions {
                    container: container_id.clone(),
                    endpoint_config: bollard::secret::EndpointSettings {
                        ipam_config: Some(bollard::secret::EndpointIpamConfig {
                            ipv4_address: Some(gateway_ip.to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                },
            )
            .await
            .map_err(|e| {
                Error::Other(anyhow::anyhow!(
                    "Failed to connect gateway to network: {}",
                    e
                ))
            })?;

        // Copy binary and keys into container
        self.copy_binary_to_container(&container_id, binary_path)
            .await?;
        self.copy_file_to_container(&container_id, keypair_path, "/config/keypair.pem")
            .await?;
        self.copy_file_to_container(&container_id, public_key_path, "/config/public_key.pem")
            .await?;

        // Start container
        self.docker
            .start_container(&container_id, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("Failed to start gateway: {}", e)))?;

        // Apply network emulation if configured
        self.apply_network_emulation(&container_id, &container_name)
            .await?;

        // Get the Docker-allocated host port by inspecting the running container
        let host_ws_port = self
            .get_container_host_port(&container_id, ws_port)
            .await?;

        let info = DockerPeerInfo {
            container_id: container_id.clone(),
            container_name: container_name.clone(),
            private_ip: gateway_ip, // Gateways don't have private IP
            public_ip: gateway_ip,
            host_ws_port,
            network_port,
            is_gateway: true,
            nat_router_id: None,
        };

        self.peer_containers.insert(index, info.clone());

        let local_log_cache = run_root.join(format!("gw{}", index)).join("peer.log");

        tracing::info!(
            "Created gateway {} at {} (ws: localhost:{})",
            container_name,
            gateway_ip,
            host_ws_port
        );

        Ok((
            info,
            DockerProcess {
                docker: self.docker.clone(),
                container_id,
                container_name,
                local_log_cache,
            },
        ))
    }

    /// Create a peer container behind NAT
    pub async fn create_peer(
        &mut self,
        index: usize,
        binary_path: &Path,
        keypair_path: &Path,
        public_key_path: &Path,
        gateways_toml_path: &Path,
        gateway_public_key_path: Option<&Path>,
        ws_port: u16,
        network_port: u16,
        run_root: &Path,
    ) -> Result<(DockerPeerInfo, DockerProcess)> {
        let container_name = format!("{}-peer-{}", self.config.name_prefix, index);
        let image = self.ensure_base_image().await?;

        // Create NAT network for this peer
        let (nat_network_id, router_id, router_public_ip) = self.create_nat_network(index).await?;

        // Peer's private IP (behind NAT) - use the randomized base from config
        let base = self.config.private_subnet_base.octets();
        let private_ip = Ipv4Addr::new(base[0], base[1].wrapping_add(index as u8), 0, 2);

        // Create container - let Docker auto-allocate host port to avoid TOCTOU race
        let config = Config {
            image: Some(image),
            hostname: Some(container_name.clone()),
            exposed_ports: Some(HashMap::from([(
                format!("{}/tcp", ws_port),
                HashMap::new(),
            )])),
            host_config: Some(HostConfig {
                port_bindings: Some(HashMap::from([(
                    format!("{}/tcp", ws_port),
                    Some(vec![PortBinding {
                        host_ip: Some("0.0.0.0".to_string()),
                        host_port: None, // Let Docker auto-allocate to avoid port conflicts
                    }]),
                )])),
                cap_add: Some(vec!["NET_ADMIN".to_string()]),
                ..Default::default()
            }),
            env: Some(vec![
                format!("RUST_LOG={}", std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string())),
                "RUST_BACKTRACE=1".to_string(),
            ]),
            cmd: Some(vec![
                "/app/freenet".to_string(),
                "network".to_string(),
                "--data-dir".to_string(),
                "/data".to_string(),
                "--config-dir".to_string(),
                "/config".to_string(),
                "--ws-api-address".to_string(),
                "0.0.0.0".to_string(),
                "--ws-api-port".to_string(),
                ws_port.to_string(),
                "--network-address".to_string(),
                "0.0.0.0".to_string(),
                "--network-port".to_string(),
                network_port.to_string(),
                // Don't set public address - let Freenet discover it via gateway
                "--skip-load-from-network".to_string(),
                "--transport-keypair".to_string(),
                "/config/keypair.pem".to_string(),
            ]),
            ..Default::default()
        };

        let container_id = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: container_name.clone(),
                    ..Default::default()
                }),
                config,
            )
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("Failed to create peer container: {}", e)))?
            .id;

        self.containers.push(container_id.clone());

        // Keep bridge network connected for Docker port forwarding to work (WebSocket access from host)
        // Connect to NAT private network for Freenet traffic
        self.docker
            .connect_network(
                &nat_network_id,
                bollard::network::ConnectNetworkOptions {
                    container: container_id.clone(),
                    endpoint_config: bollard::secret::EndpointSettings {
                        ipam_config: Some(bollard::secret::EndpointIpamConfig {
                            ipv4_address: Some(private_ip.to_string()),
                            ..Default::default()
                        }),
                        gateway: Some(
                            Ipv4Addr::new(base[0], base[1].wrapping_add(index as u8), 0, 1)
                                .to_string(),
                        ),
                        ..Default::default()
                    },
                },
            )
            .await
            .map_err(|e| {
                Error::Other(anyhow::anyhow!(
                    "Failed to connect peer to NAT network: {}",
                    e
                ))
            })?;

        // Copy binary and keys into container
        self.copy_binary_to_container(&container_id, binary_path)
            .await?;
        self.copy_file_to_container(&container_id, keypair_path, "/config/keypair.pem")
            .await?;
        self.copy_file_to_container(&container_id, public_key_path, "/config/public_key.pem")
            .await?;
        self.copy_file_to_container(&container_id, gateways_toml_path, "/config/gateways.toml")
            .await?;

        // Copy gateway public key if provided
        if let Some(gw_pubkey_path) = gateway_public_key_path {
            self.copy_file_to_container(&container_id, gw_pubkey_path, "/config/gw_public_key.pem")
                .await?;
        }

        // Start container
        self.docker
            .start_container(&container_id, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("Failed to start peer: {}", e)))?;

        // Apply network emulation if configured
        self.apply_network_emulation(&container_id, &container_name)
            .await?;

        // Get the Docker-allocated host port by inspecting the running container
        let host_ws_port = self
            .get_container_host_port(&container_id, ws_port)
            .await?;

        // Configure routing: traffic to public network goes through NAT router
        // Keep default route via bridge for Docker port forwarding (WebSocket access from host)
        let router_gateway = Ipv4Addr::new(base[0], base[1].wrapping_add(index as u8), 0, 254);
        let public_subnet = self.config.public_subnet;
        self.exec_in_container(
            &container_id,
            &[
                "sh",
                "-c",
                &format!("ip route add {} via {}", public_subnet, router_gateway),
            ],
        )
        .await?;

        let info = DockerPeerInfo {
            container_id: container_id.clone(),
            container_name: container_name.clone(),
            private_ip,
            public_ip: router_public_ip,
            host_ws_port,
            network_port,
            is_gateway: false,
            nat_router_id: Some(router_id),
        };

        self.peer_containers.insert(index, info.clone());

        let local_log_cache = run_root.join(format!("peer{}", index)).join("peer.log");

        tracing::info!(
            "Created peer {} at {} behind NAT {} (ws: localhost:{})",
            container_name,
            private_ip,
            router_public_ip,
            host_ws_port
        );

        Ok((
            info,
            DockerProcess {
                docker: self.docker.clone(),
                container_id,
                container_name,
                local_log_cache,
            },
        ))
    }

    /// Copy a file into a container (public version)
    pub async fn copy_file_to_container_pub(
        &self,
        container_id: &str,
        local_path: &Path,
        container_path: &str,
    ) -> Result<()> {
        self.copy_file_to_container(container_id, local_path, container_path)
            .await
    }

    /// Copy a file into a container
    async fn copy_file_to_container(
        &self,
        container_id: &str,
        local_path: &Path,
        container_path: &str,
    ) -> Result<()> {
        let file_data = std::fs::read(local_path)?;
        let file_name = Path::new(container_path)
            .file_name()
            .ok_or_else(|| Error::Other(anyhow::anyhow!("Invalid container path")))?
            .to_str()
            .ok_or_else(|| Error::Other(anyhow::anyhow!("Invalid file name")))?;

        let dir_path = Path::new(container_path)
            .parent()
            .ok_or_else(|| Error::Other(anyhow::anyhow!("Invalid container path")))?
            .to_str()
            .ok_or_else(|| Error::Other(anyhow::anyhow!("Invalid directory path")))?;

        // Create tar archive
        let mut tar_builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_path(file_name)?;
        header.set_size(file_data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar_builder.append(&header, file_data.as_slice())?;
        let tar_data = tar_builder.into_inner()?;

        self.docker
            .upload_to_container(
                container_id,
                Some(UploadToContainerOptions {
                    path: dir_path,
                    ..Default::default()
                }),
                tar_data.into(),
            )
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("Failed to copy file: {}", e)))?;

        Ok(())
    }

    /// Execute a command in a container
    async fn exec_in_container(&self, container_id: &str, cmd: &[&str]) -> Result<String> {
        let exec = self
            .docker
            .create_exec(
                container_id,
                CreateExecOptions {
                    cmd: Some(cmd.iter().map(|s| s.to_string()).collect()),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("Failed to create exec: {}", e)))?;

        let output = self
            .docker
            .start_exec(&exec.id, None)
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("Failed to start exec: {}", e)))?;

        let mut result = String::new();
        if let StartExecResults::Attached { mut output, .. } = output {
            while let Some(Ok(msg)) = output.next().await {
                match msg {
                    LogOutput::StdOut { message } | LogOutput::StdErr { message } => {
                        result.push_str(&String::from_utf8_lossy(&message));
                    }
                    _ => {}
                }
            }
        }

        Ok(result)
    }

    /// Apply network emulation (latency, jitter, packet loss) to a container using tc netem.
    ///
    /// This requires NET_ADMIN capability and iproute2 installed in the container.
    /// The emulation is applied to the eth0 interface (primary network interface).
    async fn apply_network_emulation(&self, container_id: &str, container_name: &str) -> Result<()> {
        let Some(ref emulation) = self.config.network_emulation else {
            return Ok(());
        };

        // Skip if no emulation configured
        if emulation.delay_ms == 0 && emulation.loss_percent == 0.0 {
            return Ok(());
        }

        // Build tc netem command
        // tc qdisc add dev eth0 root netem delay 100ms 20ms loss 1% 25%
        let mut tc_args = vec!["tc", "qdisc", "add", "dev", "eth0", "root", "netem"];

        let delay_str;
        let jitter_str;
        let loss_str;
        let correlation_str;

        // Add delay if configured
        if emulation.delay_ms > 0 {
            delay_str = format!("{}ms", emulation.delay_ms);
            tc_args.push("delay");
            tc_args.push(&delay_str);

            if emulation.jitter_ms > 0 {
                jitter_str = format!("{}ms", emulation.jitter_ms);
                tc_args.push(&jitter_str);
            }
        }

        // Add packet loss if configured
        if emulation.loss_percent > 0.0 {
            loss_str = format!("{:.2}%", emulation.loss_percent);
            tc_args.push("loss");
            tc_args.push(&loss_str);

            if emulation.loss_correlation > 0.0 {
                correlation_str = format!("{:.0}%", emulation.loss_correlation);
                tc_args.push(&correlation_str);
            }
        }

        tracing::info!(
            "Applying network emulation to {}: delay={}ms±{}ms, loss={:.2}%",
            container_name,
            emulation.delay_ms,
            emulation.jitter_ms,
            emulation.loss_percent
        );

        let output = self.exec_in_container(container_id, &tc_args).await?;

        if !output.is_empty() && output.contains("Error") {
            tracing::warn!(
                "Network emulation may have failed for {}: {}",
                container_name,
                output.trim()
            );
        } else {
            tracing::debug!(
                "Network emulation applied to {}: {:?}",
                container_name,
                tc_args
            );
        }

        Ok(())
    }

    /// Clean up all Docker resources created by this backend
    pub async fn cleanup(&mut self) -> Result<()> {
        tracing::info!("Cleaning up Docker NAT resources...");

        // Stop and remove containers
        for container_id in self.containers.drain(..) {
            let _ = self
                .docker
                .stop_container(&container_id, Some(StopContainerOptions { t: 2 }))
                .await;
            let _ = self
                .docker
                .remove_container(
                    &container_id,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await;
        }

        // Remove networks
        for network_id in self.networks.drain(..) {
            let _ = self.docker.remove_network(&network_id).await;
        }

        self.peer_containers.clear();
        self.public_network_id = None;

        Ok(())
    }

    /// Get peer info by index
    pub fn get_peer_info(&self, index: usize) -> Option<&DockerPeerInfo> {
        self.peer_containers.get(&index)
    }

    /// Dump iptables NAT rules and packet counters from all NAT routers
    ///
    /// Returns a map of peer_index -> iptables output showing:
    /// - NAT table rules with packet/byte counters
    /// - FORWARD chain counters
    pub async fn dump_iptables_counters(&self) -> Result<std::collections::HashMap<usize, String>> {
        let mut results = std::collections::HashMap::new();

        for (&peer_index, peer_info) in &self.peer_containers {
            if let Some(router_id) = &peer_info.nat_router_id {
                let mut output = String::new();

                // Get NAT table with counters
                output.push_str("=== NAT table ===\n");
                match self.exec_in_container(router_id, &["iptables", "-t", "nat", "-nvL"]).await {
                    Ok(s) => output.push_str(&s),
                    Err(e) => output.push_str(&format!("Error: {}\n", e)),
                }

                // Get FORWARD chain with counters
                output.push_str("\n=== FORWARD chain ===\n");
                match self.exec_in_container(router_id, &["iptables", "-nvL", "FORWARD"]).await {
                    Ok(s) => output.push_str(&s),
                    Err(e) => output.push_str(&format!("Error: {}\n", e)),
                }

                results.insert(peer_index, output);
            }
        }

        Ok(results)
    }

    /// Dump conntrack table from all NAT routers
    ///
    /// Shows active NAT connection tracking entries for UDP traffic.
    /// Note: Installs conntrack-tools if not present (adds ~2s per router first time).
    pub async fn dump_conntrack_table(&self) -> Result<std::collections::HashMap<usize, String>> {
        let mut results = std::collections::HashMap::new();

        for (&peer_index, peer_info) in &self.peer_containers {
            if let Some(router_id) = &peer_info.nat_router_id {
                // Install conntrack-tools if needed
                let _ = self.exec_in_container(
                    router_id,
                    &["apk", "add", "--no-cache", "conntrack-tools"]
                ).await;

                // Get conntrack entries for UDP
                match self.exec_in_container(router_id, &["conntrack", "-L", "-p", "udp"]).await {
                    Ok(s) if s.trim().is_empty() => {
                        results.insert(peer_index, "(no UDP conntrack entries)".to_string());
                    }
                    Ok(s) => {
                        results.insert(peer_index, s);
                    }
                    Err(e) => {
                        results.insert(peer_index, format!("Error: {}", e));
                    }
                }
            }
        }

        Ok(results)
    }

    /// Dump routing tables from all peer containers
    ///
    /// Shows the ip route table for each peer, useful for debugging NAT connectivity.
    pub async fn dump_peer_routes(&self) -> Result<std::collections::HashMap<usize, String>> {
        let mut results = std::collections::HashMap::new();

        for (&peer_index, peer_info) in &self.peer_containers {
            if peer_info.nat_router_id.is_some() {
                // Get route table from the peer container
                match self.exec_in_container(&peer_info.container_id, &["ip", "route"]).await {
                    Ok(s) => { results.insert(peer_index, s); }
                    Err(e) => { results.insert(peer_index, format!("Error: {}", e)); }
                }
            }
        }

        Ok(results)
    }

    /// Get the host port allocated by Docker for a container's exposed port.
    ///
    /// This is used after starting a container to discover which host port Docker
    /// auto-allocated when we specified `host_port: None` in the port binding.
    /// This approach avoids TOCTOU race conditions that can occur when pre-allocating
    /// ports with `get_free_port()` and then trying to bind them in Docker.
    async fn get_container_host_port(&self, container_id: &str, container_port: u16) -> Result<u16> {
        let info = self
            .docker
            .inspect_container(container_id, None)
            .await
            .map_err(|e| {
                Error::Other(anyhow::anyhow!(
                    "Failed to inspect container for port allocation: {}",
                    e
                ))
            })?;

        let port_key = format!("{}/tcp", container_port);

        let host_port = info
            .network_settings
            .and_then(|ns| ns.ports)
            .and_then(|ports| ports.get(&port_key).cloned())
            .flatten()
            .and_then(|bindings| bindings.first().cloned())
            .and_then(|binding| binding.host_port)
            .and_then(|port_str| port_str.parse::<u16>().ok())
            .ok_or_else(|| {
                Error::Other(anyhow::anyhow!(
                    "Failed to get allocated host port for container {} port {}",
                    container_id,
                    container_port
                ))
            })?;

        Ok(host_port)
    }
}

impl Drop for DockerNatBackend {
    fn drop(&mut self) {
        if self.config.cleanup_on_drop {
            tracing::info!("Cleaning up Docker NAT backend resources...");

            // Use blocking approach to ensure cleanup completes before drop finishes
            let docker = self.docker.clone();
            let containers = std::mem::take(&mut self.containers);
            let networks = std::mem::take(&mut self.networks);

            // Block until cleanup completes - important for ensuring resources are freed
            // even on panic or ctrl-c.
            // If we're already in a runtime, use block_in_place; otherwise create a new runtime.
            let cleanup = async {
                // Stop and remove containers in parallel for faster cleanup
                let container_futures = containers.into_iter().map(|container_id| {
                    let docker = docker.clone();
                    async move {
                        if let Err(e) = docker
                            .stop_container(&container_id, Some(StopContainerOptions { t: 2 }))
                            .await
                        {
                            tracing::debug!("Failed to stop container {}: {}", container_id, e);
                        }
                        if let Err(e) = docker
                            .remove_container(
                                &container_id,
                                Some(RemoveContainerOptions {
                                    force: true,
                                    ..Default::default()
                                }),
                            )
                            .await
                        {
                            tracing::debug!("Failed to remove container {}: {}", container_id, e);
                        }
                    }
                });

                // Wait for all containers to be cleaned up
                futures::future::join_all(container_futures).await;

                // Then remove networks (must happen after containers are disconnected)
                for network_id in networks {
                    if let Err(e) = docker.remove_network(&network_id).await {
                        tracing::debug!("Failed to remove network {}: {}", network_id, e);
                    }
                }

                tracing::info!("Docker NAT backend cleanup complete");
            };

            // Try to use existing runtime first (if we're in async context)
            // Otherwise fall back to creating a new runtime
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                tokio::task::block_in_place(|| {
                    handle.block_on(cleanup);
                });
            } else if let Ok(rt) = tokio::runtime::Runtime::new() {
                rt.block_on(cleanup);
            } else {
                tracing::error!("Failed to create runtime for cleanup");
            }
        }
    }
}
