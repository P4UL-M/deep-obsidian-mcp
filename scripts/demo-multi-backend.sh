#!/usr/bin/env bash
# One command that makes the multi-backend architecture visible end to end.
#
#   scripts/demo-multi-backend.sh
#
# It builds a throwaway world in a temp directory -- a filesystem vault, a mock
# CouchDB (the sidecar's own fixture server), a mock Algolia -- mounts all three
# under ONE namespace, boots the real HTTP server against it, and then drives the
# real MCP tools over JSON-RPC while asserting on what comes back. Every response
# is printed. Nothing is faked: if an assertion fails the script stops there.
#
# Prerequisites: cargo, node >= 20, curl. `jq` is used for pretty-printing when
# present and skipped when not. Docker is NOT required. Nothing outside the temp
# directory is written -- not your config, not your vault, not your keyring.
#
# Flags:
#   --keep       leave the sandbox (and its config, index, logs) on disk
#   --no-build   skip the cargo/npm build steps (they must already be built)
#   -h|--help    this header
#
# -----------------------------------------------------------------------------
# How each backend's secret is provided, and why
# -----------------------------------------------------------------------------
# A mount never carries a plaintext credential: `passwordRef` / `apiKeyRef` are
# `SecretRef`s pointing at the OS keyring or at the encrypted secrets file
# (rust/crates/deep-obsidian-types/src/lib.rs, `enum SecretRef`). There is no
# `env:` variant, and no CLI subcommand that stores one non-interactively -- the
# `setup-service --wizard` path refuses a config that declares a `mounts` table.
#
# So the demo provisions BOTH secrets into the encrypted secrets file itself,
# which the server finds at $XDG_CONFIG_HOME/deep-obsidian-mcp/secrets.json --
# both HOME and XDG_CONFIG_HOME are repointed into the sandbox, so your real
# secrets file is never opened. The file is written by a generated Node helper
# that implements the same XChaCha20-Poly1305 envelope as
# `deep_obsidian_config::secrets::EncryptedFileStore` (a static app key; see
# APP_SECRET_KEY in rust/crates/deep-obsidian-config/src/secrets.rs).
#
# That duplication is checked rather than trusted: step 2 asserts that `doctor`
# does NOT report a missing secret for the couchdb mount, so a future change to
# the envelope fails here with a named reason instead of surfacing as a confusing
# "mount unavailable" three steps later.
#
# Alternative for the Algolia key only: $DEEP_OBSIDIAN_ALGOLIA_API_KEY overrides
# `apiKeyRef` (ALGOLIA_API_KEY_ENV), which is what scripts/try-algolia-mount.sh
# relies on. The demo deliberately does not use it, because the override logs a
# "SHADOWS the key its apiKeyRef points at" warning that would read as a defect
# here, and because one mechanism for both backends is the honest picture.
#
# Neither mock validates credentials, so the values themselves are arbitrary.
# What is being demonstrated is the resolution path, not authentication.
# -----------------------------------------------------------------------------

# Single-quoted JavaScript expressions are handed to the generated Node helper
# verbatim; the `${...}` inside them are JS template placeholders, never shell
# expansions, so SC2016 is exactly wrong here and is disabled file-wide.
# shellcheck disable=SC2016

# No `-E`: with the ERR trap inherited by subshells, an EXPECTED non-zero inside a
# `$(...)` -- the handshake poll below, a `grep` that finds nothing -- would print a
# scary "FAIL step N aborted" from the subshell while the run carried on fine. Without
# it the trap still fires in the main shell, which is where a real abort happens, and
# `set -e` still propagates a failed command substitution in an assignment.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO/target/debug/deep-obsidian-mcp"
MOCK_ALGOLIA_BIN="$REPO/target/debug/examples/mock_algolia"
SIDECAR_DIR="$REPO/sidecar/livesync-sidecar"
SIDECAR_BUNDLE="$SIDECAR_DIR/dist/sidecar.mjs"

KEEP=false
DO_BUILD=true
while [[ $# -gt 0 ]]; do
  case "$1" in
    --keep) KEEP=true; shift ;;
    --no-build) DO_BUILD=false; shift ;;
    -h|--help) sed -n '2,50p' "$0"; exit 0 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

# -----------------------------------------------------------------------------
# Presentation and assertion helpers
# -----------------------------------------------------------------------------

if [[ -t 1 ]]; then BOLD=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[31m'; GREEN=$'\033[32m'; OFF=$'\033[0m'
else BOLD=""; DIM=""; RED=""; GREEN=""; OFF=""; fi

STEP=0
banner() {
  STEP=$((STEP + 1))
  printf '\n%s================================================================================%s\n' "$BOLD" "$OFF"
  printf '%s  STEP %-2d  %s%s\n' "$BOLD" "$STEP" "$1" "$OFF"
  printf '%s================================================================================%s\n' "$BOLD" "$OFF"
  shift
  for line in "$@"; do printf '%s  %s%s\n' "$DIM" "$line" "$OFF"; done
}

say()  { printf '\n%s* %s%s\n' "$BOLD" "$1" "$OFF"; }
note() { printf '%s  %s%s\n' "$DIM" "$1" "$OFF"; }
cmd()  { printf '%s  $ %s%s\n' "$DIM" "$1" "$OFF"; }

# Pretty-print JSON when jq is available, raw otherwise. Never fails the run, and
# never gates an assertion -- everything asserted on is captured in a variable
# first, so a missing jq changes only what the audience reads.
# Set DEMO_NO_JQ=1 to exercise the raw path on a machine that has jq.
HAVE_JQ=false
if [[ -z "${DEMO_NO_JQ:-}" ]] && command -v jq >/dev/null 2>&1; then HAVE_JQ=true; fi
show() {
  if [[ "$HAVE_JQ" == true ]]; then jq "${1:-.}" 2>/dev/null || cat
  else cat; echo
  fi
}

pass() { printf '  %sPASS%s %s\n' "$GREEN" "$OFF" "$1"; }
die()  { printf '\n  %sFAIL%s %s\n' "$RED" "$OFF" "$1" >&2; exit 1; }

assert_eq() { # label expected actual
  [[ "$2" == "$3" ]] || die "$1: expected [$2], got [$3]"
  pass "$1 == $2"
}
assert_contains() { # label needle haystack
  case "$3" in *"$2"*) pass "$1 contains \"$2\"" ;; *) die "$1: \"$2\" not found in: $3" ;; esac
}
assert_not_contains() { # label needle haystack
  case "$3" in *"$2"*) die "$1: unexpected \"$2\" in: $3" ;; *) pass "$1 does not contain \"$2\"" ;; esac
}

trap 'die "step $STEP aborted at line $LINENO (command: $BASH_COMMAND)"' ERR

# -----------------------------------------------------------------------------
# Sandbox and cleanup. Every child is tracked; EXIT kills all of them.
# -----------------------------------------------------------------------------

WORK="$(mktemp -d "${TMPDIR:-/tmp}/deep-obsidian-demo.XXXXXX")"
PIDS=()
COUCH_STDIN_OPEN=false

cleanup() {
  local status=$?
  set +e
  trap - ERR
  # Closing the fixture server's stdin is its documented stop signal; the kills
  # below are the backstop, because `MockCouch.close()` waits on keep-alive
  # sockets and can outlive its own EOF.
  if [[ "$COUCH_STDIN_OPEN" == true ]]; then exec 9>&-; COUCH_STDIN_OPEN=false; fi
  local pid
  for pid in ${PIDS[@]+"${PIDS[@]}"}; do kill "$pid" 2>/dev/null; done
  for pid in ${PIDS[@]+"${PIDS[@]}"}; do
    for _ in 1 2 3 4 5 6 7 8 9 10; do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
    kill -9 "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
  done
  if [[ "$KEEP" == true ]]; then
    printf '\n%s  sandbox kept at %s%s\n' "$DIM" "$WORK" "$OFF"
  else
    rm -rf "$WORK"
  fi
  exit "$status"
}
trap cleanup EXIT INT TERM

# -----------------------------------------------------------------------------
# STEP 1 -- build
# -----------------------------------------------------------------------------

banner "BUILD" \
  "The debug binary, the mock-Algolia example, and the LiveSync sidecar bundle." \
  "Quiet on success; a build failure stops the demo here."

if [[ "$DO_BUILD" == true ]]; then
  cmd "cargo build -q -p deep-obsidian-cli"
  (cd "$REPO" && cargo build -q -p deep-obsidian-cli)
  cmd "cargo build -q -p deep-obsidian-algolia --example mock_algolia"
  (cd "$REPO" && cargo build -q -p deep-obsidian-algolia --example mock_algolia)
  cmd "npm run --silent build   (sidecar/livesync-sidecar)"
  (cd "$SIDECAR_DIR" && npm run --silent build >/dev/null)
else
  note "--no-build: using whatever is already in target/ and dist/"
fi

for artefact in "$BIN" "$MOCK_ALGOLIA_BIN" "$SIDECAR_BUNDLE"; do
  [[ -s "$artefact" ]] || die "missing build artefact: $artefact"
done
pass "binary, mock-Algolia example and sidecar bundle are present"
"$BIN" version | head -1

# -----------------------------------------------------------------------------
# The generated Node helper. Node is already a hard prerequisite (the sidecar and
# the CouchDB fixture are Node), so using it for port probing, JSON extraction and
# secret provisioning keeps the dependency list at cargo + node + curl.
# -----------------------------------------------------------------------------

TOOLS="$WORK/demo-tools.mjs"
cat > "$TOOLS" <<'MJS'
import crypto from "node:crypto";
import fs from "node:fs";
import net from "node:net";
import path from "node:path";

// Same static key as deep_obsidian_config::secrets::APP_SECRET_KEY.
const APP_KEY = Buffer.from("DeepObsidianMCP-static-key-v001!", "utf8");
const ROT = (v, n) => ((v << n) | (v >>> (32 - n))) >>> 0;
const qr = (s, a, b, c, d) => {
    s[a] = (s[a] + s[b]) >>> 0; s[d] = ROT(s[d] ^ s[a], 16);
    s[c] = (s[c] + s[d]) >>> 0; s[b] = ROT(s[b] ^ s[c], 12);
    s[a] = (s[a] + s[b]) >>> 0; s[d] = ROT(s[d] ^ s[a], 8);
    s[c] = (s[c] + s[d]) >>> 0; s[b] = ROT(s[b] ^ s[c], 7);
};
/** HChaCha20: the subkey derivation that turns ChaCha20-Poly1305 into XChaCha20-Poly1305. */
function hchacha20(key, nonce16) {
    const s = new Uint32Array(16);
    s[0] = 0x61707865; s[1] = 0x3320646e; s[2] = 0x79622d32; s[3] = 0x6b206574;
    for (let i = 0; i < 8; i += 1) s[4 + i] = key.readUInt32LE(i * 4);
    for (let i = 0; i < 4; i += 1) s[12 + i] = nonce16.readUInt32LE(i * 4);
    for (let i = 0; i < 10; i += 1) {
        qr(s, 0, 4, 8, 12); qr(s, 1, 5, 9, 13); qr(s, 2, 6, 10, 14); qr(s, 3, 7, 11, 15);
        qr(s, 0, 5, 10, 15); qr(s, 1, 6, 11, 12); qr(s, 2, 7, 8, 13); qr(s, 3, 4, 9, 14);
    }
    const out = Buffer.alloc(32);
    for (let i = 0; i < 4; i += 1) out.writeUInt32LE(s[i], i * 4);
    for (let i = 0; i < 4; i += 1) out.writeUInt32LE(s[12 + i], 16 + i * 4);
    return out;
}

const readStdin = async () => {
    const chunks = [];
    for await (const chunk of process.stdin) chunks.push(chunk);
    return Buffer.concat(chunks).toString("utf8");
};

const [command, ...rest] = process.argv.slice(2);

switch (command) {
    // A port that is free right now, so parallel runs of this demo cannot collide.
    case "free-port": {
        const server = net.createServer();
        await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
        process.stdout.write(String(server.address().port));
        await new Promise((resolve) => server.close(resolve));
        break;
    }
    // put-secret <secrets.json> <id> <value>
    case "put-secret": {
        const [file, id, value] = rest;
        const nonce = crypto.randomBytes(24);
        const subkey = hchacha20(APP_KEY, nonce.subarray(0, 16));
        const nonce12 = Buffer.concat([Buffer.alloc(4), nonce.subarray(16, 24)]);
        const cipher = crypto.createCipheriv("chacha20-poly1305", subkey, nonce12, { authTagLength: 16 });
        const sealed = Buffer.concat([cipher.update(Buffer.from(value, "utf8")), cipher.final(), cipher.getAuthTag()]);
        let doc = { version: 1, cipher: "xchacha20poly1305", items: {} };
        if (fs.existsSync(file)) doc = JSON.parse(fs.readFileSync(file, "utf8"));
        doc.items[id] = { nonce: nonce.toString("base64"), ciphertext: sealed.toString("base64") };
        fs.mkdirSync(path.dirname(file), { recursive: true });
        fs.writeFileSync(file, `${JSON.stringify(doc, null, 2)}\n`, { mode: 0o600 });
        break;
    }
    // handshake <file> <field>: the fixture server's stdout line 1, once it is complete.
    case "handshake": {
        const [file, field] = rest;
        const first = fs.readFileSync(file, "utf8").split("\n")[0];
        const value = JSON.parse(first)[field];
        if (value === undefined) process.exit(1);
        process.stdout.write(String(value));
        break;
    }
    // pick <expr>: evaluate <expr> over stdin JSON bound to `d`. Objects are JSON, scalars raw.
    case "pick": {
        const data = JSON.parse(await readStdin());
        // eslint-disable-next-line no-new-func
        const value = new Function("d", `return (${rest[0]});`)(data);
        process.stdout.write(typeof value === "object" && value !== null ? JSON.stringify(value) : String(value));
        break;
    }
    default:
        process.stderr.write(`unknown helper command: ${command}\n`);
        process.exit(2);
}
MJS

pick() { node "$TOOLS" pick "$1"; }
free_port() { node "$TOOLS" free-port; }

# -----------------------------------------------------------------------------
# STEP 2 -- assemble the world
# -----------------------------------------------------------------------------

banner "ASSEMBLE THE WORLD" \
  "A temp HOME, a filesystem vault, a mock Algolia, a writable mock CouchDB," \
  "and one config that mounts all three under a single namespace."

export HOME="$WORK/home"
export XDG_CONFIG_HOME="$WORK/home/.config"
export XDG_DATA_HOME="$WORK/home/.local/share"
# Anything inherited from the caller's shell would silently change what is being
# demonstrated, so the relevant variables are cleared rather than trusted.
unset DEEP_OBSIDIAN_CONFIG DEEP_OBSIDIAN_AUTH_TOKEN DEEP_OBSIDIAN_ALGOLIA_API_KEY \
      DEEP_OBSIDIAN_LIVESYNC_SIDECAR DEEP_OBSIDIAN_ALLOW_INSECURE DEEP_OBSIDIAN_PACKAGED || true

VAULT="$WORK/vault"
CONFIG="$WORK/config.json"
SECRETS="$XDG_CONFIG_HOME/deep-obsidian-mcp/secrets.json"
mkdir -p "$VAULT/Projects" "$VAULT/Attachments" "$WORK/index" "$XDG_CONFIG_HOME/deep-obsidian-mcp"

say "A filesystem vault: 3 notes, wiki-linked"
cat > "$VAULT/Charter.md" <<'MD'
# Charter

One namespace, several backends. That is the whole federation charter.
See [[Projects/Roadmap]] for milestones and [[Glossary]] for the vocabulary.
MD
cat > "$VAULT/Glossary.md" <<'MD'
# Glossary

- **federation**: recall that fans out across every mount and merges the results.
- **mount**: one backend grafted onto a path prefix. See [[Charter]].
MD
cat > "$VAULT/Projects/Roadmap.md" <<'MD'
# Roadmap

Federation milestones: routing, then guarded writes, then honest degradation.
Back to [[Charter]].
MD
find "$VAULT" -name '*.md' | sed "s|$WORK/|  |" | sort

say "Mock Algolia (the deep-obsidian-algolia crate's own mock, on a free port)"
ALGOLIA_PORT="$(free_port)"
ALGOLIA_URL="http://127.0.0.1:$ALGOLIA_PORT"
ALGOLIA_INDEX="demo_wiki"
start_algolia() {
  "$MOCK_ALGOLIA_BIN" "$ALGOLIA_PORT" >> "$WORK/mock-algolia.log" 2>&1 &
  ALGOLIA_PID=$!
  PIDS+=("$ALGOLIA_PID")
  for _ in $(seq 1 100); do
    curl -sS -o /dev/null "$ALGOLIA_URL/1/indexes/$ALGOLIA_INDEX/settings" 2>/dev/null && return 0
    kill -0 "$ALGOLIA_PID" 2>/dev/null || break
    sleep 0.1
  done
  die "mock Algolia did not come up on $ALGOLIA_PORT; see $WORK/mock-algolia.log"
}
cmd "$MOCK_ALGOLIA_BIN $ALGOLIA_PORT"
start_algolia
pass "mock Algolia serving $ALGOLIA_URL (pid $ALGOLIA_PID)"

say "Mock CouchDB: the sidecar's OWN fixture server, writable"
note "Same fixture the sidecar and Rust suites use, so the demo cannot drift from them."
note "It picks an ephemeral port and announces it on stdout line 1; it exits when stdin closes,"
note "so the demo holds a fifo open on its stdin for the whole run."
mkfifo "$WORK/couch-stdin"
cmd "node test/mock-couch-server.mjs --vault small --writable"
(cd "$SIDECAR_DIR" && exec node test/mock-couch-server.mjs --vault small --writable) \
  < "$WORK/couch-stdin" > "$WORK/couch-stdout" 2> "$WORK/couch-stderr" &
COUCH_PID=$!
PIDS+=("$COUCH_PID")
exec 9> "$WORK/couch-stdin"
COUCH_STDIN_OPEN=true
COUCH_URL=""
# The fixture announces its ephemeral port on stdout line 1. Polling until that line
# is COMPLETE (i.e. parses) is the only correct wait: the file exists immediately.
for _ in $(seq 1 100); do
  COUCH_URL="$(node "$TOOLS" handshake "$WORK/couch-stdout" url 2>/dev/null || true)"
  if [[ -n "$COUCH_URL" ]]; then break; fi
  kill -0 "$COUCH_PID" 2>/dev/null || break
  sleep 0.1
done
[[ -n "$COUCH_URL" ]] || { cat "$WORK/couch-stderr" >&2; die "no handshake from the CouchDB fixture"; }
COUCH_DB="$(node "$TOOLS" handshake "$WORK/couch-stdout" database)"
pass "mock CouchDB serving $COUCH_URL/$COUCH_DB (pid $COUCH_PID)"
note "The fixture ships a small LiveSync vault: Notes/Alpha.md, Beta.md, Legacy.md,"
note "a soft-deleted Removed.md, a CONFLICTED note, and a binary assets/logo.png."

say "Secrets: two SecretRefs, both resolved from the sandbox's encrypted secrets file"
# The VALUE of each secret is deliberately different from its ID, so the "no secret
# value reaches the config" assertions below have something real to look for.
COUCH_PASSWORD="demo-couch-password-value"
ALGOLIA_KEY="demo-algolia-key-value"
node "$TOOLS" put-secret "$SECRETS" demo-couchdb-password "$COUCH_PASSWORD"
node "$TOOLS" put-secret "$SECRETS" demo-algolia-key "$ALGOLIA_KEY"
cmd "cat \$XDG_CONFIG_HOME/deep-obsidian-mcp/secrets.json"
cat "$SECRETS"
note "Ciphertext only. The config below stores references to these ids, never values."

say "The config: three mounts, both experimental gates on"
HTTP_PORT="$(free_port)"
cat > "$CONFIG" <<JSON
{
  "indexDir": "$WORK/index",
  "transport": "http",
  "http": { "host": "127.0.0.1", "port": $HTTP_PORT, "mcpPath": "/mcp", "healthPath": "/healthz" },
  "autoReindex": { "enabled": false },
  "experimental": { "multiVault": true, "couchdbVaults": true, "algoliaVaults": true },
  "mounts": [
    {
      "id": "vault",
      "mountAt": "",
      "backend": { "kind": "filesystem", "vaultPath": "$VAULT" }
    },
    {
      "id": "team",
      "mountAt": "Team",
      "backend": {
        "kind": "couchdb",
        "url": "$COUCH_URL",
        "database": "$COUCH_DB",
        "username": "demo",
        "passwordRef": { "kind": "encryptedFile", "id": "demo-couchdb-password" },
        "sidecarPath": "$SIDECAR_BUNDLE",
        "writable": true
      }
    },
    {
      "id": "wiki",
      "mountAt": "Wiki",
      "backend": {
        "kind": "algolia",
        "appId": "DEMOAPP",
        "indexName": "$ALGOLIA_INDEX",
        "apiKeyRef": { "kind": "encryptedFile", "id": "demo-algolia-key" },
        "baseUrl": "$ALGOLIA_URL",
        "participantId": "demo@localhost",
        "writable": true
      }
    }
  ]
}
JSON
note "autoReindex is off on purpose: every refresh in this demo is an explicit build_index,"
note "so nothing here depends on a timer firing."

say "print-config: what the service will actually run"
cmd "deep-obsidian-mcp --config <sandbox>/config.json print-config"
PRINTED="$("$BIN" --config "$CONFIG" print-config)"
printf '%s\n' "$PRINTED" | show
assert_contains "print-config keeps the references" '"kind": "encryptedFile"' "$PRINTED"
assert_not_contains "no CouchDB password value in print-config" "$COUCH_PASSWORD" "$PRINTED"
assert_not_contains "no Algolia key value in print-config" "$ALGOLIA_KEY" "$PRINTED"
note "Secret VALUES are absent -- only the ids of the references appear. redact_config is"
note "an identity function precisely because a persisted config has nothing secret in it."

say "doctor: local checks plus a read-only probe of both remotes"
cmd "deep-obsidian-mcp --config <sandbox>/config.json doctor --probe-remote"
DOCTOR_JSON="$("$BIN" --config "$CONFIG" doctor --json --probe-remote 2>/dev/null)"
printf '%s\n' "$DOCTOR_JSON" | pick 'd.checks.filter(c => c.name.startsWith("mount.")).map(c => `[${c.status}] ${c.name}: ${c.message}`).join("\n")'
echo
COUCH_PROBE="$(printf '%s\n' "$DOCTOR_JSON" | pick 'd.checks.find(c => c.name === "mount.team.remote").message')"
# The gate promised in the header: if the encrypted-secret envelope ever changes,
# fail HERE, by name, instead of leaving a mysteriously unavailable mount later.
assert_not_contains "couchdb secret resolved (see APP_SECRET_KEY in secrets.rs if this fails)" \
  "missing secret" "$COUCH_PROBE"
assert_contains "couchdb handshake" "compatibility: ok" "$COUCH_PROBE"
assert_contains "algolia probe" "reachable=true" \
  "$(printf '%s\n' "$DOCTOR_JSON" | pick 'd.checks.find(c => c.name === "mount.wiki.remote").message')"

# -----------------------------------------------------------------------------
# STEP 3 -- boot
# -----------------------------------------------------------------------------

banner "BOOT" \
  "The real server, HTTP transport, on a free loopback port." \
  "Auth is off because a fresh config on 127.0.0.1 declares none -- no flag needed."

MCP_URL="http://127.0.0.1:$HTTP_PORT/mcp"
cmd "deep-obsidian-mcp --config <sandbox>/config.json serve"
"$BIN" --config "$CONFIG" serve > "$WORK/server.log" 2>&1 &
SERVER_PID=$!
PIDS+=("$SERVER_PID")
for _ in $(seq 1 150); do
  curl -fsS "http://127.0.0.1:$HTTP_PORT/healthz" >/dev/null 2>&1 && break
  kill -0 "$SERVER_PID" 2>/dev/null || { cat "$WORK/server.log" >&2; die "the server exited during startup"; }
  sleep 0.2
done
curl -fsS "http://127.0.0.1:$HTTP_PORT/healthz" >/dev/null 2>&1 || { cat "$WORK/server.log" >&2; die "no /healthz"; }
pass "server up (pid $SERVER_PID) at $MCP_URL"
sed -n '1,4p' "$WORK/server.log"

RPC_ID=0
# One MCP tool call. The response is plain JSON on 200 even for a refusal, so the
# caller reads `.result.structuredContent` or `.error.message` -- never the status.
mcp() {
  RPC_ID=$((RPC_ID + 1))
  curl -sS -X POST "$MCP_URL" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":$RPC_ID,\"method\":\"tools/call\",\"params\":{\"name\":\"$1\",\"arguments\":$2}}"
}

say "vault_info -> the mount table the agent sees"
cmd "tools/call vault_info"
INFO="$(mcp vault_info '{}')"
printf '%s\n' "$INFO" | pick 'JSON.stringify(d.result.structuredContent.mounts, null, 2)' | show
echo
assert_eq "mount count" 3 "$(printf '%s\n' "$INFO" | pick 'd.result.structuredContent.mounts.length')"
MOUNT_KINDS="$(printf '%s\n' "$INFO" | pick 'd.result.structuredContent.mounts.map(m => `${m.id}=${m.backendKind}`).join(" ")')"
assert_eq "backend kinds" "vault=filesystem team=couchdb wiki=algolia" "$MOUNT_KINDS"

say "What the capabilities say, and what they refuse to promise"
printf '%s\n' "$INFO" | pick 'd.result.structuredContent.mounts.map(m => `  ${m.id.padEnd(6)} ${m.backendKind.padEnd(11)} ${m.capabilities.join(", ")}`).join("\n")'
echo
WIKI_CAPS="$(printf '%s\n' "$INFO" | pick 'd.result.structuredContent.mounts.find(m => m.id === "wiki").capabilities.join(",")')"
TEAM_CAPS="$(printf '%s\n' "$INFO" | pick 'd.result.structuredContent.mounts.find(m => m.id === "team").capabilities.join(",")')"
assert_not_contains "algolia mount" "binary-read" "$WIKI_CAPS"
assert_not_contains "algolia mount" "binary-write" "$WIKI_CAPS"
assert_not_contains "algolia mount" "upload" "$WIKI_CAPS"
assert_contains "algolia mount" "native-recall" "$WIKI_CAPS"
assert_contains "couchdb mount" "watch" "$TEAM_CAPS"
note "The Algolia mount advertises no binary capability at all, and no local index"
note "(indexStatus: none) -- its backend does its own recall. The CouchDB mount"
note "advertises 'watch', so its changes feed drives incremental refresh."
note "It also reports the fixture's pre-existing conflict, unprompted:"
printf '%s\n' "$INFO" | pick 'JSON.stringify(d.result.structuredContent.mounts.find(m => m.id === "team").conflictedPaths)'
echo

# -----------------------------------------------------------------------------
# STEP 4 -- routing
# -----------------------------------------------------------------------------

banner "ROUTING" \
  "One namespace, three backends. The path decides where bytes land," \
  "and the demo proves it by looking INSIDE each backend afterwards."

STANDUP_BODY='# Standup\n\nFederation status for the team. Owner: demo.\n'
ARCH_BODY='# Architecture\n\nThe federation router resolves a path by longest-prefix mount match.\n'
RECALL_BODY='# Recall\n\nFederation recall: native for Algolia, local index for the rest.\n'

say "upsert_note Team/Standup.md  ->  the CouchDB mount, through the real sidecar"
cmd "tools/call upsert_note {path: Team/Standup.md}"
mcp upsert_note "{\"path\":\"Team/Standup.md\",\"content\":\"$STANDUP_BODY\"}" | show '.result.structuredContent'

say "Proof, straight out of CouchDB -- not through the server"
note "LiveSync stores a note as an ENTRY document (id = lower-cased path) plus one"
note "LEAF document per chunk. Both must exist for the write to be real."
cmd "curl $COUCH_URL/$COUCH_DB/standup.md"
ENTRY="$(curl -fsS "$COUCH_URL/$COUCH_DB/standup.md")"
printf '%s\n' "$ENTRY" | show
LEAF_ID="$(printf '%s\n' "$ENTRY" | pick 'd.children[0]')"
assert_contains "entry path" "Standup.md" "$(printf '%s\n' "$ENTRY" | pick 'd.path')"
cmd "curl $COUCH_URL/$COUCH_DB/$LEAF_ID"
LEAF="$(curl -fsS "$COUCH_URL/$COUCH_DB/$LEAF_ID")"
printf '%s\n' "$LEAF" | show
assert_contains "leaf document" "Federation status for the team" "$(printf '%s\n' "$LEAF" | pick 'd.data')"

say "upsert_note Wiki/Architecture.md  ->  the Algolia mount"
cmd "tools/call upsert_note {path: Wiki/Architecture.md}"
mcp upsert_note "{\"path\":\"Wiki/Architecture.md\",\"content\":\"$ARCH_BODY\"}" | show '.result.structuredContent'
mcp upsert_note "{\"path\":\"Wiki/Recall.md\",\"content\":\"$RECALL_BODY\"}" > /dev/null

say "Proof, straight out of the Algolia index -- not through the server"
note "A note is one small 'note' record plus one 'chunk' record per chunk of the"
note "current version. Reads reassemble the body from the chunks."
cmd "curl -X POST $ALGOLIA_URL/1/indexes/$ALGOLIA_INDEX/browse -d '{}'"
BROWSE="$(curl -fsS -X POST "$ALGOLIA_URL/1/indexes/$ALGOLIA_INDEX/browse" -H 'Content-Type: application/json' -d '{}')"
printf '%s\n' "$BROWSE" | pick 'JSON.stringify(d.hits.map(h => ({objectID: h.objectID, recordType: h.recordType, path: h.path, versionId: h.versionId})), null, 2)' | show
echo
assert_contains "algolia records" "note:Architecture.md" "$(printf '%s\n' "$BROWSE" | pick 'd.hits.map(h => h.objectID).join(" ")')"

say "read_file both back, through the one namespace"
for path in Team/Standup.md Wiki/Architecture.md; do
  cmd "tools/call read_file {path: $path}"
  mcp read_file "{\"path\":\"$path\"}" | show '.result.structuredContent'
done
STANDUP_READ="$(mcp read_file '{"path":"Team/Standup.md"}' | pick 'd.result.structuredContent.text')"
assert_eq "Team/Standup.md round-trips byte-identical" \
  "$(printf '%b' "$STANDUP_BODY")" "$STANDUP_READ"
ARCH_READ="$(mcp read_file '{"path":"Wiki/Architecture.md"}' | pick 'd.result.structuredContent.text')"
assert_eq "Wiki/Architecture.md round-trips byte-identical" \
  "$(printf '%b' "$ARCH_BODY")" "$ARCH_READ"

say "list_children at the root: synthesized mount folders beside physical ones"
cmd "tools/call list_children {path: \"\"}"
CHILDREN="$(mcp list_children '{"path":""}')"
printf '%s\n' "$CHILDREN" | show '.result.structuredContent'
KIDS="$(printf '%s\n' "$CHILDREN" | pick 'd.result.structuredContent.children.map(c => `${c.name}(${c.kind})`).join(" ")')"
assert_contains "root listing" "Team(directory)" "$KIDS"
assert_contains "root listing" "Wiki(directory)" "$KIDS"
assert_contains "root listing" "Projects(directory)" "$KIDS"
note "Team/ and Wiki/ are synthesized by the router and carry NO marker distinguishing"
note "them from Projects/, which is a real directory on disk. That is deliberate: an"
note "agent should not need to know which backend a folder came from."

# -----------------------------------------------------------------------------
# STEP 5 -- guarded writes
# -----------------------------------------------------------------------------

banner "GUARDED WRITES" \
  "expectedHash is a compare-and-set precondition, on every backend." \
  "A stale hash is refused with both hashes named; the fresh one succeeds."

say "Read the current hash"
FRESH_HASH="$(mcp read_file '{"path":"Wiki/Architecture.md","includeText":false}' | pick 'd.result.structuredContent.hash')"
note "current hash: $FRESH_HASH"

say "Write with a STALE expectedHash"
cmd "tools/call upsert_note {expectedHash: fnv1a64:0000000000000000}"
STALE="$(mcp upsert_note "{\"path\":\"Wiki/Architecture.md\",\"content\":\"lost update\",\"expectedHash\":\"fnv1a64:0000000000000000\"}")"
printf '%s\n' "$STALE" | show
STALE_MSG="$(printf '%s\n' "$STALE" | pick 'd.error.message')"
assert_contains "refusal" "hash conflict for Wiki/Architecture.md" "$STALE_MSG"
assert_contains "refusal names the hash actually found" "$FRESH_HASH" "$STALE_MSG"

say "The same write with the FRESH hash"
cmd "tools/call upsert_note {expectedHash: $FRESH_HASH}"
GUARDED="$(mcp upsert_note "{\"path\":\"Wiki/Architecture.md\",\"content\":\"$ARCH_BODY\",\"expectedHash\":\"$FRESH_HASH\"}")"
printf '%s\n' "$GUARDED" | show '.result.structuredContent'
assert_eq "guarded write" "updated" "$(printf '%s\n' "$GUARDED" | pick 'd.result.structuredContent.action')"
assert_eq "previousHash is the hash we asserted" "$FRESH_HASH" \
  "$(printf '%s\n' "$GUARDED" | pick 'd.result.structuredContent.previousHash')"
note "The refused write changed nothing: the Algolia mount rejected it before"
note "appending a version, so no lost update and no orphan history entry."

# -----------------------------------------------------------------------------
# STEP 6 -- federated recall
# -----------------------------------------------------------------------------

banner "FEDERATED RECALL" \
  "One unscoped hybrid_search fans out to every mount and merges by weighted RRF." \
  "Each hit says which mount it came from; each mount says how it was asked."

say "build_index -- explicit, because autoReindex is off in this config"
cmd "tools/call build_index"
mcp build_index '{}' | pick 'JSON.stringify({noteCount: d.result.structuredContent.noteCount, perMount: (d.result.structuredContent.mounts||[]).map(m => ({id: m.id, noteCount: m.noteCount}))}, null, 2)' | show
echo

say 'hybrid_search "federation", unscoped'
cmd 'tools/call hybrid_search {query: "federation"}'
FED="$(mcp hybrid_search '{"query":"federation","limit":10}')"
printf '%s\n' "$FED" | pick 'JSON.stringify({
  federated: d.result.structuredContent.federated,
  degraded: d.result.structuredContent.degraded,
  rerank: d.result.structuredContent.rerank,
  count: d.result.structuredContent.count,
  matches: d.result.structuredContent.matches.map(m => ({path: m.path, mountId: m.mountId, score: m.score})),
  mounts: d.result.structuredContent.mounts
}, null, 2)' | show
echo
assert_eq "federated" "true" "$(printf '%s\n' "$FED" | pick 'd.result.structuredContent.federated')"
assert_eq "degraded" "false" "$(printf '%s\n' "$FED" | pick 'd.result.structuredContent.degraded')"
assert_eq "mounts that contributed hits" "team,vault,wiki" \
  "$(printf '%s\n' "$FED" | pick '[...new Set(d.result.structuredContent.matches.map(m => m.mountId))].sort().join(",")')"
assert_eq "the Algolia mount was asked natively" "native-recall" \
  "$(printf '%s\n' "$FED" | pick 'd.result.structuredContent.mounts.find(m => m.id === "wiki").source')"
assert_eq "and it answered lexically" "lexical" \
  "$(printf '%s\n' "$FED" | pick 'd.result.structuredContent.mounts.find(m => m.id === "wiki").recallMode')"
assert_eq "the local mounts were asked through their own index" "local-index,local-index" \
  "$(printf '%s\n' "$FED" | pick 'd.result.structuredContent.mounts.filter(m => m.id !== "wiki").map(m => m.source).join(",")')"
assert_eq "final rerank" "none" "$(printf '%s\n' "$FED" | pick 'd.result.structuredContent.rerank')"
note "rerank: none and recallMode: lexical are the same honesty, twice: no embedding"
note "backend is configured, so nothing pretends to be semantic. Configure one and"
note "these become semantic+lexical and neural, on exactly the same call."

# -----------------------------------------------------------------------------
# STEP 7 -- honest degradation
# -----------------------------------------------------------------------------

banner "HONEST DEGRADATION" \
  "A backend goes away mid-session. Recall must not silently shrink." \
  "Observed semantics are scripted here -- including what recovery really costs."

say "First, an operator snapshot of the shared corpus (this is also step 10's tool)"
cmd "deep-obsidian-mcp algolia dump --mount wiki --out <sandbox>/snapshots/wiki"
"$BIN" --config "$CONFIG" algolia dump --mount wiki --out "$WORK/snapshots/wiki"

say "Now kill the Algolia mock"
cmd "kill $ALGOLIA_PID"
kill "$ALGOLIA_PID" 2>/dev/null || true
for _ in $(seq 1 50); do kill -0 "$ALGOLIA_PID" 2>/dev/null || break; sleep 0.1; done
pass "mock Algolia is gone"

say "The same search again"
DEGRADED="$(mcp hybrid_search '{"query":"federation","limit":10}')"
printf '%s\n' "$DEGRADED" | pick 'JSON.stringify({
  degraded: d.result.structuredContent.degraded,
  missingBackends: d.result.structuredContent.missingBackends,
  degradationReason: d.result.structuredContent.degradationReason,
  count: d.result.structuredContent.count,
  matches: d.result.structuredContent.matches.map(m => ({path: m.path, mountId: m.mountId})),
  wiki: d.result.structuredContent.mounts.find(m => m.id === "wiki")
}, null, 2)' | show
echo
assert_eq "degraded" "true" "$(printf '%s\n' "$DEGRADED" | pick 'd.result.structuredContent.degraded')"
assert_eq "missingBackends" '["wiki"]' "$(printf '%s\n' "$DEGRADED" | pick 'd.result.structuredContent.missingBackends')"
assert_contains "degradationReason names the mount and what failed" "mount 'wiki' could not be searched" \
  "$(printf '%s\n' "$DEGRADED" | pick 'd.result.structuredContent.degradationReason')"
assert_eq "hits from the surviving mounts are intact" "team,vault" \
  "$(printf '%s\n' "$DEGRADED" | pick '[...new Set(d.result.structuredContent.matches.map(m => m.mountId))].sort().join(",")')"
note "The result set shrank and SAID SO. An agent reading this knows its recall was"
note "partial; nothing here looks like 'no such note'."

say "A read through the missing mount fails loudly rather than returning nothing"
cmd "tools/call read_file {path: Wiki/Architecture.md}"
DEAD_READ="$(mcp read_file '{"path":"Wiki/Architecture.md"}')"
printf '%s\n' "$DEAD_READ" | show
assert_contains "read refusal" "algolia mount error" "$(printf '%s\n' "$DEAD_READ" | pick 'd.error.message')"

say "Bring the backend back"
start_algolia
pass "mock Algolia serving again on the same port (pid $ALGOLIA_PID)"
RECOVERED="$(mcp hybrid_search '{"query":"federation","limit":10}')"
assert_eq "degraded, on the very next call" "false" "$(printf '%s\n' "$RECOVERED" | pick 'd.result.structuredContent.degraded')"
assert_eq "and the mount is contacted again" "0" \
  "$(printf '%s\n' "$RECOVERED" | pick 'd.result.structuredContent.mounts.find(m => m.id === "wiki").candidateCount')"
note "Recovery needs NO server restart and no reindex: the flag is recomputed per call,"
note "so the next search is clean. But candidateCount is 0 -- and that is the honest"
note "part. This mock keeps its corpus in memory, so killing the process destroyed the"
note "index too. A real Algolia outage would not; here the corpus has to come back from"
note "the snapshot taken above, which is exactly what an operator would do:"
cmd "deep-obsidian-mcp algolia restore --mount wiki --from <sandbox>/snapshots/wiki"
"$BIN" --config "$CONFIG" algolia restore --mount wiki --from "$WORK/snapshots/wiki"
RESTORED="$(mcp hybrid_search '{"query":"federation","limit":10}')"
printf '%s\n' "$RESTORED" | pick 'JSON.stringify({degraded: d.result.structuredContent.degraded, count: d.result.structuredContent.count, matches: d.result.structuredContent.matches.map(m => ({path: m.path, mountId: m.mountId}))}, null, 2)' | show
echo
assert_eq "the shared corpus is back in the federation" "team,vault,wiki" \
  "$(printf '%s\n' "$RESTORED" | pick '[...new Set(d.result.structuredContent.matches.map(m => m.mountId))].sort().join(",")')"

# -----------------------------------------------------------------------------
# STEP 8 -- the binary exception
# -----------------------------------------------------------------------------

banner "THE BINARY EXCEPTION" \
  "The Algolia mount stores Markdown only. It says so, by name, at both doors --" \
  "and the refusal is a property of the storage, not a setting."

say "read_artifact on a binary path under Wiki/"
note "A .md path would answer 'unsupported artifact type' instead: read_artifact checks"
note "the artifact type before it reaches the mount, so a binary path is what surfaces"
note "the mount's own refusal."
cmd 'tools/call read_artifact {path: Wiki/diagram.png, includeBase64: true}'
ARTIFACT="$(mcp read_artifact '{"path":"Wiki/diagram.png","includeBase64":true}')"
printf '%s\n' "$ARTIFACT" | pick 'd.error.message'
echo
assert_contains "read_artifact refusal" "MARKDOWN ONLY" "$(printf '%s\n' "$ARTIFACT" | pick 'd.error.message')"
assert_contains "read_artifact refusal" "This is a property of the storage, not a setting" \
  "$(printf '%s\n' "$ARTIFACT" | pick 'd.error.message')"

say "request_vault_upload targeting Wiki/"
cmd 'tools/call request_vault_upload {path: Wiki/diagram.png}'
UPLOAD="$(mcp request_vault_upload '{"path":"Wiki/diagram.png","mimeType":"image/png"}')"
printf '%s\n' "$UPLOAD" | pick 'd.error.message'
echo
assert_contains "upload refusal" "no token is issued" "$(printf '%s\n' "$UPLOAD" | pick 'd.error.message')"
note "Refused at the MINT, before a token exists -- a token would only have failed"
note "after the body had already been streamed."

say "The same upload against the filesystem mount is issued normally"
cmd 'tools/call request_vault_upload {path: Attachments/diagram.png}'
OK_UPLOAD="$(mcp request_vault_upload '{"path":"Attachments/diagram.png","mimeType":"image/png"}')"
printf '%s\n' "$OK_UPLOAD" | show '.result.structuredContent'
assert_eq "filesystem upload issued" "Attachments/diagram.png" \
  "$(printf '%s\n' "$OK_UPLOAD" | pick 'd.result.structuredContent.path')"
note "Same tool, same namespace, different mount: the capability difference from step 3"
note "is what an agent hits here, worded for the mount it actually addressed."

# -----------------------------------------------------------------------------
# STEP 9 -- versioning
# -----------------------------------------------------------------------------

banner "VERSIONING" \
  "The Algolia backend never overwrites: a write appends a version." \
  "Deletion is a tombstone, so the history survives the note."

say "Two more writes to Wiki/Architecture.md"
mcp upsert_note '{"path":"Wiki/Architecture.md","content":"# Architecture\n\nVersion three: routing, recall, degradation.\n"}' \
  | pick '`  ${d.result.structuredContent.action} -> ${d.result.structuredContent.newHash}`'
echo
mcp upsert_note '{"path":"Wiki/Architecture.md","content":"# Architecture\n\nVersion four: federation is the whole point.\n"}' \
  | pick '`  ${d.result.structuredContent.action} -> ${d.result.structuredContent.newHash}`'
echo

say "note_history"
cmd "tools/call note_history {path: Wiki/Architecture.md}"
HISTORY="$(mcp note_history '{"path":"Wiki/Architecture.md"}')"
printf '%s\n' "$HISTORY" | show '.result.structuredContent'
VERSION_COUNT="$(printf '%s\n' "$HISTORY" | pick 'd.result.structuredContent.count')"
note "Every version records who wrote it (participantId) and what it superseded, so a"
note "chain of parentVersionId links reconstructs the whole edit order."
note "The chain starts at step 7's restore, not at step 4's first write: the mock's"
note "corpus is in-memory, so the outage took the earlier versions with it and the"
note "snapshot re-seeded the note as version 1. A real index would keep the full chain."
[[ "$VERSION_COUNT" -ge 3 ]] || die "expected at least 3 versions, got $VERSION_COUNT"
pass "history has $VERSION_COUNT versions, newest first"

say "read_version of the OLDEST version"
OLDEST="$(printf '%s\n' "$HISTORY" | pick 'd.result.structuredContent.versions[d.result.structuredContent.versions.length - 1].versionId')"
cmd "tools/call read_version {versionId: $OLDEST}"
OLD="$(mcp read_version "{\"path\":\"Wiki/Architecture.md\",\"versionId\":\"$OLDEST\"}")"
printf '%s\n' "$OLD" | show '.result.structuredContent'
assert_contains "the first version's text is still readable" "longest-prefix mount match" \
  "$(printf '%s\n' "$OLD" | pick 'd.result.structuredContent.text')"

say "delete_note -> a tombstone that tells you how to undo it"
cmd "tools/call delete_note {path: Wiki/Architecture.md}"
DELETED="$(mcp delete_note '{"path":"Wiki/Architecture.md"}')"
printf '%s\n' "$DELETED" | show '.result.structuredContent'
assert_eq "deleted" "true" "$(printf '%s\n' "$DELETED" | pick 'd.result.structuredContent.deleted')"
assert_contains "the tombstone carries its own recovery instruction" "read_version with versionId" \
  "$(printf '%s\n' "$DELETED" | pick 'd.result.structuredContent.howToRecover')"

say "Reading it now fails honestly..."
GONE="$(mcp read_file '{"path":"Wiki/Architecture.md"}')"
printf '%s\n' "$GONE" | show
assert_contains "read of a deleted note" "no note at" "$(printf '%s\n' "$GONE" | pick 'd.error.message')"

say "...while the history is still there"
AFTER="$(mcp note_history '{"path":"Wiki/Architecture.md"}')"
printf '%s\n' "$AFTER" | pick '`  versions: ${d.result.structuredContent.count} (the delete itself is the newest one)`'
echo
[[ "$(printf '%s\n' "$AFTER" | pick 'd.result.structuredContent.count')" -gt "$VERSION_COUNT" ]] \
  || die "the tombstone should have added a version"
pass "the deletion is a version, not an erasure"

# -----------------------------------------------------------------------------
# STEP 10 -- ops
# -----------------------------------------------------------------------------

banner "OPS" \
  "doctor's per-mount lines, and a snapshot of each remote backend to a directory." \
  "Both are the operator's answer to 'what is mounted, and can I get my data out?'"

say "doctor, with the mount lines and the per-mount checks"
cmd "deep-obsidian-mcp --config <sandbox>/config.json doctor --probe-remote"
DOCTOR_TEXT="$("$BIN" --config "$CONFIG" doctor --probe-remote 2>&1)"
printf '%s\n' "$DOCTOR_TEXT" | grep -E '^(mount |\[(ok|warn|fail|skip)\] mount)' || true
assert_contains "doctor lists the couchdb mount" "mount team at /Team (couchdb)" "$DOCTOR_TEXT"
assert_contains "doctor lists the algolia mount" "mount wiki at /Wiki (algolia)" "$DOCTOR_TEXT"

note "Both mounts here are writable, so neither line may carry '(read-only)'."
note "The couchdb arm of this label had exactly that bug (unconditional suffix) --"
note "found by an earlier revision of this demo, fixed and pinned by"
note "doctor_mount_lines_track_writability. These assertions keep both arms honest."
assert_not_contains "the couchdb mount line reports writability correctly" "(read-only)" \
  "$(printf '%s\n' "$DOCTOR_TEXT" | grep '^mount team ' || true)"
assert_not_contains "the algolia mount line reports writability correctly" "(read-only)" \
  "$(printf '%s\n' "$DOCTOR_TEXT" | grep '^mount wiki ' || true)"
echo

say "couchdb export --mount team: the LiveSync vault as a directory tree"
cmd "deep-obsidian-mcp --config <sandbox>/config.json couchdb export --mount team --out <sandbox>/export/team"
"$BIN" --config "$CONFIG" couchdb export --mount team --out "$WORK/export/team"
echo
find "$WORK/export/team" -type f | sed "s|$WORK/export/team|  team|" | sort
say "and its manifest"
pick 'JSON.stringify({version: d.version, mount: d.mount, entries: d.entries.length, sample: d.entries.slice(0, 2)}, null, 2)' < "$WORK/export/team/manifest.json" | show
echo
assert_contains "the note written in step 4 is in the export" "Standup.md" \
  "$(find "$WORK/export/team" -type f -print0 | xargs -0 -n1 basename | tr '\n' ' ')"
assert_contains "so is the fixture's binary attachment" "logo.png" \
  "$(find "$WORK/export/team" -type f -print0 | xargs -0 -n1 basename | tr '\n' ' ')"

say "algolia dump --mount wiki: the shared corpus as a directory tree"
cmd "deep-obsidian-mcp --config <sandbox>/config.json algolia dump --mount wiki --out <sandbox>/export/wiki"
"$BIN" --config "$CONFIG" algolia dump --mount wiki --out "$WORK/export/wiki"
echo
find "$WORK/export/wiki" -type f | sed "s|$WORK/export/wiki|  wiki|" | sort
pick 'JSON.stringify({version: d.version, mount: d.mount, entries: d.entries.length, sample: d.entries.slice(0, 2)}, null, 2)' < "$WORK/export/wiki/manifest.json" | show
echo
note "Architecture.md is absent: it was deleted in step 9, and a dump snapshots the"
note "LIVE corpus. Its versions are still in the index, reachable through read_version."
assert_contains "the surviving note is dumped" "Recall.md" \
  "$(find "$WORK/export/wiki" -type f -print0 | xargs -0 -n1 basename | tr '\n' ' ')"

# -----------------------------------------------------------------------------
# STEP 11 -- teardown
# -----------------------------------------------------------------------------

banner "TEARDOWN" "Every child is killed and the sandbox removed by the EXIT trap."

cat <<RECAP

  What just ran, and what each part proved:

    2  three backends behind ONE config, secrets only ever as SecretRefs
       (values live in the sandbox's encrypted secrets file; print-config shows none)
    3  vault_info's mounts[] -- per-mount backendKind and capabilities, including
       the Algolia mount's absent binary capabilities and the CouchDB mount's watch
    4  path-based routing, proved from INSIDE each backend: a LiveSync entry+leaf
       pair in CouchDB, note+chunk records in Algolia, byte-identical reads back,
       and synthesized mount folders indistinguishable from physical ones
    5  expectedHash as compare-and-set: the stale write refused with both hashes
       named, the fresh one applied
    6  one unscoped hybrid_search fanning out to three mounts, per-hit mountId,
       per-mount provenance (native-recall vs local-index), and an honest
       rerank: none / recallMode: lexical with no embedding backend configured
    7  a backend dying mid-session -> degraded: true + missingBackends, surviving
       hits intact, reads refused loudly; recovery on the next call with no
       restart, and a snapshot restore to bring the corpus back
    8  the Markdown-only storage refusing binary reads and upload tokens by name,
       while the same tools keep working on the filesystem mount
    9  append-only versions, read_version of the first one, and a delete that is a
       tombstone: the note is gone, the history is not
   10  doctor's per-mount lines and both export paths -- plus one real bug, in
       doctor's couchdb line, reported rather than papered over

  Runtime: ${SECONDS}s.

RECAP
pass "demo complete"
