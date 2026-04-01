#!/bin/zsh
set -euo pipefail

LABEL="${DEEP_OBSIDIAN_LABEL:-io.deep-obsidian-mcp}"
PLIST_PATH="${HOME}/Library/LaunchAgents/${LABEL}.plist"

launchctl bootout "gui/$(id -u)" "${PLIST_PATH}" 2>/dev/null || true
rm -f "${PLIST_PATH}"

echo "Removed ${LABEL}"
echo "plist: ${PLIST_PATH}"
