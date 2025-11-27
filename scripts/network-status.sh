#!/bin/bash
# network-status.sh - Show current network topology and connection status
#
# Usage:
#   network-status.sh          # Query running Docker containers
#   network-status.sh --watch  # Continuously monitor (refresh every 5s)
#
# Queries each peer via WebSocket for connection count

set -euo pipefail

WATCH_MODE=false
REFRESH_INTERVAL=5

while [[ $# -gt 0 ]]; do
    case $1 in
        --watch|-w)
            WATCH_MODE=true
            shift
            ;;
        --interval|-i)
            REFRESH_INTERVAL="$2"
            shift 2
            ;;
        *)
            echo "Unknown option: $1"
            echo "Usage: $0 [--watch] [--interval <seconds>]"
            exit 1
            ;;
    esac
done

query_peer_connections() {
    local ws_port="$1"
    local timeout=3

    # Use websocat if available, otherwise fall back to netcat
    if command -v websocat &> /dev/null; then
        # Query via WebSocket
        response=$(echo '{"request":"NodeQueries","query":"ConnectedPeers"}' | \
            timeout "$timeout" websocat -n1 "ws://127.0.0.1:$ws_port/v1/contract/command?encodingProtocol=native" 2>/dev/null || echo "")
        if [[ -n "$response" ]]; then
            # Parse JSON response to count peers
            echo "$response" | grep -oE '"peers":\s*\[[^\]]*\]' | grep -oE '\{[^}]+\}' | wc -l 2>/dev/null || echo "?"
        else
            echo "err"
        fi
    else
        echo "?"
    fi
}

show_status() {
    local timestamp=$(date '+%Y-%m-%d %H:%M:%S')
    echo "=== Network Status at $timestamp ==="
    echo ""

    # Get list of running containers
    local containers=$(docker ps --filter "name=freenet-nat" --format "{{.Names}}:{{.Ports}}" 2>/dev/null | sort)

    if [[ -z "$containers" ]]; then
        echo "No running freenet-nat containers found"
        return
    fi

    local total_peers=0
    local connected_peers=0
    local status_lines=()

    while IFS= read -r container_info; do
        local container_name=$(echo "$container_info" | cut -d: -f1)
        local ports=$(echo "$container_info" | cut -d: -f2-)

        # Extract peer name from container name (e.g., freenet-nat-12345-peer-1 -> peer-1)
        local peer_name=$(echo "$container_name" | sed 's/freenet-nat-[0-9]*-//')

        # Skip routers
        if [[ "$peer_name" == router-* ]]; then
            continue
        fi

        # Extract host WebSocket port
        local ws_port=$(echo "$ports" | grep -oE '0\.0\.0\.0:[0-9]+->9000' | cut -d: -f2 | cut -d'-' -f1)

        if [[ -z "$ws_port" ]]; then
            status_lines+=("$peer_name: no port mapping")
            continue
        fi

        # Query connection count (would need websocat or similar)
        # For now just show port info
        local connections="?"

        # Try to get connection count from container logs
        local log_connections=$(docker logs "$container_name" 2>&1 | \
            grep -oE 'current_connections: [0-9]+' | tail -1 | grep -oE '[0-9]+' || echo "?")

        if [[ "$log_connections" != "?" && "$log_connections" -gt 0 ]]; then
            connections="$log_connections"
            ((connected_peers++)) || true
        fi

        ((total_peers++)) || true
        status_lines+=("$peer_name: $connections connections (ws://127.0.0.1:$ws_port)")
    done <<< "$containers"

    # Display status
    local connectivity_pct=0
    if [[ $total_peers -gt 0 ]]; then
        connectivity_pct=$((connected_peers * 100 / total_peers))
    fi

    echo "Connectivity: $connected_peers/$total_peers ($connectivity_pct%)"
    echo ""
    echo "Peers:"
    for line in "${status_lines[@]}"; do
        if echo "$line" | grep -qE ': 0 connections|: \? connections|: err connections'; then
            echo -e "  \033[31m$line\033[0m"  # Red for unconnected
        else
            echo -e "  \033[32m$line\033[0m"  # Green for connected
        fi
    done
    echo ""

    # Show network info
    local networks=$(docker network ls --filter "name=freenet-nat" --format "{{.Name}}" 2>/dev/null)
    if [[ -n "$networks" ]]; then
        echo "Docker Networks:"
        while IFS= read -r net; do
            local subnet=$(docker network inspect "$net" --format '{{range .IPAM.Config}}{{.Subnet}}{{end}}' 2>/dev/null || echo "?")
            echo "  $net: $subnet"
        done <<< "$networks"
    fi
}

# Main
if [[ "$WATCH_MODE" == "true" ]]; then
    while true; do
        clear
        show_status
        echo "Refreshing in ${REFRESH_INTERVAL}s... (Ctrl+C to stop)"
        sleep "$REFRESH_INTERVAL"
    done
else
    show_status
fi
