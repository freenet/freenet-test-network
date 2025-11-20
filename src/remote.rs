//! Remote peer support via SSH

use crate::{Error, Result};
use ssh2::Session;
use std::io::Read;
use std::net::TcpStream;
use std::path::PathBuf;

/// Configuration for a remote Linux machine accessible via SSH
#[derive(Debug, Clone)]
pub struct RemoteMachine {
    /// SSH hostname or IP address
    pub host: String,

    /// SSH username (defaults to current user if None)
    pub user: Option<String>,

    /// SSH port (defaults to 22 if None)
    pub port: Option<u16>,

    /// Path to SSH identity file (private key)
    pub identity_file: Option<PathBuf>,

    /// Path to freenet binary on remote machine (if pre-installed)
    /// If None, binary will be deployed via SCP
    pub freenet_binary: Option<PathBuf>,

    /// Working directory for peer data on remote machine
    /// Defaults to /tmp/freenet-test-network if None
    pub work_dir: Option<PathBuf>,
}

impl RemoteMachine {
    /// Create a new remote machine configuration
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            user: None,
            port: None,
            identity_file: None,
            freenet_binary: None,
            work_dir: None,
        }
    }

    /// Set the SSH username
    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// Set the SSH port
    pub fn port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// Set the SSH identity file path
    pub fn identity_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.identity_file = Some(path.into());
        self
    }

    /// Set the remote freenet binary path
    pub fn freenet_binary(mut self, path: impl Into<PathBuf>) -> Self {
        self.freenet_binary = Some(path.into());
        self
    }

    /// Set the remote working directory
    pub fn work_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.work_dir = Some(path.into());
        self
    }

    /// Get the SSH port (22 if not specified)
    pub fn ssh_port(&self) -> u16 {
        self.port.unwrap_or(22)
    }

    /// Get the SSH username (current user if not specified)
    pub fn ssh_user(&self) -> String {
        self.user
            .clone()
            .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "root".to_string()))
    }

    /// Get the remote work directory
    pub fn remote_work_dir(&self) -> PathBuf {
        self.work_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("/tmp/freenet-test-network"))
    }

    /// Establish an SSH connection to this machine
    pub fn connect(&self) -> Result<Session> {
        let addr = format!("{}:{}", self.host, self.ssh_port());
        let tcp = TcpStream::connect(&addr).map_err(|e| {
            Error::PeerStartupFailed(format!("Failed to connect to {}: {}", addr, e))
        })?;

        let mut session = Session::new().map_err(|e| {
            Error::PeerStartupFailed(format!("Failed to create SSH session: {}", e))
        })?;

        session.set_tcp_stream(tcp);
        session
            .handshake()
            .map_err(|e| Error::PeerStartupFailed(format!("SSH handshake failed: {}", e)))?;

        // Authenticate
        let username = self.ssh_user();
        if let Some(identity) = &self.identity_file {
            session
                .userauth_pubkey_file(&username, None, identity, None)
                .map_err(|e| {
                    Error::PeerStartupFailed(format!("SSH key authentication failed: {}", e))
                })?;
        } else {
            // Try agent authentication
            session.userauth_agent(&username).map_err(|e| {
                Error::PeerStartupFailed(format!("SSH agent authentication failed: {}", e))
            })?;
        }

        if !session.authenticated() {
            return Err(Error::PeerStartupFailed(
                "SSH authentication failed".to_string(),
            ));
        }

        Ok(session)
    }

    /// Execute a command on the remote machine and return output
    pub fn exec(&self, command: &str) -> Result<String> {
        let session = self.connect()?;
        let mut channel = session
            .channel_session()
            .map_err(|e| Error::PeerStartupFailed(format!("Failed to open SSH channel: {}", e)))?;

        channel
            .exec(command)
            .map_err(|e| Error::PeerStartupFailed(format!("Failed to execute command: {}", e)))?;

        let mut output = String::new();
        channel.read_to_string(&mut output).map_err(|e| {
            Error::PeerStartupFailed(format!("Failed to read command output: {}", e))
        })?;

        channel.wait_close().ok();
        let exit_status = channel
            .exit_status()
            .map_err(|e| Error::PeerStartupFailed(format!("Failed to get exit status: {}", e)))?;

        if exit_status != 0 {
            return Err(Error::PeerStartupFailed(format!(
                "Command failed with exit code {}: {}",
                exit_status, output
            )));
        }

        Ok(output.trim().to_string())
    }

    /// Copy a local file to the remote machine via SCP
    pub fn scp_upload(&self, local_path: &std::path::Path, remote_path: &str) -> Result<()> {
        let session = self.connect()?;

        let local_file = std::fs::File::open(local_path)
            .map_err(|e| Error::PeerStartupFailed(format!("Failed to open local file: {}", e)))?;

        let metadata = local_file
            .metadata()
            .map_err(|e| Error::PeerStartupFailed(format!("Failed to get file metadata: {}", e)))?;

        let mut remote_file = session
            .scp_send(
                std::path::Path::new(remote_path),
                0o755, // executable permissions
                metadata.len(),
                None,
            )
            .map_err(|e| {
                Error::PeerStartupFailed(format!("Failed to initiate SCP upload: {}", e))
            })?;

        std::io::copy(
            &mut std::fs::File::open(local_path).unwrap(),
            &mut remote_file,
        )
        .map_err(|e| Error::PeerStartupFailed(format!("Failed to upload file: {}", e)))?;

        remote_file.send_eof().ok();
        remote_file.wait_eof().ok();
        remote_file.close().ok();
        remote_file.wait_close().ok();

        Ok(())
    }

    /// Copy a remote file to the local machine via SCP
    pub fn scp_download(&self, remote_path: &str, local_path: &std::path::Path) -> Result<()> {
        let session = self.connect()?;

        let (mut remote_file, _stat) = session
            .scp_recv(std::path::Path::new(remote_path))
            .map_err(|e| {
                Error::PeerStartupFailed(format!("Failed to initiate SCP download: {}", e))
            })?;

        let mut local_file = std::fs::File::create(local_path)
            .map_err(|e| Error::PeerStartupFailed(format!("Failed to create local file: {}", e)))?;

        std::io::copy(&mut remote_file, &mut local_file)
            .map_err(|e| Error::PeerStartupFailed(format!("Failed to download file: {}", e)))?;

        remote_file.send_eof().ok();
        remote_file.wait_eof().ok();
        remote_file.close().ok();
        remote_file.wait_close().ok();

        Ok(())
    }

    /// Discover the public IP address of the remote machine
    pub fn discover_public_address(&self) -> Result<String> {
        // Try multiple methods to discover the public IP

        // Method 1: Use ip route to find the default interface's IP
        if let Ok(addr) = self.exec("ip route get 8.8.8.8 | awk '{print $7; exit}'") {
            if !addr.is_empty() && addr != "127.0.0.1" {
                return Ok(addr);
            }
        }

        // Method 2: Use hostname -I to get all IPs and filter
        if let Ok(output) = self.exec("hostname -I") {
            for addr in output.split_whitespace() {
                if addr.starts_with("192.168.")
                    || addr.starts_with("10.")
                    || addr.starts_with("172.")
                {
                    return Ok(addr.to_string());
                }
            }
        }

        // Fallback: use the SSH host address
        Ok(self.host.clone())
    }
}

/// Specifies where a peer should be spawned
#[derive(Debug, Clone)]
pub enum PeerLocation {
    /// Spawn the peer on the local machine
    Local,

    /// Spawn the peer on a remote machine via SSH
    Remote(RemoteMachine),
}

impl Default for PeerLocation {
    fn default() -> Self {
        PeerLocation::Local
    }
}
