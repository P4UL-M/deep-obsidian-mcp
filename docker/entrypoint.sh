#!/bin/sh
# Container entrypoint for deep-obsidian-mcp. POSIX sh: no bashisms, so it runs the
# same under `sh -c` and under a distroless-ish shell if the base image ever changes.
#
# ===========================================================================
# The contract, in one place
# ===========================================================================
#
# ## 1. Which config the server runs
#
# A MOUNTED config wins. If $DO_MOUNTED_CONFIG exists, it is used verbatim and this
# script writes NOTHING into it: it validates the file through `print-config` (the
# real loader, so an invalid file fails here with the loader's own message rather
# than half a boot later), injects secrets into the references the file already
# declares, and serves. The volume's own $DO_CONFIG_PATH is not created or touched.
#
# Otherwise the config is derived from the environment, ONCE, into
# $DO_CONFIG_PATH — which lives on the volume, so every later boot finds it and
# skips straight to secret injection. Changing a DO_* variable after the first boot
# therefore changes nothing until the file is removed (or DO_REBUILD_CONFIG=1 is
# set); that is deliberate, because the config the operator has been running is a
# fact and silently rewriting it on restart is how an index gets orphaned.
#
# Two settings are NOT taken from either config, and this is the one exception to
# "the mounted config wins": the HTTP host and port are forced from
# $DO_HTTP_HOST/$DO_HTTP_PORT on the command line. Both are container-deployment
# facts — a config written on a laptop says `127.0.0.1`, which in a container means
# "reachable by nobody", and a port the container did not expect would silently
# break the image's HEALTHCHECK. CLI flags beat the config file in
# `resolve_runtime_config`, which is why they are passed rather than exported.
#
# ## 2. Secrets are ephemeral, by construction
#
# $XDG_CONFIG_HOME is inside the container's home directory and NOT on the volume,
# so the encrypted secret store (`$XDG_CONFIG_HOME/deep-obsidian-mcp/secrets.json`,
# derived by `default_secrets_path()` independently of `--config`) dies with the
# container. This script deletes it before every boot and re-injects each secret
# from $DO_SECRETS_DIR, so:
#
#   * the volume holds an index and a config file and never a credential;
#   * the file store's derived-key weakness (see the threat model in
#     CONFIGURATION.md, "the encrypted file store") stops mattering, because the
#     ciphertext is never persisted anywhere an attacker outlives it;
#   * a secret that disappears from the orchestrator fails LOUDLY on the next boot
#     instead of continuing to work from a stale copy nobody remembers storing.
#
# The bearer token is the exception that proves the rule: it is exported as
# DEEP_OBSIDIAN_AUTH_TOKEN rather than stored. `secrets set --target auth-token`
# needs `auth.tokenRef` to already exist in the config, and the only command that
# creates it (`setup-service --auth`) REFUSES a config with a mount table
# (`setup_service_refuses_an_auth_change_on_a_mounts_config`). The env var is the
# path the server documents for exactly this case (bootstrap.rs: "useful for
# containers, tunnels, and headless hosts where the OS keyring is absent"), it
# overrides any configured reference, and because this script sets it — rather than
# the image or the compose file — it does not appear in `docker inspect`.
#
# ## 3. Auth is required
#
# No token file and no explicit DO_INSECURE_NO_AUTH=1 is a refusal to start, before
# anything binds. The server would also refuse (it will not expose a non-loopback
# bind unauthenticated), but its message is about a config a container operator
# never wrote; this one names the secret file.
#
# ## 4. Anything but `serve`
#
# `docker compose run deep-obsidian doctor --probe-remote`, `... secrets check`,
# `... mounts list` all work: the prep above runs (so the secrets are in place) and
# then the CLI is exec'd with the arguments given, with `--config` injected.

set -eu

log() { printf '[entrypoint] %s\n' "$*" >&2; }
die() { printf '[entrypoint] FATAL: %s\n' "$*" >&2; exit 1; }

is_true() {
    case "${1:-}" in
        1 | true | TRUE | True | yes | YES | on | ON) return 0 ;;
        *) return 1 ;;
    esac
}

BIN=/opt/deep-obsidian-mcp/bin/deep-obsidian-mcp

: "${DO_STATE_DIR:=/var/lib/deep-obsidian-mcp}"
: "${DO_CONFIG_PATH:=$DO_STATE_DIR/config.json}"
: "${DO_MOUNTED_CONFIG:=/etc/deep-obsidian/config.json}"
: "${DO_SECRETS_DIR:=/run/secrets}"
: "${DO_INDEX_DIR:=$DO_STATE_DIR/index}"
: "${DO_ROOT_KIND:=filesystem}"
: "${DO_ROOT_ID:=root}"
: "${DO_VAULT_PATH:=/vault}"
: "${DO_HTTP_HOST:=0.0.0.0}"
: "${DO_HTTP_PORT:=4100}"
: "${DO_INSECURE_NO_AUTH:=0}"
: "${DO_REBUILD_CONFIG:=0}"
: "${XDG_CONFIG_HOME:=$HOME/.config}"

# EXPORTED, not merely set. The `:=` above assigns a shell variable, and a variable
# that fell back to its default here would be invisible to the `node` helpers below,
# which read the whole contract out of `process.env` — a config would then be written
# with `indexDir: undefined` (silently dropped by JSON.stringify) rather than with the
# documented path. Most of these are also image ENV, so exporting is a no-op for them;
# doing it for all of them is what makes that irrelevant.
export DO_STATE_DIR DO_CONFIG_PATH DO_MOUNTED_CONFIG DO_SECRETS_DIR DO_INDEX_DIR \
    DO_ROOT_KIND DO_ROOT_ID DO_VAULT_PATH DO_HTTP_HOST DO_HTTP_PORT \
    DO_INSECURE_NO_AUTH DO_REBUILD_CONFIG XDG_CONFIG_HOME

SECRETS_STORE="$XDG_CONFIG_HOME/deep-obsidian-mcp/secrets.json"

# Every CLI invocation goes through here so none can forget `--config` and silently
# read the (empty, ephemeral) default path under $XDG_CONFIG_HOME instead.
CONFIG=""
cli() { "$BIN" --config "$CONFIG" "$@"; }

# ---------------------------------------------------------------------------
# 1. Pick the config
# ---------------------------------------------------------------------------
CONFIG_SOURCE=""
if [ -f "$DO_MOUNTED_CONFIG" ]; then
    CONFIG="$DO_MOUNTED_CONFIG"
    CONFIG_SOURCE=mounted
    log "config: $CONFIG (mounted; it wins over every DO_* config variable)"
else
    CONFIG="$DO_CONFIG_PATH"
    CONFIG_SOURCE=env
fi

# ---------------------------------------------------------------------------
# 2. Derive the config from the environment, on first boot only
# ---------------------------------------------------------------------------

# A remote ROOT mount is written here rather than through `mounts add`, and that is
# a limitation of the CLI, not a preference:
#
#   * `mounts add` refuses a config that declares neither `vaultPath` nor `mounts`
#     ("there is no vault to add a mount beside" — `allow_empty_base: false`, hard
#     coded), so it cannot create the first mount;
#   * seeding a filesystem root first and then adding the remote at `mountAt ""`
#     fails validation ('"vault" and "live" both mount at ""'), and nothing is
#     written;
#   * the only code path that CAN create a remote root is the first-init wizard,
#     which is gated on `io::stdin().is_terminal()`.
#
# So: write the file, then hand it to `print-config`, which runs the real
# `normalize_service_config` and fails with the loader's own message if anything
# here is wrong. The references are written as `encryptedFile` deliberately — the
# container has no OS keyring, and `secrets set` preserves a reference's kind rather
# than falling back, so an `osKeyring` reference would be unfillable.
#
# Follow-up worth having in the CLI: a non-interactive way to declare the root mount
# (`mounts add --allow-empty-base`, or `setup-service --root <kind>`), after which
# this function collapses into two `mounts add` calls.
write_env_config() {
    mkdir -p "$(dirname "$CONFIG")" "$DO_INDEX_DIR"

    case "$DO_ROOT_KIND" in
        filesystem)
            [ -d "$DO_VAULT_PATH" ] || die "DO_VAULT_PATH=$DO_VAULT_PATH is not a directory; mount the vault there (or set DO_ROOT_KIND=couchdb|algolia)"
            # The one kind the CLI can express on its own, so it does.
            #
            # `--index-dir` is passed explicitly because the default for a
            # filesystem vault is `<vault>/.deep-obsidian-mcp`, i.e. INSIDE the
            # vault: that would write into a bind mount the operator may have
            # mounted read-only, and it would put a container's index into a
            # directory an Obsidian client syncs.
            log "first boot: deriving a filesystem-root config from the environment"
            cli setup-service \
                --vault "$DO_VAULT_PATH" \
                --index-dir "$DO_INDEX_DIR" \
                --transport http \
                --host "$DO_HTTP_HOST" \
                --port "$DO_HTTP_PORT" >&2
            ;;
        couchdb)
            [ -n "${DO_COUCHDB_URL:-}" ] || die "DO_ROOT_KIND=couchdb needs DO_COUCHDB_URL (e.g. http://couchdb:5984)"
            [ -n "${DO_COUCHDB_DATABASE:-}" ] || die "DO_ROOT_KIND=couchdb needs DO_COUCHDB_DATABASE"
            [ -n "${DO_COUCHDB_USERNAME:-}" ] || die "DO_ROOT_KIND=couchdb needs DO_COUCHDB_USERNAME"
            log "first boot: deriving a couchdb-root config from the environment"
            write_remote_config couchdb
            ;;
        algolia)
            [ -n "${DO_ALGOLIA_APP_ID:-}" ] || die "DO_ROOT_KIND=algolia needs DO_ALGOLIA_APP_ID"
            [ -n "${DO_ALGOLIA_INDEX_NAME:-}" ] || die "DO_ROOT_KIND=algolia needs DO_ALGOLIA_INDEX_NAME"
            log "first boot: deriving an algolia-root config from the environment"
            write_remote_config algolia
            ;;
        *)
            die "DO_ROOT_KIND=$DO_ROOT_KIND is not one of filesystem, couchdb, algolia"
            ;;
    esac
}

# The JSON is emitted by node rather than by `printf`, so a database name with a
# quote in it cannot produce a broken file — and node is present because the couchdb
# mount kind needs it for the sidecar anyway. Secret ids follow the same
# `mount-<id>-<purpose>` convention `mounts add` derives, so a container-written
# config and a CLI-written one address the same store entries.
write_remote_config() {
    kind="$1"
    DO_ENTRYPOINT_KIND="$kind" node -e '
const kind = process.env.DO_ENTRYPOINT_KIND;
const id = process.env.DO_ROOT_ID;
const truthy = (v) => ["1", "true", "TRUE", "True", "yes", "YES", "on", "ON"].includes(v ?? "");
const ref = (purpose) => ({ kind: "encryptedFile", id: `mount-${id}-${purpose}` });

const backend = kind === "couchdb"
    ? {
        kind: "couchdb",
        url: process.env.DO_COUCHDB_URL,
        database: process.env.DO_COUCHDB_DATABASE,
        username: process.env.DO_COUCHDB_USERNAME,
        passwordRef: ref("password"),
        ...(truthy(process.env.DO_COUCHDB_WRITABLE) ? { writable: true } : {}),
        ...(truthy(process.env.DO_COUCHDB_E2EE)
            ? { e2ee: { passphraseRef: ref("e2ee-passphrase") } }
            : {}),
      }
    : {
        kind: "algolia",
        appId: process.env.DO_ALGOLIA_APP_ID,
        indexName: process.env.DO_ALGOLIA_INDEX_NAME,
        apiKeyRef: ref("api-key"),
        ...(process.env.DO_ALGOLIA_BASE_URL ? { baseUrl: process.env.DO_ALGOLIA_BASE_URL } : {}),
        ...(truthy(process.env.DO_ALGOLIA_WRITABLE) ? { writable: true } : {}),
        ...(process.env.DO_ALGOLIA_PARTICIPANT_ID
            ? { participantId: process.env.DO_ALGOLIA_PARTICIPANT_ID }
            : {}),
      };

const config = {
    // A remote root has no local directory to hang an index off, so it is named
    // explicitly rather than left to `default_remote_root_index_dir` — which would
    // resolve under $XDG_DATA_HOME (also the volume), but by a path derived from
    // the mount id that an operator has no reason to be able to predict.
    indexDir: process.env.DO_INDEX_DIR,
    transport: "http",
    http: {
        host: process.env.DO_HTTP_HOST,
        port: Number(process.env.DO_HTTP_PORT),
        mcpPath: "/mcp",
        healthPath: "/healthz",
    },
    // A single-mount fully-remote table needs only the per-kind flag; multiVault
    // gates a table of SEVERAL mounts, which this is not.
    experimental: kind === "couchdb" ? { couchdbVaults: true } : { algoliaVaults: true },
    mounts: [{ id, mountAt: "", backend }],
};
process.stdout.write(JSON.stringify(config, null, 2) + "\n");
' > "$CONFIG.tmp"
    mv "$CONFIG.tmp" "$CONFIG"
    log "wrote config: $CONFIG"
}

if [ "$CONFIG_SOURCE" = env ]; then
    if [ -f "$CONFIG" ] && is_true "$DO_REBUILD_CONFIG"; then
        log "DO_REBUILD_CONFIG is set: re-deriving $CONFIG from the environment"
        rm -f "$CONFIG"
    fi
    if [ -f "$CONFIG" ]; then
        log "config: $CONFIG (already on the volume; DO_* config variables are not re-applied — set DO_REBUILD_CONFIG=1 to re-derive)"
    else
        write_env_config
    fi
fi

# ---------------------------------------------------------------------------
# 3. Validate, whichever config it is
# ---------------------------------------------------------------------------
# `print-config` runs the same `normalize_service_config` the server runs and
# redacts secrets, so this both fails fast on a bad file and leaves a readable
# record of what the container is actually about to serve.
log "validating $CONFIG"
cli print-config >&2 || die "$CONFIG is not a config this build can load (see the loader error above)"

# ---------------------------------------------------------------------------
# 4. Re-inject every secret, from scratch
# ---------------------------------------------------------------------------
rm -f "$SECRETS_STORE"
mkdir -p "$(dirname "$SECRETS_STORE")"

# Which mount owns which credential is read out of the config rather than assumed,
# so a mounted config with a differently-named root works without extra variables.
mounts_of_kind() {
    node -e '
const fs = require("fs");
const [file, kind, needle] = process.argv.slice(1);
// Deliberately unguarded: `print-config` has already parsed this file through the
// real loader, so a throw here means the file changed underneath us or the loader
// and JSON.parse disagree — either way a silent empty answer would report "no mount
// to inject into" for a config that has one, and the credential would go missing
// without anyone being told.
const config = JSON.parse(fs.readFileSync(file, "utf8"));
for (const mount of config.mounts ?? []) {
    const backend = mount.backend ?? {};
    if (backend.kind !== kind) continue;
    if (needle === "e2ee" && !backend.e2ee) continue;
    console.log(mount.id);
}
' "$CONFIG" "$1" "${2:-}"
}

# One secret file -> one reference. The value is piped, never passed as an argument,
# so it never reaches `ps` or a shell history; `secrets set --stdin` reads the first
# line with the newline stripped and refuses a blank one.
inject_mount_secret() {
    file="$1"
    kind="$2"
    field="$3"
    needle="${4:-}"

    [ -f "$file" ] || return 0
    # A file the container cannot READ is its own failure mode, and a distinct
    # message: the usual cause is `chmod 600` on the host, which leaves the file
    # readable only by the host uid that owns it while this process is uid 10001.
    [ -r "$file" ] || die "$file exists but is not readable by $(id -un) (uid $(id -u)). Secret files are bind-mounted with their host permissions: use mode 644 inside a directory only you can enter (chmod 700 secrets && chmod 644 secrets/*), or run the container as the owning uid with compose's 'user:'."
    ids=$(mounts_of_kind "$kind" "$needle")
    if [ -z "$ids" ]; then
        log "WARNING: $file was provided but $CONFIG declares no $kind mount${needle:+ with e2ee} to inject it into; ignoring it"
        return 0
    fi
    if [ "$(printf '%s\n' "$ids" | wc -l)" -gt 1 ]; then
        die "$file is a single value but $CONFIG declares several $kind mounts ($(printf '%s' "$ids" | tr '\n' ' ')); a per-mount secret file convention does not exist yet, so mount one config per service or rotate these by hand with 'secrets set --mount <id>'"
    fi
    log "injecting $(basename "$file") into mount '$ids' ($field)"
    # A failure here is fatal on purpose: the alternative is a mount that cannot
    # authenticate and a startup log that looked fine.
    head -n 1 "$file" | cli secrets set --mount "$ids" --field "$field" --stdin >&2 \
        || die "could not store $(basename "$file") for mount '$ids'. If the config's reference is an 'osKeyring' one, change it to {\"kind\":\"encryptedFile\",\"id\":\"...\"}: a container has no keyring, and 'secrets set' preserves a reference's kind rather than silently writing somewhere the config does not point."
}

inject_mount_secret "$DO_SECRETS_DIR/couchdb_password" couchdb password
inject_mount_secret "$DO_SECRETS_DIR/e2ee_passphrase" couchdb e2ee-passphrase e2ee
inject_mount_secret "$DO_SECRETS_DIR/algolia_api_key" algolia api-key

# Informational, never a gate: `secrets check` reports the STORE, and a MISSING line
# can be correct for a reference an environment variable shadows at runtime.
cli secrets check >&2 || log "WARNING: 'secrets check' reported a missing or unreadable reference (see above); mounts needing it will start degraded"

# ---------------------------------------------------------------------------
# 5. Anything other than `serve` runs and exits
# ---------------------------------------------------------------------------
if [ "$#" -gt 0 ] && [ "$1" != serve ]; then
    log "running: deep-obsidian-mcp $*"
    exec "$BIN" --config "$CONFIG" "$@"
fi

# ---------------------------------------------------------------------------
# 6. The auth gate
# ---------------------------------------------------------------------------
AUTH_TOKEN_FILE="$DO_SECRETS_DIR/auth_token"
INSECURE_FLAG=""
# Checked separately from the emptiness test below so "you cannot read it" and "it is
# blank" are different messages: an unreadable token file otherwise looks exactly
# like a missing one, and the remedy is completely different.
if [ -f "$AUTH_TOKEN_FILE" ] && [ ! -r "$AUTH_TOKEN_FILE" ]; then
    die "$AUTH_TOKEN_FILE exists but is not readable by $(id -un) (uid $(id -u)). Secret files keep their host permissions when bind-mounted: use mode 644 inside a directory only you can enter (chmod 700 secrets && chmod 644 secrets/*), or run the container as the owning uid with compose's 'user:'."
fi
if [ -s "$AUTH_TOKEN_FILE" ] && [ -n "$(head -n 1 "$AUTH_TOKEN_FILE" | tr -d '[:space:]')" ]; then
    DEEP_OBSIDIAN_AUTH_TOKEN="$(head -n 1 "$AUTH_TOKEN_FILE")"
    export DEEP_OBSIDIAN_AUTH_TOKEN
    log "HTTP bearer auth enabled from $AUTH_TOKEN_FILE"
    if is_true "$DO_INSECURE_NO_AUTH"; then
        log "WARNING: DO_INSECURE_NO_AUTH is set but a token was provided; the token wins and auth stays ON"
    fi
elif is_true "$DO_INSECURE_NO_AUTH"; then
    # Passed as a flag, which is what sets DEEP_OBSIDIAN_ALLOW_INSECURE for the
    # bootstrap's fail-closed exposure check.
    INSECURE_FLAG=--insecure-no-auth
    log "WARNING: DO_INSECURE_NO_AUTH=1 — serving $DO_HTTP_HOST:$DO_HTTP_PORT with NO authentication. Every client that can reach this port can read and write the vault. Use this only behind a private network boundary you control."
else
    die "no bearer token: mount a non-empty secret at $AUTH_TOKEN_FILE (compose: 'secrets: [auth_token]'), or set DO_INSECURE_NO_AUTH=1 to serve without authentication on purpose. Refusing to start: this container binds $DO_HTTP_HOST, and an unauthenticated MCP endpoint on a routable address exposes the whole vault."
fi

# ---------------------------------------------------------------------------
# 7. Serve, as PID 1
# ---------------------------------------------------------------------------
# `exec` so the server IS the container's PID 1 and receives SIGTERM directly: it
# handles SIGTERM by stopping its sidecar children before exiting, which a shell
# wrapper swallowing the signal would turn into a 10-second SIGKILL on every
# `docker stop`.
log "starting: deep-obsidian-mcp serve --transport http --host $DO_HTTP_HOST --port $DO_HTTP_PORT"
exec "$BIN" \
    --config "$CONFIG" \
    --transport http \
    --host "$DO_HTTP_HOST" \
    --port "$DO_HTTP_PORT" \
    ${INSECURE_FLAG:+$INSECURE_FLAG} \
    serve
