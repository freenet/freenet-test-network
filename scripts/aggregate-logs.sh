#!/bin/bash
# aggregate-logs.sh - Aggregate and merge logs from multiple Freenet peers
#
# Usage:
#   aggregate-logs.sh <log_dir>              # Aggregate all peer logs
#   aggregate-logs.sh <log_dir> --grep <pattern>  # Filter by pattern
#   aggregate-logs.sh <log_dir> --trace <tx_id>   # Trace a transaction
#
# Output: Merged logs sorted by timestamp with peer ID prefix

set -euo pipefail

usage() {
    echo "Usage: $0 <log_dir> [options]"
    echo ""
    echo "Options:"
    echo "  --grep <pattern>    Filter logs matching pattern"
    echo "  --trace <tx_id>     Trace a specific transaction across peers"
    echo "  --level <level>     Filter by log level (INFO, WARN, ERROR, DEBUG)"
    echo "  --peer <peer_id>    Show logs only from specific peer"
    echo "  --last <n>          Show only last n lines"
    echo "  --connections       Show only connection-related messages"
    echo "  --help              Show this help"
    exit 1
}

LOG_DIR=""
GREP_PATTERN=""
TRACE_TX=""
LEVEL_FILTER=""
PEER_FILTER=""
LAST_N=""
CONNECTIONS_ONLY=false

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --grep)
            GREP_PATTERN="$2"
            shift 2
            ;;
        --trace)
            TRACE_TX="$2"
            shift 2
            ;;
        --level)
            LEVEL_FILTER="$2"
            shift 2
            ;;
        --peer)
            PEER_FILTER="$2"
            shift 2
            ;;
        --last)
            LAST_N="$2"
            shift 2
            ;;
        --connections)
            CONNECTIONS_ONLY=true
            shift
            ;;
        --help|-h)
            usage
            ;;
        -*)
            echo "Unknown option: $1"
            usage
            ;;
        *)
            if [[ -z "$LOG_DIR" ]]; then
                LOG_DIR="$1"
            else
                echo "Unexpected argument: $1"
                usage
            fi
            shift
            ;;
    esac
done

if [[ -z "$LOG_DIR" ]]; then
    echo "Error: log directory required"
    usage
fi

if [[ ! -d "$LOG_DIR" ]]; then
    echo "Error: $LOG_DIR is not a directory"
    exit 1
fi

# Function to extract timestamp from log line
# Handles format: 2025-11-27T00:47:17.864134Z
extract_timestamp() {
    echo "$1" | grep -oE '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]+Z?' | head -1
}

# Collect and prefix logs from all peers
collect_logs() {
    local tmpfile=$(mktemp)

    # Process each peer directory
    for peer_dir in "$LOG_DIR"/*/; do
        if [[ -d "$peer_dir" ]]; then
            peer_name=$(basename "$peer_dir")
            log_file="$peer_dir/peer.log"

            # Skip if no log file
            if [[ ! -f "$log_file" ]]; then
                # Try Docker log collection for running containers
                if docker ps --filter "name=freenet-nat.*${peer_name}" --format "{{.Names}}" 2>/dev/null | grep -q .; then
                    container=$(docker ps --filter "name=freenet-nat.*${peer_name}" --format "{{.Names}}" | head -1)
                    if [[ -n "$container" ]]; then
                        docker logs "$container" 2>&1 | while IFS= read -r line; do
                            # Strip ANSI codes
                            clean_line=$(echo "$line" | sed 's/\x1b\[[0-9;]*m//g')
                            ts=$(extract_timestamp "$clean_line")
                            if [[ -n "$ts" ]]; then
                                echo "[$peer_name] $clean_line"
                            fi
                        done >> "$tmpfile"
                    fi
                fi
                continue
            fi

            # Read log file and prefix with peer name
            while IFS= read -r line; do
                # Strip ANSI codes
                clean_line=$(echo "$line" | sed 's/\x1b\[[0-9;]*m//g')
                ts=$(extract_timestamp "$clean_line")
                if [[ -n "$ts" ]]; then
                    echo "[$peer_name] $clean_line"
                fi
            done < "$log_file" >> "$tmpfile"
        fi
    done

    echo "$tmpfile"
}

# Main processing
tmpfile=$(collect_logs)

# Apply filters
result="$tmpfile"

if [[ -n "$PEER_FILTER" ]]; then
    filtered=$(mktemp)
    grep "^\[$PEER_FILTER\]" "$result" > "$filtered" || true
    result="$filtered"
fi

if [[ -n "$LEVEL_FILTER" ]]; then
    filtered=$(mktemp)
    grep -i " $LEVEL_FILTER " "$result" > "$filtered" || true
    result="$filtered"
fi

if [[ -n "$GREP_PATTERN" ]]; then
    filtered=$(mktemp)
    grep -E "$GREP_PATTERN" "$result" > "$filtered" || true
    result="$filtered"
fi

if [[ -n "$TRACE_TX" ]]; then
    filtered=$(mktemp)
    grep "$TRACE_TX" "$result" > "$filtered" || true
    result="$filtered"
fi

if [[ "$CONNECTIONS_ONLY" == "true" ]]; then
    filtered=$(mktemp)
    grep -E "(connection|connect|Connected|disconnect|Disconnected|peer.*joined|peer.*left|current_connections)" "$result" > "$filtered" || true
    result="$filtered"
fi

# Sort by timestamp (field after [peer_name])
sorted=$(mktemp)
sort -t'T' -k1,1 "$result" > "$sorted" 2>/dev/null || cat "$result" > "$sorted"

# Apply --last filter
if [[ -n "$LAST_N" ]]; then
    tail -n "$LAST_N" "$sorted"
else
    cat "$sorted"
fi

# Cleanup
rm -f "$tmpfile" "$sorted" 2>/dev/null || true
