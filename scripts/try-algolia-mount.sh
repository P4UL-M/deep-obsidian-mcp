#!/usr/bin/env bash
# Spin up a throwaway sandbox that mounts an EXISTING Algolia index, so an agent can
# be pointed at a shared corpus without touching your real setup.
#
# Adapted from PR #40's try-shared-wiki.sh. What changed: the mount definition is
# read out of the `mounts` table (backend.kind == "algolia") rather than the old
# top-level `shared[]` array, the sandbox config emits a `mounts` table with the
# experimental gates set, and the smoke test calls `algolia status --mount <id>`.
#
# What this does NOT touch:
#   - ~/.config/deep-obsidian-mcp/config.json   (read once, never written)
#   - your real vault                           (the sandbox gets its own)
#   - the running service on :4100              (stdio transport, no port)
#   - your global MCP client config             (project-scoped .mcp.json)
#   - the Algolia API key                       (only the secret REFERENCE is copied;
#                                                the value stays in your keyring)
#
# Read-only by default: writes to the mount are refused. Pass --writable to let the
# agent author into the shared index. A superseded note stays recoverable through
# note_history / read_version — but take an `algolia dump` first anyway.
#
# Usage:
#   scripts/try-algolia-mount.sh                  # read-only sandbox
#   scripts/try-algolia-mount.sh --writable       # allow mount writes
#   scripts/try-algolia-mount.sh --mount <id>     # pick one of several algolia mounts
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$REPO/target/debug/deep-obsidian-mcp"
SRC_CONFIG="${DEEP_OBSIDIAN_CONFIG:-$HOME/.config/deep-obsidian-mcp/config.json}"
WRITABLE=false
MOUNT_FILTER=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --writable) WRITABLE=true; shift ;;
    --mount) MOUNT_FILTER="${2:-}"; shift 2 ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

[[ -f "$SRC_CONFIG" ]] || {
  echo "no config at $SRC_CONFIG" >&2
  echo "set DEEP_OBSIDIAN_CONFIG, or run: deep-obsidian-mcp setup-service --wizard" >&2
  exit 1
}

echo "Building the debug binary…"
(cd "$REPO" && cargo build -q -p deep-obsidian-cli)

SANDBOX="$(mktemp -d /tmp/deep-obsidian-try.XXXXXX)"
mkdir -p "$SANDBOX/vault" "$SANDBOX/index"

# One local note, so you can see local and shared content coexist under one namespace.
cat > "$SANDBOX/vault/Sandbox note.md" <<'EOF'
# Sandbox note

A purely local note. Only this file lives on disk; everything under the algolia
mount's prefix is served from the Algolia index.
EOF

# Copy the algolia mount out of the real config's `mounts` table. Only appId,
# indexName and the secret REFERENCE travel; the key itself stays where it is.
python3 - "$SRC_CONFIG" "$SANDBOX" "$WRITABLE" "$MOUNT_FILTER" <<'PY'
import json, sys

src, sandbox, writable, mount_filter = sys.argv[1:5]
config = json.load(open(src))

candidates = [
    m for m in (config.get("mounts") or [])
    if (m.get("backend") or {}).get("kind") == "algolia"
]
if mount_filter:
    candidates = [m for m in candidates if m.get("id") == mount_filter]
if not candidates:
    sys.exit(
        f"no algolia mount found in {src}"
        + (f" with id {mount_filter}" if mount_filter else "")
        + "\nAdd one under `mounts` (see CONFIGURATION.md § Multiple vaults)."
    )
if len(candidates) > 1:
    ids = ", ".join(m.get("id", "?") for m in candidates)
    sys.exit(f"several algolia mounts configured ({ids}); pick one with --mount <id>")

mount = json.loads(json.dumps(candidates[0]))          # deep copy
backend = mount["backend"]
backend["writable"] = writable == "true"
# The sandbox keeps its cache to itself, so it cannot evict from the real one.
backend["indexDir"] = f"{sandbox}/index/mounts/{mount['id']}"

# No top-level `vaultPath`: a config sets that OR `mounts`, never both
# (ConfigError::VaultPathAndMountsBothSet), so the sandbox vault is the root mount.
json.dump({
    "indexDir": f"{sandbox}/index",
    "transport": "stdio",
    "autoReindex": {"enabled": True, "debounceMs": 1500, "intervalMs": 30000},
    "experimental": {"multiVault": True, "algoliaVaults": True},
    "mounts": [
        {
            "id": "vault",
            "mountAt": "",
            "backend": {"kind": "filesystem", "vaultPath": f"{sandbox}/vault"},
        },
        mount,
    ],
}, open(f"{sandbox}/config.json", "w"), indent=2)

print(f"mount: {mount['id']} (index {backend['indexName']}, app {backend['appId']}) "
      f"at '{mount.get('mountAt', '')}'  "
      f"{'WRITABLE' if backend['writable'] else 'read-only'}")
print(mount["id"], file=open(f"{sandbox}/.mount-id", "w"))
PY

MOUNT_ID="$(cat "$SANDBOX/.mount-id")"

# Project-scoped MCP registration: Claude Code picks this up from the working
# directory, so your global client config is untouched.
cat > "$SANDBOX/.mcp.json" <<EOF
{
  "mcpServers": {
    "algolia_mount": {
      "command": "$BIN",
      "args": ["--config", "$SANDBOX/config.json", "--transport", "stdio"]
    }
  }
}
EOF

echo
echo "Sandbox ready: $SANDBOX"
echo
echo "Smoke test (no agent needed):"
echo "  $BIN --config $SANDBOX/config.json algolia status --mount $MOUNT_ID"
echo "  $BIN --config $SANDBOX/config.json doctor"
echo
echo "Point an agent at it:"
echo "  cd $SANDBOX && claude"
echo
echo "Then ask it things like:"
echo "  - what folders exist under the mount's prefix?"
echo "  - search the shared corpus for <topic>"
echo "  - read <a note path under the prefix>"
if [[ "$WRITABLE" == "true" ]]; then
  echo
  echo "WRITABLE: the agent can author into mount '$MOUNT_ID'."
  echo "Back it up first:"
  echo "  $BIN --config $SANDBOX/config.json algolia dump --mount $MOUNT_ID --out ~/Backups/$MOUNT_ID"
fi
echo
echo "Throw it away when done:  rm -rf $SANDBOX"
