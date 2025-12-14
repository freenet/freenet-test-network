use crate::{process::PeerProcess, remote::PeerLocation, Result};
use std::net::UdpSocket;
use std::path::{Path, PathBuf};

/// Represents a single peer in the test network
pub struct TestPeer {
    pub(crate) id: String,
    pub(crate) is_gateway: bool,
    pub(crate) ws_port: u16,
    pub(crate) network_port: u16,
    pub(crate) network_address: String,
    pub(crate) data_dir: PathBuf,
    pub(crate) process: Box<dyn PeerProcess + Send>,
    pub(crate) public_key_path: Option<PathBuf>,
    pub(crate) location: PeerLocation,
}

impl TestPeer {
    /// Get the WebSocket URL for connecting to this peer
    pub fn ws_url(&self) -> String {
        format!("ws://127.0.0.1:{}/v1/contract/command", self.ws_port)
    }

    /// Get the peer's identifier
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Check if this is a gateway peer
    pub fn is_gateway(&self) -> bool {
        self.is_gateway
    }

    /// Get the peer's network address
    pub fn network_address(&self) -> &str {
        &self.network_address
    }

    /// Get the peer's network port
    pub fn network_port(&self) -> u16 {
        self.network_port
    }

    /// Get the peer's full socket address (address:port)
    pub fn socket_address(&self) -> std::net::SocketAddr {
        format!("{}:{}", self.network_address, self.network_port)
            .parse()
            .expect("valid socket address")
    }

    /// Get the root data directory for this peer
    pub fn data_dir_path(&self) -> &Path {
        &self.data_dir
    }

    /// Get the path to this peer's log file
    pub fn log_path(&self) -> PathBuf {
        self.process.log_path()
    }

    /// Read logs from this peer (fetches from Docker if needed)
    pub fn read_logs(&self) -> Result<Vec<crate::logs::LogEntry>> {
        self.process.read_logs()
    }

    /// Check if the peer process is still running
    pub fn is_running(&self) -> bool {
        self.process.is_running()
    }

    /// Kill the peer process
    pub fn kill(&mut self) -> Result<()> {
        self.process.kill()
    }

    /// Get the peer's location (local or remote)
    pub fn location(&self) -> &PeerLocation {
        &self.location
    }
}

/// Get a free port by binding to port 0 and letting the OS assign one
pub(crate) fn get_free_port() -> Result<u16> {
    // Use UDP to mirror the freenet transport listener and avoid conflicts with UDP-only bindings.
    let socket = UdpSocket::bind("127.0.0.1:0")?;
    Ok(socket.local_addr()?.port())
}
