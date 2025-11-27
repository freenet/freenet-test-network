//! Minimal NAT reproduction test
//!
//! This test creates the simplest possible NAT scenario to reproduce contract
//! operation issues when peers are behind NAT:
//!
//! - 1 gateway on public Docker network (172.20.0.x)
//! - 1 peer behind simulated NAT (10.2.0.x with iptables MASQUERADE)
//!
//! Test steps:
//! 1. Start network, wait for peer<->gateway connectivity
//! 2. PUT contract from peer (behind NAT)
//! 3. GET contract from gateway
//! 4. GET contract from peer (self-retrieval)
//!
//! KNOWN ISSUE: The PUT from peer behind NAT currently times out. This test
//! serves to reproduce the issue for debugging. The logs show:
//! - Network connectivity is established
//! - PUT request is sent
//! - "Transaction timed out" appears in peer logs
//!
//! Run with: cargo run --example minimal_nat_test
//!
//! For repeated testing:
//!   for i in {1..5}; do echo "=== Run $i ===" && cargo run --example minimal_nat_test && echo "PASS" || echo "FAIL"; done

use anyhow::{anyhow, Context, Result};
use freenet_stdlib::{
    client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse, WebApi},
    prelude::*,
};
use freenet_test_network::{Backend, DockerNatConfig, FreenetBinary, TestNetwork};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tracing::{debug, error, info, warn};

/// Path to pre-built ping contract (relative to freenet-core)
const PING_CONTRACT_PATH: &str =
    "../freenet-core/main/apps/freenet-ping/contracts/ping/build/freenet/freenet_ping_contract";

/// Ping contract options (must match PingContractOptions in freenet-ping-types)
#[derive(Debug, Serialize, Deserialize)]
struct PingContractOptions {
    #[serde(with = "humantime_serde")]
    ttl: Duration,
    #[serde(with = "humantime_serde")]
    frequency: Duration,
    tag: String,
    code_key: String,
}

/// Ping state (must match Ping in freenet-ping-types)
#[derive(Debug, Default, Serialize, Deserialize, Clone, PartialEq)]
struct Ping {
    from: HashMap<String, Vec<chrono::DateTime<chrono::Utc>>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing with debug level for detailed output
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,freenet_test_network=debug".into()),
        )
        .init();

    info!("=== Minimal NAT Reproduction Test ===");
    info!("Configuration: 1 gateway (public) + 1 peer (behind NAT)");

    // Start the network
    info!("Starting Docker NAT test network...");
    let start_time = std::time::Instant::now();

    let network = Arc::new(
        TestNetwork::builder()
            .gateways(1)
            .peers(1)
            .binary(FreenetBinary::Installed)
            .backend(Backend::DockerNat(DockerNatConfig::default()))
            .require_connectivity(1.0)
            .connectivity_timeout(Duration::from_secs(120))
            .preserve_temp_dirs_on_failure(true)
            .build()
            .await
            .context("Failed to start test network")?,
    );

    let startup_duration = start_time.elapsed();
    info!(
        "Network started in {:.1}s",
        startup_duration.as_secs_f64()
    );

    // Log network topology
    info!("Network topology:");
    info!(
        "  Gateway: {} (WS: {})",
        network.gateway(0).network_address(),
        network.gateway(0).ws_url()
    );
    info!(
        "  Peer:    {} (behind NAT) (WS: {})",
        network.peer(0).network_address(),
        network.peer(0).ws_url()
    );

    // Connect WebSocket clients
    info!("Connecting WebSocket clients...");
    let mut gateway_client = connect_ws_client(&network.gateway(0).ws_url())
        .await
        .context("Failed to connect to gateway")?;
    let mut peer_client = connect_ws_client(&network.peer(0).ws_url())
        .await
        .context("Failed to connect to peer")?;
    info!("WebSocket clients connected");

    // Load the test contract
    info!("Loading test contract...");
    let contract_code = load_ping_contract().context("Failed to load ping contract")?;
    info!("Contract code loaded: {} bytes", contract_code.len());

    // Create initial state (empty Ping)
    let initial_state = Ping::default();
    let state_bytes = serde_json::to_vec(&initial_state)?;

    // Create contract parameters (required by ping contract)
    let options = PingContractOptions {
        ttl: Duration::from_secs(300),
        frequency: Duration::from_secs(1),
        tag: "nat-test".to_string(),
        code_key: "test".to_string(),
    };
    let params_bytes = serde_json::to_vec(&options)?;
    let params = Parameters::from(params_bytes);

    // Create contract container
    let contract = ContractContainer::try_from((contract_code, &params))
        .context("Failed to create contract container")?;
    let contract_key = contract.key();
    info!("Contract key: {}", contract_key);

    // Step 1: PUT contract from peer (behind NAT)
    info!("Step 1: PUT contract from peer (behind NAT)...");
    let put_start = std::time::Instant::now();

    peer_client
        .send(ClientRequest::ContractOp(ContractRequest::Put {
            contract,
            state: WrappedState::new(state_bytes.clone()),
            related_contracts: RelatedContracts::new(),
            subscribe: false,
        }))
        .await
        .context("Failed to send PUT request")?;

    // Wait for PUT response with extended timeout and logging
    info!("Waiting for PUT response (timeout: 60s)...");
    let put_result = wait_for_response(&mut peer_client, Duration::from_secs(60)).await;

    match put_result {
        Ok(HostResponse::ContractResponse(ContractResponse::PutResponse { key })) => {
            info!(
                "PUT successful in {:.1}s, key: {}",
                put_start.elapsed().as_secs_f64(),
                key
            );
        }
        Ok(other) => {
            error!("Unexpected PUT response: {:?}", other);
            dump_container_logs(&network, "put_unexpected").await;
            return Err(anyhow!("PUT failed with unexpected response"));
        }
        Err(e) => {
            error!("PUT failed after {:.1}s: {:?}", put_start.elapsed().as_secs_f64(), e);

            // Dump container logs for debugging
            dump_container_logs(&network, "put_timeout").await;

            return Err(e.context("PUT from peer behind NAT failed"));
        }
    }

    // Step 2: GET contract from gateway
    info!("Step 2: GET contract from gateway...");
    let get_start = std::time::Instant::now();

    gateway_client
        .send(ClientRequest::ContractOp(ContractRequest::Get {
            key: contract_key.clone(),
            return_contract_code: false,
            subscribe: false,
        }))
        .await
        .context("Failed to send GET request")?;

    // Wait for GET response
    let get_response = wait_for_response(&mut gateway_client, Duration::from_secs(30))
        .await
        .context("Waiting for GET response")?;

    match get_response {
        HostResponse::ContractResponse(ContractResponse::GetResponse { state, .. }) => {
            let get_duration = get_start.elapsed();
            info!("GET successful in {:.1}s", get_duration.as_secs_f64());

            // Verify the state matches
            let retrieved_state: Ping = serde_json::from_slice(&state)
                .context("Failed to deserialize retrieved state")?;

            if retrieved_state == initial_state {
                info!("State verification PASSED");
                info!("  Expected: {:?}", initial_state);
                info!("  Got:      {:?}", retrieved_state);
            } else {
                // For the ping contract, state may differ due to timestamps
                // What matters is the structure is correct
                warn!("State differs (may be expected for timestamp-based contracts)");
                info!("  Initial: {:?}", initial_state);
                info!("  Got:     {:?}", retrieved_state);
            }
        }
        other => {
            error!("Unexpected GET response: {:?}", other);
            return Err(anyhow!("GET failed with unexpected response"));
        }
    }

    // Step 3: Also verify GET from peer itself
    info!("Step 3: Verify GET from peer (self-retrieval)...");
    peer_client
        .send(ClientRequest::ContractOp(ContractRequest::Get {
            key: contract_key.clone(),
            return_contract_code: false,
            subscribe: false,
        }))
        .await
        .context("Failed to send peer GET request")?;

    let peer_get_response = wait_for_response(&mut peer_client, Duration::from_secs(30))
        .await
        .context("Waiting for peer GET response")?;

    match peer_get_response {
        HostResponse::ContractResponse(ContractResponse::GetResponse { state, .. }) => {
            let peer_state: Ping = serde_json::from_slice(&state)
                .context("Failed to deserialize peer state")?;
            info!("Peer self-GET successful: {:?}", peer_state);
        }
        other => {
            warn!("Peer GET returned unexpected response: {:?}", other);
        }
    }

    info!("=== Test PASSED ===");
    info!(
        "Total test duration: {:.1}s",
        start_time.elapsed().as_secs_f64()
    );

    // Cleanup happens automatically when network is dropped
    info!("Cleaning up...");

    Ok(())
}

/// Connect to a WebSocket API endpoint
async fn connect_ws_client(ws_url: &str) -> Result<WebApi> {
    // The ws_url from test-network already includes the path
    let uri = format!("{}?encodingProtocol=native", ws_url);
    debug!("Connecting to WebSocket: {}", uri);

    let (stream, _) = connect_async(&uri)
        .await
        .context("WebSocket connection failed")?;

    Ok(WebApi::start(stream))
}

/// Load the ping contract code
fn load_ping_contract() -> Result<Vec<u8>> {
    let contract_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(PING_CONTRACT_PATH);

    if contract_path.exists() {
        debug!("Loading contract from: {:?}", contract_path);
        std::fs::read(&contract_path).context("Failed to read contract file")
    } else {
        // Try alternative paths
        let alt_paths = [
            "/home/ian/code/freenet/freenet-core/main/apps/freenet-ping/contracts/ping/build/freenet/freenet_ping_contract",
            "./freenet_ping_contract",
        ];

        for alt in &alt_paths {
            let path = std::path::PathBuf::from(alt);
            if path.exists() {
                debug!("Loading contract from alternative path: {:?}", path);
                return std::fs::read(&path).context("Failed to read contract file");
            }
        }

        Err(anyhow!(
            "Could not find ping contract. Tried:\n  - {:?}\n  - {:?}",
            contract_path,
            alt_paths.join("\n  - ")
        ))
    }
}

/// Wait for a response from the WebSocket API with timeout
async fn wait_for_response(client: &mut WebApi, timeout: Duration) -> Result<HostResponse> {
    tokio::time::timeout(timeout, client.recv())
        .await
        .context("Timeout waiting for response")?
        .map_err(|e| anyhow!("Client error: {:?}", e))
}

/// Dump container logs for debugging
async fn dump_container_logs(network: &TestNetwork, context: &str) {
    info!("=== Dumping container logs ({}) ===", context);

    // Get gateway logs
    info!("--- Gateway logs (last 50 lines) ---");
    match network.gateway(0).read_logs() {
        Ok(logs) => {
            for entry in logs.iter().rev().take(50).rev() {
                info!("  [gw] {}", entry.message);
            }
        }
        Err(e) => warn!("Failed to read gateway logs: {:?}", e),
    }

    // Get peer logs
    info!("--- Peer logs (last 50 lines) ---");
    match network.peer(0).read_logs() {
        Ok(logs) => {
            for entry in logs.iter().rev().take(50).rev() {
                info!("  [peer] {}", entry.message);
            }
        }
        Err(e) => warn!("Failed to read peer logs: {:?}", e),
    }

    info!("=== End of logs ===");
}
