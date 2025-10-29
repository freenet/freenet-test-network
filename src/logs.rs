use crate::{Result, TestNetwork};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

/// A log entry from a peer with timestamp
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub peer_id: String,
    pub timestamp: Option<String>,
    pub level: Option<String>,
    pub message: String,
}

impl TestNetwork {
    /// Get all log files from the network
    pub fn log_files(&self) -> Vec<(String, PathBuf)> {
        self.gateways
            .iter()
            .chain(self.peers.iter())
            .map(|p| (p.id().to_string(), p.log_path()))
            .collect()
    }

    /// Read all logs in chronological order
    pub fn read_logs(&self) -> Result<Vec<LogEntry>> {
        let mut entries = Vec::new();

        for (peer_id, log_path) in self.log_files() {
            if let Ok(file) = File::open(&log_path) {
                let reader = BufReader::new(file);
                for line in reader.lines().flatten() {
                    entries.push(parse_log_line(&peer_id, &line));
                }
            }
        }

        // Sort by timestamp if parseable
        entries.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

        Ok(entries)
    }

    /// Tail logs from all peers (blocking)
    pub fn tail_logs(&self) -> Result<impl Iterator<Item = LogEntry> + '_> {
        // TODO: Implement proper log tailing with file watchers
        // For now, just read current logs
        Ok(self.read_logs()?.into_iter())
    }
}

fn parse_log_line(peer_id: &str, line: &str) -> LogEntry {
    // Try to parse structured log format (JSON or key=value)
    // If that fails, treat as plain text

    // Simple heuristic: look for common log patterns
    let (timestamp, level, message) = if let Some(rest) = line.strip_prefix('[') {
        // [2024-10-29T12:34:56Z INFO] message
        let parts: Vec<&str> = rest.splitn(3, ' ').collect();
        if parts.len() >= 3 {
            let ts = parts[0].trim_end_matches(']');
            let lvl = parts[1].trim_end_matches(']');
            (Some(ts.to_string()), Some(lvl.to_string()), parts[2].to_string())
        } else {
            (None, None, line.to_string())
        }
    } else {
        (None, None, line.to_string())
    };

    LogEntry {
        peer_id: peer_id.to_string(),
        timestamp,
        level,
        message,
    }
}
