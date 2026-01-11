//! Process management abstraction for local and remote peers

use crate::{logs::LogEntry, remote::RemoteMachine, Error, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Trait for managing a peer process (local or remote)
pub(crate) trait PeerProcess {
    /// Check if the process is still running
    fn is_running(&self) -> bool;

    /// Kill the process
    fn kill(&mut self) -> Result<()>;

    /// Get the path to the peer's log file
    fn log_path(&self) -> PathBuf;

    /// Read recent log entries from the peer
    fn read_logs(&self) -> Result<Vec<LogEntry>>;
}

/// A peer process running locally
pub(crate) struct LocalProcess {
    pub(crate) process: Option<Child>,
    pub(crate) data_dir: PathBuf,
}

impl PeerProcess for LocalProcess {
    fn is_running(&self) -> bool {
        if let Some(process) = &self.process {
            let pid = process.id();
            let mut system = sysinfo::System::new();
            system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            system.process(sysinfo::Pid::from_u32(pid as u32)).is_some()
        } else {
            false
        }
    }

    fn kill(&mut self) -> Result<()> {
        if let Some(mut process) = self.process.take() {
            process.kill()?;
            process.wait()?;
        }
        Ok(())
    }

    fn log_path(&self) -> PathBuf {
        self.data_dir.join("peer.log")
    }

    fn read_logs(&self) -> Result<Vec<LogEntry>> {
        crate::logs::read_log_file(&self.log_path())
    }
}

impl Drop for LocalProcess {
    fn drop(&mut self) {
        if let Some(mut process) = self.process.take() {
            let _ = process.kill();
            let _ = process.wait();
        }
    }
}

/// A peer process running on a remote machine via SSH
pub(crate) struct SshProcess {
    pub(crate) remote: RemoteMachine,
    pub(crate) remote_pid: Option<u32>,
    pub(crate) remote_data_dir: PathBuf,
    pub(crate) local_cache_dir: PathBuf, // Local cache for downloaded logs
}

impl PeerProcess for SshProcess {
    fn is_running(&self) -> bool {
        if let Some(pid) = self.remote_pid {
            // Check if process is running on remote machine
            let cmd = format!("kill -0 {}", pid);
            self.remote.exec(&cmd).is_ok()
        } else {
            false
        }
    }

    fn kill(&mut self) -> Result<()> {
        if let Some(pid) = self.remote_pid.take() {
            let cmd = format!("kill {}", pid);
            // Ignore errors - process might already be dead
            let _ = self.remote.exec(&cmd);

            // Give it a moment, then force kill if needed
            std::thread::sleep(std::time::Duration::from_millis(500));
            if self.remote.exec(&format!("kill -0 {}", pid)).is_ok() {
                let _ = self.remote.exec(&format!("kill -9 {}", pid));
            }
        }
        Ok(())
    }

    fn log_path(&self) -> PathBuf {
        // Return the local cached log path
        self.local_cache_dir.join("peer.log")
    }

    fn read_logs(&self) -> Result<Vec<LogEntry>> {
        // Download the log file from remote machine
        let remote_log = self.remote_data_dir.join("peer.log");
        let local_log = self.local_cache_dir.join("peer.log");

        // Ensure local cache directory exists
        if let Some(parent) = local_log.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Download via SCP
        self.remote.scp_download(
            remote_log
                .to_str()
                .ok_or_else(|| Error::PeerStartupFailed("Invalid remote log path".to_string()))?,
            &local_log,
        )?;

        // Read the downloaded log file
        crate::logs::read_log_file(&local_log)
    }
}

impl Drop for SshProcess {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

/// Spawn a local peer process
pub(crate) fn spawn_local_peer(
    binary_path: &Path,
    args: &[String],
    data_dir: &Path,
    env_vars: &[(String, String)],
) -> Result<LocalProcess> {
    let log_path = data_dir.join("peer.log");
    let log_file = std::fs::File::create(&log_path)?;

    let mut cmd = Command::new(binary_path);
    // Inherit RUST_LOG from parent process if set, otherwise default to "info"
    let rust_log = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    cmd.args(args)
        .env("RUST_LOG", rust_log)
        .env("RUST_BACKTRACE", "1")
        .stdout(Stdio::from(log_file.try_clone()?))
        .stderr(Stdio::from(log_file));

    for (key, value) in env_vars {
        cmd.env(key, value);
    }

    let process = cmd
        .spawn()
        .map_err(|e| Error::PeerStartupFailed(format!("Failed to spawn local process: {}", e)))?;

    Ok(LocalProcess {
        process: Some(process),
        data_dir: data_dir.to_path_buf(),
    })
}

/// Deploy binary to remote machine and spawn a peer process
pub(crate) async fn spawn_remote_peer(
    local_binary_path: &Path,
    args: &[String],
    remote: &RemoteMachine,
    remote_data_dir: &Path,
    local_cache_dir: &Path,
    env_vars: &[(String, String)],
) -> Result<SshProcess> {
    // Determine remote binary path
    let remote_binary_path = if let Some(binary) = &remote.freenet_binary {
        binary.clone()
    } else {
        // Need to deploy binary via SCP
        let remote_bin_dir = remote.remote_work_dir().join("bin");
        let binary_name = local_binary_path
            .file_name()
            .ok_or_else(|| Error::PeerStartupFailed("Invalid binary path".to_string()))?;
        let remote_binary = remote_bin_dir.join(binary_name);

        // Create remote bin directory
        let mkdir_cmd = format!("mkdir -p {}", remote_bin_dir.display());
        remote.exec(&mkdir_cmd)?;

        // Upload binary
        tracing::debug!(
            "Deploying binary to {}:{}",
            remote.host,
            remote_binary.display()
        );
        remote.scp_upload(
            local_binary_path,
            remote_binary.to_str().ok_or_else(|| {
                Error::PeerStartupFailed("Invalid remote binary path".to_string())
            })?,
        )?;

        remote_binary
    };

    // Create remote data directory
    let mkdir_cmd = format!("mkdir -p {}", remote_data_dir.display());
    remote.exec(&mkdir_cmd)?;

    // Build command to run on remote machine
    let mut cmd_parts = vec![remote_binary_path.to_string_lossy().to_string()];
    cmd_parts.extend(args.iter().map(|s| shell_escape(s)));

    // Add environment variables
    let mut env_prefix = String::new();
    env_prefix.push_str("RUST_LOG=info RUST_BACKTRACE=1 ");
    for (key, value) in env_vars {
        env_prefix.push_str(&format!("{}={} ", shell_escape(key), shell_escape(value)));
    }

    let log_path = remote_data_dir.join("peer.log");
    let full_command = format!(
        "nohup {} {} > {} 2>&1 & echo $!",
        env_prefix,
        cmd_parts.join(" "),
        log_path.display()
    );

    tracing::debug!("Executing remote command: {}", full_command);

    // Execute and capture PID
    let output = remote.exec(&full_command)?;
    let pid: u32 = output
        .trim()
        .parse()
        .map_err(|e| Error::PeerStartupFailed(format!("Failed to parse remote PID: {}", e)))?;

    tracing::debug!("Remote peer started with PID {}", pid);

    // Give it a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    Ok(SshProcess {
        remote: remote.clone(),
        remote_pid: Some(pid),
        remote_data_dir: remote_data_dir.to_path_buf(),
        local_cache_dir: local_cache_dir.to_path_buf(),
    })
}

/// Escape a string for safe use in shell commands
fn shell_escape(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '/' || c == '.')
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_escape() {
        assert_eq!(shell_escape("simple"), "simple");
        assert_eq!(shell_escape("/path/to/file"), "/path/to/file");
        assert_eq!(shell_escape("hello world"), "'hello world'");
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }
}
