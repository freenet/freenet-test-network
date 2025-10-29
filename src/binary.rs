use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// Specifies which freenet binary to use for the test network
#[derive(Debug, Clone)]
pub enum FreenetBinary {
    /// Build and use binary from current cargo workspace
    CurrentCrate,

    /// Use `freenet` binary from PATH
    Installed,

    /// Use binary at specific path
    Path(PathBuf),

    /// Build binary from a cargo workspace at path
    CargoWorkspace(PathBuf),
}

impl Default for FreenetBinary {
    fn default() -> Self {
        // Smart default: If in freenet-core workspace, use CurrentCrate
        // Otherwise, try Installed
        if is_in_freenet_workspace() {
            Self::CurrentCrate
        } else if which::which("freenet").is_ok() {
            Self::Installed
        } else {
            Self::Installed // Will error in resolve() with helpful message
        }
    }
}

impl FreenetBinary {
    /// Resolve to actual binary path, building if necessary
    pub fn resolve(&self) -> Result<PathBuf> {
        match self {
            Self::CurrentCrate => {
                tracing::info!("Building freenet binary from current workspace");
                build_current_workspace()
            }
            Self::Installed => {
                which::which("freenet")
                    .map_err(|_| Error::InvalidBinary(
                        "freenet binary not found in PATH. Install with: cargo install freenet".into()
                    ))
            }
            Self::Path(p) => {
                if !p.exists() {
                    return Err(Error::InvalidBinary(format!("Binary not found: {}", p.display())));
                }
                Ok(p.clone())
            }
            Self::CargoWorkspace(workspace) => {
                tracing::info!("Building freenet binary from workspace: {}", workspace.display());
                build_workspace(workspace)
            }
        }
    }
}

fn is_in_freenet_workspace() -> bool {
    std::env::current_dir()
        .ok()
        .and_then(|dir| {
            dir.ancestors()
                .find(|p| p.join("Cargo.toml").exists())
                .and_then(|p| std::fs::read_to_string(p.join("Cargo.toml")).ok())
        })
        .map(|content| content.contains("name = \"freenet\""))
        .unwrap_or(false)
}

fn build_current_workspace() -> Result<PathBuf> {
    let output = std::process::Command::new("cargo")
        .args(["build", "--bin", "freenet", "--release"])
        .output()?;

    if !output.status.success() {
        return Err(Error::InvalidBinary(format!(
            "Failed to build freenet: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    // Find the binary in target/release
    let target_dir = get_target_dir()?;
    let binary = target_dir.join("release/freenet");

    if !binary.exists() {
        return Err(Error::InvalidBinary("Built binary not found in target/release".into()));
    }

    Ok(binary)
}

fn build_workspace(workspace: &Path) -> Result<PathBuf> {
    let output = std::process::Command::new("cargo")
        .args(["build", "--bin", "freenet", "--release"])
        .current_dir(workspace)
        .output()?;

    if !output.status.success() {
        return Err(Error::InvalidBinary(format!(
            "Failed to build freenet in {}: {}",
            workspace.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let target_dir = workspace.join("target");
    let binary = target_dir.join("release/freenet");

    if !binary.exists() {
        return Err(Error::InvalidBinary("Built binary not found in target/release".into()));
    }

    Ok(binary)
}

fn get_target_dir() -> Result<PathBuf> {
    // Check CARGO_TARGET_DIR env var first
    if let Ok(target) = std::env::var("CARGO_TARGET_DIR") {
        return Ok(PathBuf::from(target));
    }

    // Find workspace root and use target/ there
    std::env::current_dir()?
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists())
        .map(|p| p.join("target"))
        .ok_or_else(|| Error::InvalidBinary("Could not find workspace root".into()))
}
