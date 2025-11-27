#!/bin/bash
# trace-tx.sh - Trace a transaction ID across all peer logs
#
# Usage:
#   trace-tx.sh <tx_id> <log_dir>
#   trace-tx.sh <tx_id>  # Uses running Docker containers
#
# Shows the message flow as a transaction traverses the network

set -euo pipefail

if [[ $# -lt 1 ]]; then
    echo "Usage: $0 <tx_id> [log_dir]"
    echo ""
    echo "Examples:"
    echo "  $0 01KB1CERB6N7KT9XD0F0JR6F80 /tmp/freenet-test-networks/20251127-001234"
    echo "  $0 01KB1CERB6N7KT9XD0F0JR6F80  # Uses running Docker containers"
    exit 1
fi

TX_ID="$1"
LOG_DIR="${2:-}"

echo "=== Tracing transaction: $TX_ID ==="
echo ""

# Collect logs from Docker or directory
collect_and_trace() {
    local tmpfile=$(mktemp)

    if [[ -n "$LOG_DIR" && -d "$LOG_DIR" ]]; then
        # Read from log directory
        for peer_dir in "$LOG_DIR"/*/; do
            if [[ -d "$peer_dir" ]]; then
                peer_name=$(basename "$peer_dir")
                log_file="$peer_dir/peer.log"
                if [[ -f "$log_file" ]]; then
                    grep "$TX_ID" "$log_file" 2>/dev/null | while IFS= read -r line; do
                        # Strip ANSI codes
                        clean_line=$(echo "$line" | sed 's/\x1b\[[0-9;]*m//g')
                        echo "[$peer_name] $clean_line"
                    done >> "$tmpfile"
                fi
            fi
        done
    else
        # Read from running Docker containers
        for container in $(docker ps --filter "name=freenet-nat" --format "{{.Names}}" 2>/dev/null); do
            peer_name=$(echo "$container" | sed 's/.*-\(gw-[0-9]*\|peer-[0-9]*\)$/\1/' | sed 's/-//')
            docker logs "$container" 2>&1 | grep "$TX_ID" | while IFS= read -r line; do
                clean_line=$(echo "$line" | sed 's/\x1b\[[0-9;]*m//g')
                echo "[$peer_name] $clean_line"
            done >> "$tmpfile"
        done
    fi

    # Sort by timestamp and display
    if [[ -s "$tmpfile" ]]; then
        sort "$tmpfile" | while IFS= read -r line; do
            # Color code by message type
            if echo "$line" | grep -qE "(FindOptimalPeer|CheckConnectivity)"; then
                echo -e "\033[33m$line\033[0m"  # Yellow for requests
            elif echo "$line" | grep -qE "(AcceptedBy|Connected)"; then
                echo -e "\033[32m$line\033[0m"  # Green for success
            elif echo "$line" | grep -qE "(ERROR|Failed|rejected)"; then
                echo -e "\033[31m$line\033[0m"  # Red for errors
            else
                echo "$line"
            fi
        done

        echo ""
        echo "=== Summary ==="
        echo "Total messages: $(wc -l < "$tmpfile")"
        echo "Peers involved: $(cut -d']' -f1 "$tmpfile" | sort -u | tr '\n' ' ')"
    else
        echo "No messages found for transaction $TX_ID"
    fi

    rm -f "$tmpfile"
}

collect_and_trace
