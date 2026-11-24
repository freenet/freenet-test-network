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
//! Builds 1 gateway + 1 peer. The gateway runs the installed binary and the
//! peer runs the other one, every container gets a `github.com` host entry,
//! and the peer gets an extra CLI argument. It then checks from inside the
//! containers that each one runs the binary configured for it, that the host
//! entry and argument took effect, and that the mixed network formed.
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
    println!("gateway binary {} sha256 {new_sha}", new_binary.display());
    println!("peer binary    {} sha256 {old_sha}", old_binary.display());

    let config = DockerNatConfig::default();
    let prefix = config.name_prefix.clone();

    let network = TestNetwork::builder()
        .gateways(1)
        .peers(1)
        .binary(FreenetBinary::Path(new_binary.clone()))
        .gateway_binary(0, FreenetBinary::Path(new_binary))
        .peer_binary(0, FreenetBinary::Path(old_binary))
        .extra_hosts_all(["github.com:127.0.0.1"])
        .peer_extra_args(0, ["--min-number-of-connections", "1"])
        .backend(Backend::DockerNat(config))
        .require_connectivity(1.0)
        .connectivity_timeout(Duration::from_secs(120))
        .preserve_temp_dirs_on_failure(true)
        .build()
        .await?;
    println!("mixed network formed (readiness check passed)");

    // Container names use the combined index: the gateway is 0 and the first
    // peer is 1, while the builder's per-node indices count within each role.
    let gw = format!("{prefix}-gw-0");
    let peer = format!("{prefix}-peer-1");

    let gw_running = running_sha256(&gw)?;
    anyhow::ensure!(
        gw_running == new_sha,
        "{gw} runs {gw_running}, expected {new_sha}"
    );
    println!("{gw}: runs the configured gateway binary");
    let peer_running = running_sha256(&peer)?;
    anyhow::ensure!(
        peer_running == old_sha,
        "{peer} runs {peer_running}, expected {old_sha}"
    );
    println!("{peer}: runs the configured peer binary");

    for container in [&gw, &peer] {
        let version = docker_exec(container, &["/proc/1/exe", "--version"])?;
        println!("{container}: {}", version.lines().next().unwrap_or(""));
        let hosts = docker_exec(container, &["cat", "/etc/hosts"])?;
        anyhow::ensure!(
            hosts
                .lines()
                .any(|l| l.contains("127.0.0.1") && l.contains("github.com")),
            "{container}: /etc/hosts lacks the github.com entry:\n{hosts}"
        );
        println!("{container}: /etc/hosts has the github.com entry");
    }

    let cmdline = docker_exec(&peer, &["sh", "-c", "tr '\\0' ' ' < /proc/1/cmdline"])?;
    anyhow::ensure!(
        cmdline.contains("--min-number-of-connections 1"),
        "{peer}: command line lacks the extra arg: {cmdline}"
    );
    println!("{peer}: command line includes the extra arg: {cmdline}");

    let gw_cmdline = docker_exec(&gw, &["sh", "-c", "tr '\\0' ' ' < /proc/1/cmdline"])?;
    anyhow::ensure!(
        !gw_cmdline.contains("--min-number-of-connections"),
        "{gw}: extra arg leaked to the gateway: {gw_cmdline}"
    );
    println!("{gw}: command line has no extra arg (as expected)");

    println!("OK");
    drop(network);
    Ok(())
}
