//! Reliable test network infrastructure for Freenet
//!
//! This crate provides tools for spinning up and managing local Freenet test networks
//! for integration testing.

mod binary;
mod builder;
mod network;
mod peer;

pub use binary::FreenetBinary;
pub use builder::NetworkBuilder;
pub use network::{TestNetwork, NetworkTopology};
pub use peer::TestPeer;

/// Result type used throughout this crate
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur when managing test networks
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Failed to start peer: {0}")]
    PeerStartupFailed(String),

    #[error("Network connectivity check failed: {0}")]
    ConnectivityFailed(String),

    #[error("Binary not found or invalid: {0}")]
    InvalidBinary(String),

    #[error("Port allocation failed: {0}")]
    PortAllocationFailed(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Other error: {0}")]
    Other(#[from] anyhow::Error),
}
