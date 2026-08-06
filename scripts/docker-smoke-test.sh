#!/usr/bin/env bash
# Parity smoke test for the deep-obsidian-mcp container image.
#
# # Why this file exists next to scripts/linux-smoke-test.sh
#
# The image is a SECOND packaging path beside the .deb, built from sources with its
# own install prefix. The risk that buys is drift: a layout change that keeps the
# .deb working while the container silently loses the sidecar bundle, or an image
# that serves happily with authentication switched off. So this script asserts, on
# the image, the same things linux-smoke-test.sh asserts on the package —
#
#   * the binary runs and ripgrep is on PATH;
#   * the packaged data directories are installed;
#   * `doctor`, run from `/`, LOCATES the sidecar bundle through the binary's own
#     exe-relative probe, at the image's prefix specifically;
#   * `/healthz` answers and MCP `initialize` returns `serverInfo`;
#   * the stdio transport answers too;
#
# — plus the four contract points that only exist in a container: the auth refusal,
# the explicit insecure escape hatch, mounted-config precedence, and an index that
# lands on the volume instead of inside the vault.
#
# Usage: scripts/docker-smoke-test.sh [IMAGE] [--no-couchdb]
#   IMAGE          defaults to deep-obsidian-mcp:ci
#   --no-couchdb   skip the live-CouchDB section (degraded start + self-heal)
#
# Exit code 0 = all checks passed. Leaves nothing behind: every container, volume
# and temp directory it creates is removed on exit, including on failure.
#
# One shell note that cost a debugging round: `pipefail` is on, so no assertion here
# ends a pipeline with `grep -q`. `grep -q` exits on the FIRST match, the upstream
# `docker logs` then dies of SIGPIPE, and `pipefail` reports the pipeline as failed —
# so a check whose pattern was present would fail intermittently, depending on which
# side of the pipe finished first. Every match therefore uses a full-consuming
# `grep -e PATTERN >/dev/null`.
set -uo pipefail

IMAGE=deep-obsidian-mcp:ci
WITH_COUCHDB=1
for arg in "$@"; do
  case "$arg" in
    --no-couchdb) WITH_COUCHDB=0 ;;
    -*) echo "unknown flag: $arg" >&2; exit 2 ;;
    *) IMAGE="$arg" ;;
  esac
done

FAIL=0
# The one path the image's exe-relative probe derives: the exe is
# /opt/deep-obsidian-mcp/bin/deep-obsidian-mcp, so the walk up reaches
# /opt/deep-obsidian-mcp and appends PACKAGED_BUNDLE_PREFIX
# (share/deep-obsidian-mcp) + sidecar/livesync-sidecar/dist/sidecar.mjs. Hard-coded
# on purpose, exactly as in linux-smoke-test.sh: this file's job is to catch the
# packaging and the probe drifting apart.
PREFIX=/opt/deep-obsidian-mcp
BUNDLE="$PREFIX/share/deep-obsidian-mcp/sidecar/livesync-sidecar/dist/sidecar.mjs"
NET=do-smoke-net-$$
PORT=${PORT:-47100}
TMP=$(mktemp -d)
CONTAINERS=()
VOLUMES=()

step() { echo; echo "=== $* ==="; }
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAIL=1; }
check() {
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then pass "$desc"; else fail "$desc (cmd: $*)"; fi
}

cleanup() {
  for c in "${CONTAINERS[@]:-}"; do [ -n "$c" ] && docker rm -f "$c" >/dev/null 2>&1; done
  for v in "${VOLUMES[@]:-}"; do [ -n "$v" ] && docker volume rm -f "$v" >/dev/null 2>&1; done
  docker network rm "$NET" >/dev/null 2>&1
  rm -rf "$TMP"
}
trap cleanup EXIT

# Wait for the HTTP surface of a container published on $PORT.
wait_for_health() {
  local container="$1" tries="${2:-60}"
  for _ in $(seq 1 "$tries"); do
    if curl -fsS "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then return 0; fi
    docker inspect -f '{{.State.Running}}' "$container" 2>/dev/null | grep -e true >/dev/null || return 1
    sleep 1
  done
  return 1
}

mcp_initialize() {
  local token="${1:-}"
  local auth=()
  [ -n "$token" ] && auth=(-H "Authorization: Bearer $token")
  # `${auth[@]+"${auth[@]}"}` rather than `"${auth[@]}"`: under `set -u`, bash 3.2 —
  # which is what /bin/bash still is on macOS — treats an EMPTY array expansion as an
  # unbound variable and aborts the function. The unauthenticated case is exactly the
  # one that would break, so the guard is load-bearing rather than stylistic.
  curl -sS -o "$TMP/init.json" -w '%{http_code}' -X POST "http://127.0.0.1:$PORT/mcp" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    ${auth[@]+"${auth[@]}"} \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}'
}

echo "image: $IMAGE"
docker image inspect "$IMAGE" >/dev/null 2>&1 || { echo "no such image; build it with: docker build -t $IMAGE ."; exit 2; }
docker image inspect "$IMAGE" --format 'size: {{.Size}} bytes  arch: {{.Os}}/{{.Architecture}}  user: {{.Config.User}}'

# ---------------------------------------------------------------------------
step "Binary, dependencies and packaged files (parity with the .deb checks)"
# ---------------------------------------------------------------------------
# `--entrypoint` is overridden here on purpose: these are questions about the IMAGE,
# not about the boot sequence, and the entrypoint would derive a config first.
run_in_image() { docker run --rm --entrypoint /bin/sh "$IMAGE" -c "$*"; }

VERSION_OUT=$(docker run --rm --entrypoint "$PREFIX/bin/deep-obsidian-mcp" "$IMAGE" version 2>&1)
if echo "$VERSION_OUT" | grep -Eq '^[0-9]+\.[0-9]+'; then
  pass "deep-obsidian-mcp version → $VERSION_OUT"
else
  fail "deep-obsidian-mcp version printed '$VERSION_OUT'"
fi
check "deep-obsidian-mcp on PATH" run_in_image 'command -v deep-obsidian-mcp'
check "ripgrep (rg) on PATH" run_in_image 'command -v rg'
check "node on PATH (the couchdb sidecar runtime)" run_in_image 'command -v node'
check "curl on PATH (the HEALTHCHECK's only dependency)" run_in_image 'command -v curl'
check "livesync sidecar bundle installed" run_in_image "test -s $BUNDLE"
check "skills installed" run_in_image "test -f $PREFIX/share/deep-obsidian-mcp/skills/obsidian-capture-session/SKILL.md"
check "obsidian-snippets installed" run_in_image "test -d $PREFIX/share/deep-obsidian-mcp/obsidian-snippets"
check "assets installed" run_in_image "test -d $PREFIX/share/deep-obsidian-mcp/assets"

RUNTIME_USER=$(docker image inspect "$IMAGE" --format '{{.Config.User}}')
if [ "$RUNTIME_USER" = deepobsidian ]; then
  pass "image runs as the non-root user 'deepobsidian'"
else
  fail "image user is '$RUNTIME_USER', expected 'deepobsidian'"
fi
WHOAMI=$(run_in_image 'id -un; id -u' 2>&1 | tr '\n' ' ')
echo "container identity: $WHOAMI"
case "$WHOAMI" in *"deepobsidian 10001"*) pass "processes run as uid 10001" ;; *) fail "unexpected identity: $WHOAMI" ;; esac

if docker image inspect "$IMAGE" --format '{{json .Config.Healthcheck}}' | grep -e healthz >/dev/null; then
  pass "HEALTHCHECK probes /healthz"
else
  fail "HEALTHCHECK does not mention /healthz"
fi

# ---------------------------------------------------------------------------
step "The sidecar bundle is discoverable BY THE BINARY'S OWN PROBE, from /"
# ---------------------------------------------------------------------------
# `test -s` above proves the file shipped; it does NOT prove the binary can find it.
# The probe walks up from the executable AND from the current directory, so this runs
# with `-w /`: from a directory that cannot possibly contain a fallback copy. A
# couchdb mount must be DECLARED because the sidecar checks are per-mount; the host
# below does not resolve and the referenced secret does not exist, neither of which
# matters — `doctor` without --probe-remote contacts nothing.
cat > "$TMP/doctor-config.json" <<JSON
{
  "indexDir": "/tmp/smoke-index",
  "transport": "stdio",
  "experimental": { "multiVault": true, "couchdbVaults": true },
  "mounts": [
    { "id": "vault", "mountAt": "", "backend": { "kind": "filesystem", "vaultPath": "/vault" } },
    { "id": "live", "mountAt": "Live",
      "backend": { "kind": "couchdb", "url": "http://couchdb.invalid:5984", "database": "vault",
                   "username": "smoke",
                   "passwordRef": { "kind": "encryptedFile", "id": "smoke-absent-password" } } }
  ]
}
JSON
# Through the ENTRYPOINT (DO_MOUNTED_CONFIG + a passthrough subcommand), so the same
# path an operator uses for `docker compose run ... doctor` is what gets tested.
DOCTOR_JSON=$(docker run --rm -w / \
  -e DO_MOUNTED_CONFIG=/etc/deep-obsidian/config.json \
  -v "$TMP/doctor-config.json:/etc/deep-obsidian/config.json:ro" \
  "$IMAGE" doctor --json 2>/dev/null)
if printf '%s\n' "$DOCTOR_JSON" | grep -A1 -F '"name": "mount.live.sidecar-bundle"' | grep -e '"status": "ok"' >/dev/null; then
  pass "doctor located the packaged sidecar bundle"
else
  fail "doctor did not locate the packaged sidecar bundle"
  printf '%s\n' "$DOCTOR_JSON" | grep -A2 -F '"name": "mount.' || printf '%s\n' "$DOCTOR_JSON" | tail -30
fi
if printf '%s\n' "$DOCTOR_JSON" | grep -F -e "$BUNDLE" >/dev/null; then
  pass "the located bundle is the image's own ($BUNDLE)"
else
  fail "doctor did not report the bundle at $BUNDLE"
fi
if printf '%s\n' "$DOCTOR_JSON" | grep -A1 -F '"name": "mount.live.sidecar-node"' | grep -e '"status": "ok"' >/dev/null; then
  pass "doctor found the Node runtime the sidecar needs"
else
  fail "doctor did not find a usable Node runtime"
fi
# The same assertion linux-smoke-test.sh makes about the .deb: a couchdb mount that
# cannot be reached must not make doctor call the INSTALL broken. The mount is
# experimental and non-root here, and the vault root keeps serving.
if printf '%s\n' "$DOCTOR_JSON" | grep -e '"ok": true' >/dev/null; then
  pass "doctor reports the install healthy with an unreachable couchdb mount"
else
  fail "doctor reported ok=false with only an unreachable couchdb mount to blame"
  printf '%s\n' "$DOCTOR_JSON" | grep -B1 -A2 '"status": "fail"' || true
fi

# ---------------------------------------------------------------------------
step "Entrypoint contract: no token secret ⇒ refuses to start"
# ---------------------------------------------------------------------------
REFUSAL=$(docker run --rm "$IMAGE" 2>&1); REFUSAL_CODE=$?
if [ "$REFUSAL_CODE" -ne 0 ]; then
  pass "exited non-zero ($REFUSAL_CODE) without a token secret"
else
  fail "started without a bearer token and without DO_INSECURE_NO_AUTH"
fi
if printf '%s\n' "$REFUSAL" | grep -e 'no bearer token' >/dev/null; then
  pass "the refusal names the missing secret"
  printf '%s\n' "$REFUSAL" | grep -m1 'no bearer token'
else
  fail "the refusal did not mention the missing bearer token"
  printf '%s\n' "$REFUSAL" | tail -10
fi
# A present-but-blank file is the same refusal, not an unauthenticated boot.
mkdir -p "$TMP/blank-secrets"; : > "$TMP/blank-secrets/auth_token"
BLANK=$(docker run --rm -v "$TMP/blank-secrets:/run/secrets:ro" "$IMAGE" 2>&1); BLANK_CODE=$?
if [ "$BLANK_CODE" -ne 0 ] && printf '%s\n' "$BLANK" | grep -e 'no bearer token' >/dev/null; then
  pass "an empty token file is refused, not treated as 'no auth wanted'"
else
  fail "an empty token file did not produce the refusal (exit $BLANK_CODE)"
fi

# ---------------------------------------------------------------------------
step "Entrypoint contract: filesystem root, bearer auth, index on the volume"
# ---------------------------------------------------------------------------
mkdir -p "$TMP/secrets" "$TMP/vault/Notes"
TOKEN=smoke-token-$RANDOM$RANDOM
printf '%s\n' "$TOKEN" > "$TMP/secrets/auth_token"
cat > "$TMP/vault/Notes/Hello.md" <<'MD'
# Hello

A smoke test note linking to [[World]].
MD
cat > "$TMP/vault/World.md" <<'MD'
# World

Deep Obsidian container smoke test content.
MD

VOL=do-smoke-state-$$
VOLUMES+=("$VOL")
C1=do-smoke-fs-$$
CONTAINERS+=("$C1")
docker run -d --name "$C1" \
  -p "127.0.0.1:$PORT:4100" \
  -v "$VOL:/var/lib/deep-obsidian-mcp" \
  -v "$TMP/vault:/vault" \
  -v "$TMP/secrets:/run/secrets:ro" \
  "$IMAGE" >/dev/null

if wait_for_health "$C1"; then
  pass "server is up on 127.0.0.1:$PORT"
  echo "--- /healthz ---"; curl -fsS "http://127.0.0.1:$PORT/healthz" | head -c 400; echo
else
  fail "server did not come up"
  docker logs "$C1" 2>&1 | tail -40
fi

CODE=$(mcp_initialize "")
if [ "$CODE" = 401 ]; then
  pass "MCP /mcp without a token → 401"
else
  fail "MCP /mcp without a token returned $CODE (expected 401)"
  head -c 300 "$TMP/init.json"; echo
fi
CODE=$(mcp_initialize "$TOKEN")
if [ "$CODE" = 200 ] && grep -q '"serverInfo"' "$TMP/init.json"; then
  pass "MCP initialize with the bearer token returned serverInfo"
else
  fail "MCP initialize with the token returned $CODE"
  head -c 400 "$TMP/init.json"; echo
fi
# /healthz must stay open: the image's HEALTHCHECK carries no credential.
HEALTH_CODE=$(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/healthz")
[ "$HEALTH_CODE" = 200 ] && pass "/healthz is reachable unauthenticated (as the HEALTHCHECK needs)" \
  || fail "/healthz returned $HEALTH_CODE unauthenticated"

# The index must not land inside the vault: a vault can be a read-only bind mount,
# and an in-vault index is a directory an Obsidian client would sync.
CONF_DUMP=$(docker exec "$C1" deep-obsidian-mcp --config /var/lib/deep-obsidian-mcp/config.json print-config 2>&1)
if printf '%s\n' "$CONF_DUMP" | grep -e '/var/lib/deep-obsidian-mcp/index' >/dev/null; then
  pass "indexDir resolves onto the volume (/var/lib/deep-obsidian-mcp/index)"
else
  fail "indexDir did not resolve onto the volume"
  printf '%s\n' "$CONF_DUMP" | grep -i index
fi
if [ -d "$TMP/vault/.deep-obsidian-mcp" ]; then
  fail "an index directory was created INSIDE the vault ($TMP/vault/.deep-obsidian-mcp)"
else
  pass "nothing was written inside the vault"
fi
# The credential store must not be on the volume.
if docker exec "$C1" sh -c 'test -e /var/lib/deep-obsidian-mcp/secrets.json -o -e /var/lib/deep-obsidian-mcp/config/secrets.json'; then
  fail "a secrets store was found on the volume"
else
  pass "no secret store on the volume (it lives in \$XDG_CONFIG_HOME, which dies with the container)"
fi
docker exec "$C1" sh -c 'ls -la "$XDG_CONFIG_HOME/deep-obsidian-mcp" 2>/dev/null || echo "(no store: this config needs no stored secret)"'

# A restart must not re-derive the config, and must still serve.
CONF_BEFORE=$(docker exec "$C1" cat /var/lib/deep-obsidian-mcp/config.json | tr -d ' \n')
docker restart "$C1" >/dev/null
if wait_for_health "$C1"; then
  CONF_AFTER=$(docker exec "$C1" cat /var/lib/deep-obsidian-mcp/config.json | tr -d ' \n')
  [ "$CONF_BEFORE" = "$CONF_AFTER" ] && pass "restart is idempotent: the config on the volume is unchanged" \
    || fail "the config changed across a restart"
  docker logs "$C1" 2>&1 | grep -e 'already on the volume' >/dev/null \
    && pass "the entrypoint reported reusing the volume's config" \
    || fail "the entrypoint did not report reusing the volume's config"
else
  fail "server did not come back after a restart"
  docker logs "$C1" 2>&1 | tail -30
fi
docker rm -f "$C1" >/dev/null

# ---------------------------------------------------------------------------
step "Entrypoint contract: DO_INSECURE_NO_AUTH=1 serves without a token"
# ---------------------------------------------------------------------------
C2=do-smoke-insecure-$$
CONTAINERS+=("$C2")
docker run -d --name "$C2" \
  -p "127.0.0.1:$PORT:4100" \
  -v "$TMP/vault:/vault" \
  -e DO_INSECURE_NO_AUTH=1 \
  "$IMAGE" >/dev/null
if wait_for_health "$C2"; then
  pass "server is up with DO_INSECURE_NO_AUTH=1"
  CODE=$(mcp_initialize "")
  if [ "$CODE" = 200 ] && grep -q '"serverInfo"' "$TMP/init.json"; then
    pass "MCP initialize with no token returned serverInfo (auth is off, as asked)"
  else
    fail "unauthenticated MCP initialize returned $CODE"
  fi
  docker logs "$C2" 2>&1 | grep -e 'NO authentication' >/dev/null \
    && pass "the log warns loudly that the vault is exposed" \
    || fail "no warning about serving without authentication"
else
  fail "server did not come up with DO_INSECURE_NO_AUTH=1"
  docker logs "$C2" 2>&1 | tail -30
fi
docker rm -f "$C2" >/dev/null

# ---------------------------------------------------------------------------
step "Entrypoint contract: a mounted config WINS and the volume stays untouched"
# ---------------------------------------------------------------------------
# Two assertions, and the second is the crisp one: the mounted file's distinctive
# values are what the server resolves, AND no config was derived onto the volume.
cat > "$TMP/mounted-config.json" <<'JSON'
{
  "indexDir": "/var/lib/deep-obsidian-mcp/mounted-index",
  "transport": "http",
  "http": { "host": "127.0.0.1", "port": 9999, "mcpPath": "/rpc", "healthPath": "/healthz" },
  "experimental": { "multiVault": true },
  "mounts": [
    { "id": "mountedroot", "mountAt": "", "backend": { "kind": "filesystem", "vaultPath": "/vault" } }
  ]
}
JSON
VOL2=do-smoke-state2-$$
VOLUMES+=("$VOL2")
C3=do-smoke-mounted-$$
CONTAINERS+=("$C3")
docker run -d --name "$C3" \
  -p "127.0.0.1:$PORT:4100" \
  -v "$VOL2:/var/lib/deep-obsidian-mcp" \
  -v "$TMP/vault:/vault" \
  -v "$TMP/secrets:/run/secrets:ro" \
  -v "$TMP/mounted-config.json:/etc/deep-obsidian/config.json:ro" \
  -e DO_ROOT_KIND=couchdb \
  -e DO_COUCHDB_URL=http://never.used:5984 \
  -e DO_COUCHDB_DATABASE=never-used \
  -e DO_COUCHDB_USERNAME=never-used \
  "$IMAGE" >/dev/null
if wait_for_health "$C3"; then
  pass "server is up from the mounted config (DO_ROOT_KIND=couchdb was ignored)"
  MOUNTED_DUMP=$(docker exec "$C3" deep-obsidian-mcp --config /etc/deep-obsidian/config.json print-config 2>&1)
  printf '%s\n' "$MOUNTED_DUMP" | grep -e mountedroot >/dev/null \
    && pass "the mounted config's mount id ('mountedroot') is what resolves" \
    || { fail "the mounted config's mount id is not what resolved"; printf '%s\n' "$MOUNTED_DUMP" | head -30; }
  printf '%s\n' "$MOUNTED_DUMP" | grep -e '/rpc' >/dev/null \
    && pass "the mounted config's mcpPath ('/rpc') survived" \
    || fail "the mounted config's mcpPath did not survive"
  # The MCP endpoint really is at /rpc, not /mcp.
  RPC_CODE=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$PORT/rpc" \
    -H 'Content-Type: application/json' -H "Authorization: Bearer $TOKEN" \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"s","version":"0"}}}')
  [ "$RPC_CODE" = 200 ] && pass "MCP answers on the mounted config's /rpc path" \
    || fail "POST /rpc returned $RPC_CODE"
  # ...but host and port are the container's, not the file's 127.0.0.1:9999. That is
  # the documented exception to "the mounted config wins".
  pass "host/port were forced by the container (the file said 127.0.0.1:9999 and the port is reachable from the host)"
  if docker exec "$C3" test -e /var/lib/deep-obsidian-mcp/config.json; then
    fail "a config was derived onto the volume even though one was mounted"
  else
    pass "no config was derived onto the volume"
  fi
else
  fail "server did not come up from the mounted config"
  docker logs "$C3" 2>&1 | tail -40
fi
docker rm -f "$C3" >/dev/null

# ---------------------------------------------------------------------------
step "stdio transport (parity with the .deb check)"
# ---------------------------------------------------------------------------
# No token and no DO_INSECURE_NO_AUTH, on purpose: stdio has no network surface, so
# the entrypoint's auth gate must NOT apply to it. A refusal here would mean the gate
# is placed wrongly (it belongs after the passthrough branch, not before it).
STDIO_OUT=$(printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  | docker run -i --rm -v "$TMP/vault:/vault" "$IMAGE" --transport stdio 2>/dev/null | head -c 2000)
if printf '%s\n' "$STDIO_OUT" | grep -e '"serverInfo"' >/dev/null; then
  pass "stdio initialize returned serverInfo with no token (the auth gate is HTTP-only, as intended)"
else
  fail "stdio initialize did not return serverInfo"
  printf '%s\n' "$STDIO_OUT" | head -c 500; echo
fi

# ---------------------------------------------------------------------------
# `couchdb:3` comes from Docker Hub, which is the one dependency in this script that
# neither the image nor this repository controls: a rate limit or a registry outage
# would otherwise turn an unrelated third-party hiccup into a red build. So a failed
# pull SKIPS this section loudly instead of failing it. Everything above — the whole
# image and entrypoint contract — is self-contained and always runs.
if [ "$WITH_COUCHDB" = 1 ] && ! docker image inspect couchdb:3 >/dev/null 2>&1; then
  if ! docker pull couchdb:3 >/dev/null 2>&1; then
    echo; echo "=== Live CouchDB section SKIPPED: could not pull couchdb:3 (registry unreachable or rate-limited) ==="
    WITH_COUCHDB=0
  fi
fi

if [ "$WITH_COUCHDB" = 1 ]; then
step "Live CouchDB: an env-driven remote root starts DEGRADED and self-heals"
# ---------------------------------------------------------------------------
# The honest flow, without an Obsidian client: a LiveSync database only becomes one
# when a client writes `obsydian_livesync_version` (and the milestone). Until then
# the compatibility gate answers `unknown-schema`, the mount is degraded, /healthz is
# 200 and /readyz is 503. Those two documents are then PUT by hand — the same shape
# `sidecar/livesync-sidecar/test/live-couch.test.mjs` seeds — and the supervisor's
# recovery loop must ready the mount with NO restart.
docker network create "$NET" >/dev/null
CDB=do-smoke-couch-$$
CONTAINERS+=("$CDB")
CDB_USER=smokeadmin
CDB_PASS=smokepass-$RANDOM
CDB_DB=smokevault
docker run -d --name "$CDB" --network "$NET" \
  -e COUCHDB_USER="$CDB_USER" -e COUCHDB_PASSWORD="$CDB_PASS" \
  -p 127.0.0.1:45984:5984 \
  couchdb:3 >/dev/null

COUCH="http://127.0.0.1:45984"
CURL_AUTH=(-u "$CDB_USER:$CDB_PASS")
UP=0
for _ in $(seq 1 60); do
  if curl -fsS "${CURL_AUTH[@]}" "$COUCH/_up" >/dev/null 2>&1; then UP=1; break; fi
  sleep 1
done
if [ "$UP" = 1 ]; then
  pass "couchdb is up and answers /_up"
  # Exactly what docker-compose.example.yml's couchdb-init job does, in the same
  # order — this is the half of the compose flow that a `docker run` can rehearse.
  couch_config() {
    curl -fsS "${CURL_AUTH[@]}" -X PUT "$COUCH/_node/_local/_config/$1/$2" \
      -H 'Content-Type: application/json' -d "$3" >/dev/null
  }
  couch_config chttpd require_valid_user '"true"'
  couch_config chttpd_auth require_valid_user '"true"'
  couch_config httpd WWW-Authenticate '"Basic realm=\"couchdb\""'
  couch_config httpd enable_cors '"true"'
  couch_config chttpd enable_cors '"true"'
  couch_config cors credentials '"true"'
  couch_config cors origins '"app://obsidian.md,capacitor://localhost,http://localhost"'
  couch_config couchdb max_document_size '"50000000"'
  couch_config chttpd max_http_request_size '"4294967296"'
  pass "applied the LiveSync CouchDB settings through the _config API"
  curl -sS "${CURL_AUTH[@]}" -X POST "$COUCH/_cluster_setup" -H 'Content-Type: application/json' \
    -d "{\"action\":\"enable_single_node\",\"bind_address\":\"0.0.0.0\",\"username\":\"$CDB_USER\",\"password\":\"$CDB_PASS\",\"singlenode\":true}" >/dev/null
  curl -sS "${CURL_AUTH[@]}" -X PUT "$COUCH/$CDB_DB" >/dev/null
  # The setting the compose healthcheck's `-u` exists for: prove it really is on, so a
  # future healthcheck that drops the credentials fails here rather than in an
  # operator's `depends_on` hanging forever.
  UNAUTH=$(curl -sS -o /dev/null -w '%{http_code}' "$COUCH/_up")
  [ "$UNAUTH" = 401 ] && pass "require_valid_user is in effect (unauthenticated /_up → 401)" \
    || fail "unauthenticated /_up returned $UNAUTH (expected 401 after require_valid_user)"
  AUTHED=$(curl -sS -o /dev/null -w '%{http_code}' "${CURL_AUTH[@]}" "$COUCH/_up")
  [ "$AUTHED" = 200 ] && pass "an authenticated /_up is still 200 (what the compose healthcheck runs)" \
    || fail "authenticated /_up returned $AUTHED"

  printf '%s\n' "$CDB_PASS" > "$TMP/secrets/couchdb_password"
  VOL3=do-smoke-state3-$$
  VOLUMES+=("$VOL3")
  C4=do-smoke-couchroot-$$
  CONTAINERS+=("$C4")
  docker run -d --name "$C4" --network "$NET" \
    -p "127.0.0.1:$PORT:4100" \
    -v "$VOL3:/var/lib/deep-obsidian-mcp" \
    -v "$TMP/secrets:/run/secrets:ro" \
    -e DO_ROOT_KIND=couchdb \
    -e DO_ROOT_ID=vault \
    -e DO_COUCHDB_URL="http://$CDB:5984" \
    -e DO_COUCHDB_DATABASE="$CDB_DB" \
    -e DO_COUCHDB_USERNAME="$CDB_USER" \
    "$IMAGE" >/dev/null

  if wait_for_health "$C4"; then
    pass "the container booted with a CouchDB ROOT mount and no local vault at all"
    docker logs "$C4" 2>&1 | grep -e 'injecting couchdb_password' >/dev/null \
      && pass "the entrypoint injected couchdb_password into the mount's reference" \
      || fail "the entrypoint did not report injecting couchdb_password"
    READY_CODE=$(curl -sS -o "$TMP/ready.json" -w '%{http_code}' "http://127.0.0.1:$PORT/readyz")
    if [ "$READY_CODE" = 503 ]; then
      pass "/readyz is 503 before any client sync (degraded, as documented)"
      head -c 400 "$TMP/ready.json"; echo
    else
      fail "/readyz returned $READY_CODE before the LiveSync documents existed (expected 503)"
      head -c 600 "$TMP/ready.json"; echo
    fi

    # Make it a LiveSync vault, by hand. Schema version 12 is upstream's VER, pinned
    # by sidecar/livesync-sidecar/test/upstream-constants.test.mjs.
    curl -sS "${CURL_AUTH[@]}" -X PUT "$COUCH/$CDB_DB/obsydian_livesync_version" \
      -H 'Content-Type: application/json' \
      -d '{"type":"versioninfo","version":12}' >/dev/null
    curl -sS "${CURL_AUTH[@]}" -X PUT "$COUCH/$CDB_DB/_local%2Fobsydian_livesync_milestone" \
      -H 'Content-Type: application/json' \
      -d '{"type":"milestoneinfo","created":1700000000000,"locked":false,"accepted_nodes":["smoke-node"],"node_chunk_info":{"smoke-node":{"min":0,"max":2,"current":2}},"node_info":{},"tweak_values":{}}' >/dev/null
    pass "PUT the LiveSync version document and milestone (what a first client sync would create)"

    # Up to four minutes: the sidecar's readiness-recovery loop re-hand-shakes on a
    # backoff capped at 30s (RESTART_BACKOFF_MAX), and readiness additionally needs an
    # index refresh, whose periodic tick is 30s by default. Two of each, with room.
    READIED=0
    for _ in $(seq 1 120); do
      if [ "$(curl -sS -o "$TMP/ready.json" -w '%{http_code}' "http://127.0.0.1:$PORT/readyz")" = 200 ]; then READIED=1; break; fi
      sleep 2
    done
    if [ "$READIED" = 1 ]; then
      pass "/readyz became 200 with NO restart — the self-heal loop readied the mount"
      head -c 400 "$TMP/ready.json"; echo
    else
      fail "/readyz never became 200 after the LiveSync documents appeared"
      head -c 600 "$TMP/ready.json"; echo
      docker logs "$C4" 2>&1 | tail -40
    fi
  else
    fail "the container did not come up against a live CouchDB"
    docker logs "$C4" 2>&1 | tail -40
  fi
else
  fail "couchdb never became healthy"
  docker logs "$CDB" 2>&1 | tail -20
fi
else
  echo; echo "=== Live CouchDB section SKIPPED (--no-couchdb) ==="
fi

step "RESULT"
if [ "$FAIL" = 0 ]; then echo "DOCKER SMOKE TEST: ALL PASS"; else echo "DOCKER SMOKE TEST: FAILURES DETECTED"; fi
exit "$FAIL"
