use crate::Result;
use std::path::PathBuf;
use std::process::Child;
use tempfile::TempDir;

/// Represents a single peer in the test network
pub struct TestPeer {
    pub(crate) id: String,
    pub(crate) is_gateway: bool,
    pub(crate) ws_port: u16,
    pub(crate) network_port: u16,
    pub(crate) data_dir: TempDir,
    pub(crate) process: Option<Child>,
    pub(crate) public_key_path: Option<PathBuf>,
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

    /// Get the path to this peer's log file
    pub fn log_path(&self) -> PathBuf {
        self.data_dir.path().join("peer.log")
    }

    /// Check if the peer process is still running
    pub fn is_running(&self) -> bool {
        if let Some(process) = &self.process {
            let pid = process.id();
            let mut system = sysinfo::System::new();
            system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            system.process(sysinfo::Pid::from_u32(pid as u32)).is_some()
        } else {
            false
        }
    }

    /// Kill the peer process
    pub fn kill(&mut self) -> Result<()> {
        if let Some(mut process) = self.process.take() {
            process.kill()?;
            process.wait()?;
        }
        Ok(())
    }
}

impl Drop for TestPeer {
    fn drop(&mut self) {
        if let Some(mut process) = self.process.take() {
            let _ = process.kill();
            let _ = process.wait();
        }
    }
}

/// Get a free port by binding to port 0 and letting the OS assign one
pub(crate) fn get_free_port() -> Result<u16> {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}
