use crate::{Result, TestNetwork};
use chrono::{DateTime, Utc};
use std::cmp::Ordering;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// A log entry from a peer with timestamp
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub peer_id: String,
    pub timestamp_raw: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
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
                    let parsed = parse_log_line(&peer_id, &line);
                    let is_new_entry = parsed.timestamp.is_some()
                        || parsed.timestamp_raw.is_some()
                        || entries.is_empty();

                    if is_new_entry {
                        entries.push(parsed);
                    } else if let Some(last) = entries.last_mut() {
                        if !parsed.message.is_empty() {
                            if !last.message.is_empty() {
                                last.message.push('\n');
                            }
                            last.message.push_str(&parsed.message);
                        }
                    }
                }
            }
        }

        // Sort by timestamp if parseable
        entries.sort_by(|a, b| match (&a.timestamp, &b.timestamp) {
            (Some(a_ts), Some(b_ts)) => a_ts.cmp(b_ts),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            _ => a.peer_id.cmp(&b.peer_id),
        });

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
    let cleaned = strip_ansi_codes(line);
    let trimmed = cleaned.trim();

    if let Some(entry) = parse_rfc3339_style(peer_id, trimmed) {
        return entry;
    }

    if let Some(entry) = parse_bracket_style(peer_id, trimmed) {
        return entry;
    }

    LogEntry {
        peer_id: peer_id.to_string(),
        timestamp_raw: None,
        timestamp: None,
        level: None,
        message: trimmed.to_string(),
    }
}

fn parse_rfc3339_style(peer_id: &str, line: &str) -> Option<LogEntry> {
    let (ts_token, rest) = split_first_token(line)?;
    let parsed = chrono::DateTime::parse_from_rfc3339(ts_token).ok()?;
    let timestamp = parsed.with_timezone(&Utc);

    let (level_token, remainder) = split_first_token(rest).unwrap_or(("", ""));
    let level = if level_token.is_empty() {
        None
    } else {
        Some(
            level_token
                .trim_matches(|c| c == ':' || c == '-')
                .to_string(),
        )
    };

    Some(LogEntry {
        peer_id: peer_id.to_string(),
        timestamp_raw: Some(ts_token.to_string()),
        timestamp: Some(timestamp),
        level,
        message: remainder.trim().to_string(),
    })
}

fn parse_bracket_style(peer_id: &str, line: &str) -> Option<LogEntry> {
    let rest = line.strip_prefix('[')?;
    let end_idx = rest.find(']')?;
    let inside = &rest[..end_idx];

    let (ts_token, inside_rest) = split_first_token(inside)?;
    let parsed = chrono::DateTime::parse_from_rfc3339(ts_token).ok()?;
    let timestamp = parsed.with_timezone(&Utc);

    let (level_token, _) = split_first_token(inside_rest).unwrap_or(("", ""));
    let level = if level_token.is_empty() {
        None
    } else {
        Some(
            level_token
                .trim_matches(|c| c == ':' || c == '-')
                .to_string(),
        )
    };

    let remainder = rest[end_idx + 1..].trim();

    Some(LogEntry {
        peer_id: peer_id.to_string(),
        timestamp_raw: Some(ts_token.to_string()),
        timestamp: Some(timestamp),
        level,
        message: remainder.to_string(),
    })
}

fn split_first_token(input: &str) -> Option<(&str, &str)> {
    let trimmed = input.trim_start();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(idx) = trimmed.find(char::is_whitespace) {
        let token = &trimmed[..idx];
        let rest = trimmed[idx..].trim_start();
        Some((token, rest))
    } else {
        Some((trimmed, ""))
    }
}

/// Read a single log file and parse its entries
pub(crate) fn read_log_file(log_path: &Path) -> Result<Vec<LogEntry>> {
    let peer_id = log_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let mut entries = Vec::new();

    if let Ok(file) = File::open(log_path) {
        let reader = BufReader::new(file);
        for line in reader.lines().flatten() {
            let parsed = parse_log_line(peer_id, &line);
            let is_new_entry =
                parsed.timestamp.is_some() || parsed.timestamp_raw.is_some() || entries.is_empty();

            if is_new_entry {
                entries.push(parsed);
            } else if let Some(last) = entries.last_mut() {
                if !parsed.message.is_empty() {
                    if !last.message.is_empty() {
                        last.message.push('\n');
                    }
                    last.message.push_str(&parsed.message);
                }
            }
        }
    }

    Ok(entries)
}

fn strip_ansi_codes(input: &str) -> String {
    if !input.contains('\x1b') {
        return input.to_string();
    }

    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if matches!(chars.peek(), Some('[')) {
                chars.next(); // consume '['
                while let Some(&next) = chars.peek() {
                    // ANSI sequence ends with byte in 0x40..=0x7E
                    if ('@'..='~').contains(&next) {
                        chars.next();
                        break;
                    }
                    chars.next();
                }
            }
            continue;
        }
        output.push(ch);
    }

    output
}
