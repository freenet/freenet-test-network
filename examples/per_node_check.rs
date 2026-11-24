//! Manual check of the per-node binary, extra args and extra hosts options
//! against a real Docker NAT network.
//!
//! Needs Docker and a `freenet` binary on PATH. Run with:
//!
//! ```bash
//! cargo run --example per_node_check
//! ```
//!
//! Builds 1 gateway + 1 peer, gives the peer its binary explicitly, adds a
//! host entry to every container and an extra CLI argument to the peer, then
//! checks from inside the containers that both took effect.

use freenet_test_network::{Backend, DockerNatConfig, FreenetBinary, TestNetwork};
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let binary = which_freenet()?;
    let config = DockerNatConfig::default();
    let prefix = config.name_prefix.clone();

    let network = TestNetwork::builder()
        .gateways(1)
        .peers(1)
        .binary(FreenetBinary::Path(binary.clone()))
        .peer_binary(0, FreenetBinary::Path(binary))
        .extra_hosts_all(["github.com:127.0.0.1"])
        .peer_extra_args(0, ["--min-number-of-connections", "1"])
        .backend(Backend::DockerNat(config))
        .require_connectivity(1.0)
        .connectivity_timeout(Duration::from_secs(120))
        .preserve_temp_dirs_on_failure(true)
        .build()
        .await?;
    println!("network formed (readiness check passed)");

    // Gateway is container index 0, the first peer is container index 1.
    let gw = format!("{prefix}-gw-0");
    let peer = format!("{prefix}-peer-1");

    for container in [&gw, &peer] {
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

fn which_freenet() -> anyhow::Result<std::path::PathBuf> {
    FreenetBinary::Installed
        .resolve()
        .map_err(|e| anyhow::anyhow!("{e}"))
}
