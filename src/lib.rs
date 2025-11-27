//! Reliable test network infrastructure for Freenet
//!
//! This crate provides tools for spinning up and managing local Freenet test networks
//! for integration testing.

mod binary;
mod builder;
pub mod docker;
mod logs;
mod network;
mod peer;
mod process;
mod remote;

pub use binary::{BuildProfile, FreenetBinary};
pub use builder::{Backend, NetworkBuilder};
pub use docker::{DockerNatBackend, DockerNatConfig, DockerPeerInfo, NatTopology, NatType};
pub use logs::LogEntry;
pub use network::{
    ring_nodes_from_diagnostics, write_ring_visualization_from_diagnostics,
    NetworkDiagnosticsSnapshot, NetworkTopology, PeerDiagnosticsSnapshot, RingPeerSnapshot,
    RingVizMetrics, TestNetwork,
};
pub use peer::TestPeer;
pub use remote::{PeerLocation, RemoteMachine};

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
