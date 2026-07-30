#!/usr/bin/env bash
# Spin up a throwaway sandbox that mounts an EXISTING shared Algolia index, so
# an agent can be pointed at it without touching your real setup.
#
# What it does NOT touch:
#   - ~/.config/deep-obsidian-mcp/config.json  (read once, never written)
#   - your real vault                          (the sandbox gets its own)
#   - the running service on :4100              (stdio transport, no port)
#   - your global MCP client config             (project-scoped .mcp.json)
#
# Read-only by default: writes to the mount are rejected. Pass --writable to
# let the agent author into the shared index (versioned — a superseded note is
# recoverable via note_history / read_version, but see `share retract` if you
# want it gone).
#
# Usage:
#   scripts/try-shared-wiki.sh                 # read-only sandbox
#   scripts/try-shared-wiki.sh --writable      # allow mount writes
#   scripts/try-shared-wiki.sh --index <name>  # pick a mount by index name
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$REPO/target/debug/deep-obsidian-mcp"
SRC_CONFIG="${DEEP_OBSIDIAN_CONFIG:-$HOME/.config/deep-obsidian-mcp/config.json}"
WRITABLE=false
INDEX_FILTER=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --writable) WRITABLE=true; shift ;;
    --index) INDEX_FILTER="$2"; shift 2 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

[[ -f "$SRC_CONFIG" ]] || { echo "no config at $SRC_CONFIG" >&2; exit 1; }

echo "Building the debug binary…"
(cd "$REPO" && cargo build -q -p deep-obsidian-cli)

SANDBOX="$(mktemp -d /tmp/deep-obsidian-try.XXXXXX)"
mkdir -p "$SANDBOX/vault" "$SANDBOX/index"

# One local note, so you can see local and shared content coexist in search.
cat > "$SANDBOX/vault/Sandbox note.md" <<'EOF'
# Sandbox note

A purely local note. Only this file lives on disk; everything under
_Shared/ is served from the Algolia index.
EOF

# Copy the mount definition (appId / indexName / keyRef) out of the real
# config. The key itself stays in the keyring — only the reference is copied.
python3 - "$SRC_CONFIG" "$SANDBOX" "$WRITABLE" "$INDEX_FILTER" <<'PY'
import json, sys, pathlib

src, sandbox, writable, index_filter = sys.argv[1:5]
config = json.load(open(src))
mounts = config.get("shared") or []
if index_filter:
    mounts = [m for m in mounts if m.get("indexName") == index_filter]
if not mounts:
    sys.exit(f"no shared mount found in {src}"
             + (f" with indexName {index_filter}" if index_filter else ""))
if len(mounts) > 1:
    sys.exit("several mounts configured; pick one with --index <name>")

mount = dict(mounts[0])
# Drop any legacy field and force the safety posture for this sandbox.
mount.pop("export", None)
mount["writable"] = writable == "true"

json.dump({
    "vaultPath": f"{sandbox}/vault",
    "indexDir": f"{sandbox}/index",
    "transport": "stdio",
    "autoReindex": {"enabled": True, "debounceMs": 1500, "intervalMs": 30000},
    "shared": [mount],
}, open(f"{sandbox}/config.json", "w"), indent=2)

print(f"mount: {mount['indexName']} (app {mount['appId']}) at {mount['mountAt']}"
      f"  {'WRITABLE' if mount['writable'] else 'read-only'}")
PY

# Project-scoped MCP registration: Claude Code picks this up from the working
# directory, so your global client config is untouched.
cat > "$SANDBOX/.mcp.json" <<EOF
{
  "mcpServers": {
    "shared_wiki": {
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
printf '  %s --config %s share status\n' "$BIN" "$SANDBOX/config.json"
echo
echo "Point an agent at it:"
echo "  cd $SANDBOX && claude"
echo
echo "Then ask it things like:"
echo "  - what folders exist under _Shared/Team/ ?"
echo "  - search the shared wiki for <topic>"
echo "  - read _Shared/Team/<a note path>"
if [[ "$WRITABLE" == "true" ]]; then
  echo
  echo "WRITABLE: the agent can author into index '$(python3 -c "
import json;print(json.load(open('$SANDBOX/config.json'))['shared'][0]['indexName'])")'."
  echo "Back it up first:  $BIN --config $SANDBOX/config.json share dump --to ~/Backups/shared-wiki"
fi
echo
echo "Throw it away when done:  rm -rf $SANDBOX"
