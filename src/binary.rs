use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// Build profile for compiling freenet
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildProfile {
    Debug,
    Release,
}

/// Specifies which freenet binary to use for the test network
#[derive(Debug, Clone)]
pub enum FreenetBinary {
    /// Build and use binary from current cargo workspace
    CurrentCrate(BuildProfile),

    /// Use `freenet` binary from PATH
    Installed,

    /// Use binary at specific path
    Path(PathBuf),

    /// Build binary from a cargo workspace at specified path
    /// Use this for worktrees or when freenet-core is in a different location
    Workspace { path: PathBuf, profile: BuildProfile },
}

impl Default for FreenetBinary {
    fn default() -> Self {
        // Smart default: If in freenet-core workspace, use CurrentCrate with debug
        // Otherwise, try Installed
        if is_in_freenet_workspace() {
            Self::CurrentCrate(BuildProfile::Debug)
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
            Self::CurrentCrate(profile) => {
                tracing::info!("Building freenet binary from current workspace ({:?})", profile);
                build_current_workspace(*profile)
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
            Self::Workspace { path, profile } => {
                tracing::info!("Building freenet binary from workspace: {} ({:?})", path.display(), profile);
                build_workspace(path, *profile)
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

fn build_current_workspace(profile: BuildProfile) -> Result<PathBuf> {
    // Get workspace root BEFORE building
    // Find the Cargo workspace root (with [workspace] section), not just any Cargo.toml
    let cwd = std::env::current_dir()?;
    let workspace_root = cwd
        .ancestors()
        .find(|p| {
            let cargo_toml = p.join("Cargo.toml");
            if !cargo_toml.exists() {
                return false;
            }
            // Check if this is a workspace root
            std::fs::read_to_string(&cargo_toml)
                .map(|content| content.contains("[workspace]"))
                .unwrap_or(false)
        })
        .or_else(|| {
            // Fallback: just find any Cargo.toml with freenet binary
            cwd.ancestors().find(|p| {
                let cargo_toml = p.join("Cargo.toml");
                if !cargo_toml.exists() {
                    return false;
                }
                std::fs::read_to_string(&cargo_toml)
                    .map(|content| content.contains("name = \"freenet\""))
                    .unwrap_or(false)
            })
        })
        .ok_or_else(|| Error::InvalidBinary("Could not find workspace root with freenet binary".into()))?
        .to_path_buf();

    let profile_arg = match profile {
        BuildProfile::Debug => vec!["build"],
        BuildProfile::Release => vec!["build", "--release"],
    };

    let mut cmd = std::process::Command::new("cargo");
    cmd.args(&profile_arg)
        .args(["--bin", "freenet"])
        .current_dir(&workspace_root);

    let output = cmd.output()?;

    if !output.status.success() {
        return Err(Error::InvalidBinary(format!(
            "Failed to build freenet: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    // Use workspace_root's target directory
    let target_dir = if let Ok(target) = std::env::var("CARGO_TARGET_DIR") {
        PathBuf::from(target)
    } else {
        workspace_root.join("target")
    };

    let profile_dir = match profile {
        BuildProfile::Debug => "debug",
        BuildProfile::Release => "release",
    };
    let binary = target_dir.join(format!("{}/freenet", profile_dir));

    if !binary.exists() {
        return Err(Error::InvalidBinary(format!(
            "Built binary not found at: {} (workspace: {}, target_dir: {})",
            binary.display(),
            workspace_root.display(),
            target_dir.display()
        )));
    }

    Ok(binary)
}

fn build_workspace(workspace: &Path, profile: BuildProfile) -> Result<PathBuf> {
    let profile_arg = match profile {
        BuildProfile::Debug => vec!["build"],
        BuildProfile::Release => vec!["build", "--release"],
    };

    let mut cmd = std::process::Command::new("cargo");
    cmd.args(&profile_arg)
        .args(["--bin", "freenet"])
        .current_dir(workspace);

    let output = cmd.output()?;

    if !output.status.success() {
        return Err(Error::InvalidBinary(format!(
            "Failed to build freenet in {}: {}",
            workspace.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let target_dir = workspace.join("target");
    let profile_dir = match profile {
        BuildProfile::Debug => "debug",
        BuildProfile::Release => "release",
    };
    let binary = target_dir.join(format!("{}/freenet", profile_dir));

    if !binary.exists() {
        return Err(Error::InvalidBinary(format!(
            "Built binary not found in target/{}",
            profile_dir
        )));
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
