#!/usr/bin/env bash
# Post-install smoke test for the deep-obsidian-mcp Debian/Ubuntu package.
#
# Assumes the .deb is already installed (apt repo or local file) and runs as
# root inside a container or CI runner. Exercises the full packaged surface:
# binary + ripgrep presence, installed files, the systemd user unit,
# `setup-service`, the HTTP transport (health + MCP initialize), and the
# stdio transport.
#
# Usage: linux-smoke-test.sh
# Exit code 0 = all checks passed.
set -uo pipefail

FAIL=0

# The one path the binary's exe-relative probe derives for an installed layout:
# /usr/bin/deep-obsidian-mcp walks up to /usr and appends
# share/deep-obsidian-mcp/<bundle> (PACKAGED_BUNDLE_PREFIX in
# deep-obsidian-backend/src/sidecar.rs). Hard-coded here on purpose — this file's job is
# to catch the packaging and the probe drifting apart.
SIDECAR_BUNDLE=/usr/share/deep-obsidian-mcp/sidecar/livesync-sidecar/dist/sidecar.mjs
step() { echo; echo "=== $* ==="; }
check() {
  local desc="$1"; shift
  if "$@"; then
    echo "PASS: $desc"
  else
    echo "FAIL: $desc (cmd: $*)"
    FAIL=1
  fi
}

step "Binary and dependency presence"
check "deep-obsidian-mcp on PATH" command -v deep-obsidian-mcp
check "ripgrep (rg) on PATH" command -v rg
deep-obsidian-mcp version || FAIL=1

step "Packaged files"
check "skills dir installed" test -d /usr/share/deep-obsidian-mcp/skills
check "capture-session skill" test -f /usr/share/deep-obsidian-mcp/skills/obsidian-capture-session/SKILL.md
check "obsidian-snippets installed" test -d /usr/share/deep-obsidian-mcp/obsidian-snippets
check "assets installed" test -d /usr/share/deep-obsidian-mcp/assets
check "systemd user unit installed" test -f /usr/lib/systemd/user/deep-obsidian-mcp.service
check "livesync sidecar bundle installed" test -s "$SIDECAR_BUNDLE"

step "systemd unit sanity"
if command -v systemd-analyze >/dev/null 2>&1; then
  systemd-analyze verify /usr/lib/systemd/user/deep-obsidian-mcp.service \
    && echo "PASS: systemd-analyze verify" \
    || echo "WARN: systemd-analyze verify reported issues"
else
  echo "SKIP: systemd-analyze not available in this image"
fi
UNIT_EXEC=$(grep '^ExecStart=' /usr/lib/systemd/user/deep-obsidian-mcp.service | cut -d= -f2- | awk '{print $1}')
check "unit ExecStart binary exists ($UNIT_EXEC)" test -x "$UNIT_EXEC"

step "Create a tiny test vault"
VAULT="$HOME/TestVault"
mkdir -p "$VAULT/Notes"
cat > "$VAULT/Notes/Hello.md" <<'MD'
# Hello

This is a smoke test note linking to [[World]].
MD
cat > "$VAULT/World.md" <<'MD'
# World

Deep Obsidian integration test content.
MD
echo "vault created at $VAULT"

PORT=27125

step "setup-service writes user config"
deep-obsidian-mcp setup-service --vault "$VAULT" --no-auth --port "$PORT" 2>&1 | tail -20
CONF="$HOME/.config/deep-obsidian-mcp/config.json"
check "config.json created" test -f "$CONF"
[ -f "$CONF" ] && cat "$CONF"

step "Sidecar bundle is discoverable by the binary's own probe (no CouchDB involved)"
# `test -f` above proves the file shipped. It does NOT prove the binary can FIND it —
# the probe walks up from the executable, and a wrong install prefix would leave the
# bundle present and undiscoverable. So ask the binary, through the one check that runs
# the server's own `locate_sidecar_bundle`.
#
# Two things make this a real assertion:
#
#   * A couchdb mount must be DECLARED, because the sidecar checks are per-mount. The
#     config below points at a host that does not resolve and references a secret that
#     does not exist; neither matters, because the bundle and Node checks are local and
#     contact nothing. `doctor` without --probe-remote never opens a connection.
#   * It runs from `/`, NOT from a source checkout. The probe also walks up from the
#     current directory, so running this in the repo (where build-deb.sh has just built
#     dist/sidecar.mjs) would pass via the cwd fallback and prove nothing about the
#     package.
MOUNTS_CONF="$HOME/mounts-config.json"
cat > "$MOUNTS_CONF" <<JSON
{
  "indexDir": "$HOME/.local/share/deep-obsidian-mcp/smoke-index",
  "transport": "stdio",
  "experimental": { "multiVault": true, "couchdbVaults": true },
  "mounts": [
    { "id": "vault", "mountAt": "",
      "backend": { "kind": "filesystem", "vaultPath": "$VAULT" } },
    { "id": "live", "mountAt": "Live",
      "backend": { "kind": "couchdb",
                   "url": "http://couchdb.invalid:5984",
                   "database": "vault",
                   "username": "smoke",
                   "passwordRef": { "kind": "encryptedFile", "id": "smoke-absent-password" } } }
  ]
}
JSON
DOCTOR_JSON=$(cd / && deep-obsidian-mcp --config "$MOUNTS_CONF" doctor --json 2>&1) || true
BUNDLE_CHECK=$(printf '%s\n' "$DOCTOR_JSON" | grep -A1 -F '"name": "mount.live.sidecar-bundle"' || true)
if printf '%s\n' "$BUNDLE_CHECK" | grep -q '"status": "ok"'; then
  echo "PASS: doctor located the packaged sidecar bundle"
else
  echo "FAIL: doctor did not locate the packaged sidecar bundle"
  echo "--- mount checks reported ---"
  printf '%s\n' "$DOCTOR_JSON" | grep -A2 -F '"name": "mount.' || printf '%s\n' "$DOCTOR_JSON" | tail -40
  FAIL=1
fi
# ...and at the packaged path specifically, not some other copy it happened to find.
if printf '%s\n' "$DOCTOR_JSON" | grep -qF "$SIDECAR_BUNDLE"; then
  echo "PASS: the located bundle is the packaged one ($SIDECAR_BUNDLE)"
else
  echo "FAIL: doctor did not report the packaged bundle path $SIDECAR_BUNDLE"
  FAIL=1
fi
# A couchdb mount that cannot start must not make doctor call the install broken: the
# mount is experimental and non-root, and the vault root keeps serving.
if printf '%s\n' "$DOCTOR_JSON" | grep -q '"ok": true'; then
  echo "PASS: doctor reports the install healthy with an unreachable couchdb mount"
else
  echo "WARN: doctor reported ok=false; checks that failed:"
  printf '%s\n' "$DOCTOR_JSON" | grep -B1 -A2 '"status": "fail"' || true
fi

step "HTTP serve smoke test (packaged mode, as the systemd unit runs it)"
export DEEP_OBSIDIAN_PACKAGED=1
/usr/bin/deep-obsidian-mcp serve --packaged --transport http > /tmp/serve.log 2>&1 &
SRV=$!
UP=0
HEALTH=""
for _ in $(seq 1 30); do
  sleep 1
  for hp in healthz health; do
    if curl -fsS "http://127.0.0.1:$PORT/$hp" >/dev/null 2>&1; then UP=1; HEALTH=$hp; break 2; fi
  done
  kill -0 "$SRV" 2>/dev/null || break
done
if [ "$UP" = 1 ]; then
  echo "PASS: server is up on port $PORT"
  echo "--- /$HEALTH ---"
  curl -fsS "http://127.0.0.1:$PORT/$HEALTH"; echo
  echo "--- MCP initialize ---"
  INIT=$(curl -fsS -X POST "http://127.0.0.1:$PORT/mcp" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}')
  echo "$INIT" | head -c 2000; echo
  echo "$INIT" | grep -q '"serverInfo"' && echo "PASS: MCP initialize returned serverInfo" || { echo "FAIL: MCP initialize"; FAIL=1; }
else
  echo "FAIL: server did not come up; serve.log:"
  cat /tmp/serve.log
  FAIL=1
fi
kill "$SRV" 2>/dev/null; wait "$SRV" 2>/dev/null

step "stdio transport smoke test"
STDIO_OUT=$(printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  | timeout 30 deep-obsidian-mcp --vault "$VAULT" --transport stdio 2>/tmp/stdio.err | head -c 2000) || true
echo "$STDIO_OUT"
if echo "$STDIO_OUT" | grep -q '"serverInfo"'; then
  echo "PASS: stdio initialize returned serverInfo"
else
  echo "FAIL: stdio initialize did not return serverInfo; stderr:"
  head -5 /tmp/stdio.err
  FAIL=1
fi

step "RESULT"
if [ "$FAIL" = 0 ]; then echo "SMOKE TEST: ALL PASS"; else echo "SMOKE TEST: FAILURES DETECTED"; fi
exit "$FAIL"
