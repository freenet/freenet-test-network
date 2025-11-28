#!/bin/bash
#
# Cleanup script for freenet-nat Docker resources
#
# This script removes all Docker containers and networks created by the
# freenet-test-network Docker NAT backend. Use this if automatic cleanup
# fails or if you need to force cleanup of stale resources.
#

set -euo pipefail

echo "Cleaning up freenet-nat Docker resources..."

# Count resources before cleanup
CONTAINER_COUNT=$(docker ps -a --filter "name=freenet-nat-" --format "{{.ID}}" | wc -l)
NETWORK_COUNT=$(docker network ls --filter "name=freenet-nat-" --format "{{.ID}}" | wc -l)

echo "Found $CONTAINER_COUNT container(s) and $NETWORK_COUNT network(s)"

if [ "$CONTAINER_COUNT" -eq 0 ] && [ "$NETWORK_COUNT" -eq 0 ]; then
    echo "No freenet-nat resources to clean up"
    exit 0
fi

# Stop and remove containers
if [ "$CONTAINER_COUNT" -gt 0 ]; then
    echo "Stopping and removing containers..."
    docker ps -a --filter "name=freenet-nat-" --format "{{.ID}}" | xargs -r docker rm -f
    echo "Removed $CONTAINER_COUNT container(s)"
fi

# Remove networks
if [ "$NETWORK_COUNT" -gt 0 ]; then
    echo "Removing networks..."
    docker network ls --filter "name=freenet-nat-" --format "{{.ID}}" | xargs -r docker network rm
    echo "Removed $NETWORK_COUNT network(s)"
fi

echo "Cleanup complete!"
