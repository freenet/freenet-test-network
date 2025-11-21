use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};
use freenet_test_network::{write_ring_visualization_from_diagnostics, NetworkDiagnosticsSnapshot};

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let snapshot_path =
        PathBuf::from(args.next().context(
            "usage: cargo run --example ring_viz -- <snapshot.json> <out.html> [run-root]",
        )?);
    let output_path =
        PathBuf::from(args.next().context(
            "usage: cargo run --example ring_viz -- <snapshot.json> <out.html> [run-root]",
        )?);

    let inferred_run_root = snapshot_path
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from);
    let run_root = args
        .next()
        .map(PathBuf::from)
        .or(inferred_run_root)
        .context("could not infer run root from snapshot path; pass it explicitly")?;

    let contents = fs::read_to_string(&snapshot_path)
        .with_context(|| format!("failed to read {}", snapshot_path.display()))?;
    let snapshot: NetworkDiagnosticsSnapshot = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse {}", snapshot_path.display()))?;

    write_ring_visualization_from_diagnostics(&snapshot, &run_root, &output_path)?;
    println!("Wrote ring visualization to {}", output_path.display());
    Ok(())
}
