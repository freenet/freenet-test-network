//! Manual end-to-end check of the per-node binary, extra args and extra hosts
//! options against a real Docker NAT network.
//!
//! Needs Docker, an installed `freenet` on PATH (used for the gateway), and a
//! second, different `freenet` binary whose path is in `FREENET_OLD_BINARY`
//! (used for the peer). Run with:
//!
//! ```bash
//! FREENET_OLD_BINARY=/path/to/older/freenet cargo run --example per_node_check
//! ```
//!
//! Builds 1 gateway + 2 peers. The default binary is the old one; the gateway
//! and peer 1 run the installed (new) one and peer 0 keeps the default. Every
//! container gets a `github.com` host entry, peer 0 alone gets another host
//! entry, and peer 1 alone gets an extra CLI argument. It checks from inside
//! the containers that each one runs exactly its configured binary, that the
//! entries and argument appear only where configured, and that the mixed
//! network formed.
//!
//! The command-line check reads `/proc/1/cmdline`, which assumes the
//! container runs the node directly as PID 1 (no entrypoint wrapper).

use freenet_test_network::{Backend, DockerNatConfig, FreenetBinary, TestNetwork};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn docker_exec(container: &str, cmd: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("docker")
        .arg("exec")
        .arg(container)
        .args(cmd)
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "docker exec {container} {cmd:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn sha256_of_file(path: &Path) -> anyhow::Result<String> {
    let out = Command::new("sha256sum").arg(path).output()?;
    anyhow::ensure!(out.status.success(), "sha256sum {} failed", path.display());
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string())
}

/// SHA-256 of the binary a container is running (follows `/proc/1/exe`).
fn running_sha256(container: &str) -> anyhow::Result<String> {
    Ok(docker_exec(container, &["sha256sum", "/proc/1/exe"])?
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let new_binary = FreenetBinary::Installed
        .resolve()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let old_binary = std::path::PathBuf::from(
        std::env::var("FREENET_OLD_BINARY")
            .map_err(|_| anyhow::anyhow!("set FREENET_OLD_BINARY to a second freenet binary"))?,
    );
    let new_sha = sha256_of_file(&new_binary)?;
    let old_sha = sha256_of_file(&old_binary)?;
    anyhow::ensure!(
        new_sha != old_sha,
        "the two binaries are identical, so this check would prove nothing"
    );
    println!("new binary {} sha256 {new_sha}", new_binary.display());
    println!("old binary {} sha256 {old_sha}", old_binary.display());

    let config = DockerNatConfig::default();
    let prefix = config.name_prefix.clone();

    // Default binary is the old one; the gateway and peer 1 override it with
    // the new one. Peer 0 keeps the default. A mis-wired role or index runs the
    // wrong binary and fails the checks below.
    let network = TestNetwork::builder()
        .gateways(1)
        .peers(2)
        .binary(FreenetBinary::Path(old_binary))
        .gateway_binary(0, FreenetBinary::Path(new_binary.clone()))
        .peer_binary(1, FreenetBinary::Path(new_binary))
        .extra_hosts_all(["github.com:127.0.0.1"])
        .peer_extra_hosts(0, ["only-peer0.example:127.0.0.1"])
        .peer_extra_args(1, ["--min-number-of-connections", "1"])
        .backend(Backend::DockerNat(config))
        .require_connectivity(1.0)
        .connectivity_timeout(Duration::from_secs(120))
        .preserve_temp_dirs_on_failure(true)
        .build()
        .await?;
    println!("mixed network formed (readiness check passed)");

    // Container names use the combined index (gateways first); the builder's
    // per-node indices count within each role.
    // (name, expected sha256, has peer-0-only host, has extra arg)
    let nodes = [
        (format!("{prefix}-gw-0"), &new_sha, false, false),
        (format!("{prefix}-peer-1"), &old_sha, true, false),
        (format!("{prefix}-peer-2"), &new_sha, false, true),
    ];

    for (name, expected_sha, has_peer0_host, has_arg) in &nodes {
        let running = running_sha256(name)?;
        anyhow::ensure!(
            &running == *expected_sha,
            "{name} runs {running}, expected {expected_sha}"
        );
        let version = docker_exec(name, &["/proc/1/exe", "--version"])?;
        println!(
            "{name}: runs the configured binary ({})",
            version.lines().next().unwrap_or("")
        );

        let hosts = docker_exec(name, &["cat", "/etc/hosts"])?;
        let has_host = |h: &str| {
            hosts
                .lines()
                .any(|l| l.contains("127.0.0.1") && l.contains(h))
        };
        anyhow::ensure!(
            has_host("github.com"),
            "{name}: /etc/hosts lacks github.com:\n{hosts}"
        );
        anyhow::ensure!(
            has_host("only-peer0.example") == *has_peer0_host,
            "{name}: per-node host entry present={}, expected {has_peer0_host}:\n{hosts}",
            has_host("only-peer0.example")
        );
        println!("{name}: /etc/hosts entries as configured");

        let cmdline = docker_exec(name, &["sh", "-c", "tr '\\0' ' ' < /proc/1/cmdline"])?;
        let present = cmdline.contains("--min-number-of-connections 1");
        anyhow::ensure!(
            present == *has_arg,
            "{name}: extra arg present={present}, expected {has_arg}: {cmdline}"
        );
        println!("{name}: extra arg present={present} as configured");
    }

    println!("OK");
    drop(network);
    Ok(())
}
