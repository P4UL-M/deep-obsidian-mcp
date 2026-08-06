# Running deep-obsidian-mcp in Docker

The container is a long-lived HTTP MCP server: bearer-authenticated, non-root, with
its index on a volume and its credentials nowhere. It is the deployment shape for a
CouchDB-backed vault in particular — the Obsidian LiveSync database that your
devices already sync — but it serves a plain directory just as well.

- [Quickstart](#quickstart)
- [The environment matrix](#the-environment-matrix)
- [Config precedence](#config-precedence)
- [Secrets are ephemeral](#secrets-are-ephemeral)
- [Degraded until the first client sync](#degraded-until-the-first-client-sync)
- [TLS is the proxy's job](#tls-is-the-proxys-job)
- [Running one-off commands](#running-one-off-commands)
- [Volumes, uids and the vault](#volumes-uids-and-the-vault)
- [Building and testing locally](#building-and-testing-locally)
- [What the image contains](#what-the-image-contains)
- [Known limitations](#known-limitations)

## Quickstart

`docker-compose.example.yml` in the repository root is a complete deployment:
CouchDB with the settings Self-hosted LiveSync requires, a one-shot provisioning
job, and the MCP server reading that database as its **root** vault.

```bash
cp docker-compose.example.yml docker-compose.yml
mkdir -p secrets
openssl rand -hex 24 > secrets/couchdb_password
openssl rand -hex 32 > secrets/auth_token
printf 'COUCHDB_USER=obsidian\nCOUCHDB_PASSWORD=%s\n' "$(cat secrets/couchdb_password)" > .env
# 700 on the directory, 644 on the files. A bind-mounted secret keeps its HOST
# permissions and the container runs as uid 10001, so `chmod 600` would leave a file
# only the host owner can read — the entrypoint then refuses to start and says so.
chmod 700 secrets && chmod 644 secrets/* && chmod 600 .env

docker compose up -d --build
curl -s localhost:4100/healthz
curl -s -H "Authorization: Bearer $(cat secrets/auth_token)" \
     -H 'Content-Type: application/json' \
     -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"c","version":"0"}}}' \
     localhost:4100/mcp
```

For a **filesystem** vault instead, the whole thing is one command — the
entrypoint's default root kind is `filesystem` and its default vault path is
`/vault`:

```bash
docker run -d --name deep-obsidian \
  -p 127.0.0.1:4100:4100 \
  -v "$HOME/Obsidian/MyVault:/vault" \
  -v deep-obsidian-state:/var/lib/deep-obsidian-mcp \
  -v "$PWD/secrets:/run/secrets:ro" \
  ghcr.io/p4ul-m/deep-obsidian-mcp:latest   # (see Known limitations: not published yet — build it)
```

Point your MCP client at `http://localhost:4100/mcp` with the bearer token from
`secrets/auth_token`.

The compose file's `couchdb-init` one-shot does the CouchDB side: it applies the
settings Self-hosted LiveSync requires (`chttpd`/`chttpd_auth require_valid_user`,
`httpd`/`chttpd enable_cors`, `cors credentials` + the three Obsidian origins,
`couchdb max_document_size`, `chttpd max_http_request_size`, `WWW-Authenticate`),
finishes single-node cluster setup — a fresh `couchdb:3` with only `COUCHDB_USER` set
has no `_users`/`_replicator` — and creates the database. It applies them through
CouchDB's `_config` API rather than a mounted `local.ini`, because the official
image's entrypoint chowns everything under `/opt/couchdb` before starting: a
read-only bind mount there makes that `chown` fail and the container exits 1 **with
no log output at all**. Two consequences: the job is idempotent and re-runs on every
`up` (CouchDB stores runtime config changes in the image layer, not in the data
volume), and the `couchdb` healthcheck must pass `-u` credentials, since
`require_valid_user` makes even `/_up` answer 401.

## The environment matrix

Every variable is read by `docker/entrypoint.sh`. All of them have defaults; none is
a secret (secrets are files — see [below](#secrets-are-ephemeral)).

| Variable | Default | What it does |
| --- | --- | --- |
| `DO_ROOT_KIND` | `filesystem` | `filesystem`, `couchdb` or `algolia`: what the vault ROOT is. |
| `DO_ROOT_ID` | `root` | The mount id in the generated config. Also names the store entries a secret is injected into (`mount-<id>-password`, ...). |
| `DO_VAULT_PATH` | `/vault` | `filesystem` roots only: the directory to serve. Must exist. |
| `DO_COUCHDB_URL` | — | `couchdb` roots: the server origin, no database path, no `user:password@`. Required. |
| `DO_COUCHDB_DATABASE` | — | The LiveSync database name. Required. |
| `DO_COUCHDB_USERNAME` | — | The CouchDB user. An identifier, not a credential. Required. |
| `DO_COUCHDB_WRITABLE` | `false` | `true` lets the agent write notes back into the LiveSync database. |
| `DO_COUCHDB_E2EE` | `false` | `true` declares the vault end-to-end encrypted; then `/run/secrets/e2ee_passphrase` is required too. |
| `DO_ALGOLIA_APP_ID` | — | `algolia` roots: the application id. Required. |
| `DO_ALGOLIA_INDEX_NAME` | — | The index holding the corpus. Required. |
| `DO_ALGOLIA_BASE_URL` | — | Override for `https://{appId}.algolia.net`. |
| `DO_ALGOLIA_WRITABLE` | `false` | `true` lets the agent write to the corpus. |
| `DO_ALGOLIA_PARTICIPANT_ID` | — | Who this deployment is in the corpus's audit trail. |
| `DO_HTTP_HOST` | `0.0.0.0` | Forced on the command line; see [precedence](#config-precedence). |
| `DO_HTTP_PORT` | `4100` | Same. Change it and the published port together — the image's `HEALTHCHECK` follows this variable. |
| `DO_INSECURE_NO_AUTH` | `0` | `1` serves with **no authentication**. Refuses-to-start is the default. |
| `DO_CONFIG_PATH` | `/var/lib/deep-obsidian-mcp/config.json` | Where the derived config lives (on the volume). |
| `DO_MOUNTED_CONFIG` | `/etc/deep-obsidian/config.json` | If this file exists it WINS and nothing is derived. |
| `DO_INDEX_DIR` | `/var/lib/deep-obsidian-mcp/index` | The index directory written into the derived config. |
| `DO_SECRETS_DIR` | `/run/secrets` | Where the secret files are mounted. |
| `DO_REBUILD_CONFIG` | `0` | `1` re-derives the config from the environment on this boot, replacing the volume's. |

## Config precedence

1. **A mounted `/etc/deep-obsidian/config.json` wins.** The entrypoint validates it
   through `print-config` (the real loader, so a bad file fails at boot with the
   loader's own message), injects secrets into the references it declares, and
   serves it. Nothing is derived, and `/var/lib/deep-obsidian-mcp/config.json` is
   not created. This is the path for a config you built with
   `deep-obsidian-mcp setup-service --wizard` on your laptop, and the only path for
   a multi-mount table.
2. **Otherwise the environment derives a config, once**, into
   `/var/lib/deep-obsidian-mcp/config.json`. That file is on the volume, so every
   later boot finds it and skips straight to secret injection: changing a `DO_*`
   variable after the first boot changes nothing. Deliberately — silently rewriting
   the config a service has been running is how an index gets orphaned. Set
   `DO_REBUILD_CONFIG=1` (or delete the file) to re-derive.

**The one exception: host and port.** Both are passed as CLI flags, which beat the
config file, so a config saying `127.0.0.1` — correct on a laptop, useless in a
container — cannot make the server unreachable, and the port cannot drift away from
the image's `HEALTHCHECK`. Override with `DO_HTTP_HOST` / `DO_HTTP_PORT`.

A mounted config that changes `healthPath` away from `/healthz` needs its
`HEALTHCHECK` overridden too (`--health-cmd` on `docker run`, `healthcheck:` in
compose). `mcpPath` is free to change: nothing in the image depends on it.

## Secrets are ephemeral

Four files, all optional, all read from `$DO_SECRETS_DIR` (`/run/secrets`, which is
where `secrets:` in compose mounts them):

| File | Goes to |
| --- | --- |
| `auth_token` | Exported as `DEEP_OBSIDIAN_AUTH_TOKEN` for the server process. Required unless `DO_INSECURE_NO_AUTH=1`. |
| `couchdb_password` | `secrets set --mount <id> --field password` on the config's couchdb mount. |
| `e2ee_passphrase` | `secrets set --mount <id> --field e2ee-passphrase`. |
| `algolia_api_key` | `secrets set --mount <id> --field api-key`. |

The mount is found by reading the config, so the same files work for a derived
config and for a mounted one. Only the first line of each file is used, with the
newline stripped; a blank file is an error, not "no secret".

**File permissions matter.** A bind-mounted secret (which is what compose's
`secrets: file:` is) keeps its host permissions, and the container runs as uid
`10001`: a `chmod 600` file owned by your host user is unreadable inside. Use `700`
on the directory and `644` on the files, or run the service as the owning uid with
compose's `user:`. An unreadable file is refused with a message that says exactly
this — it is never silently treated as absent.

**Nothing persists.** The encrypted secret store lives at
`$XDG_CONFIG_HOME/deep-obsidian-mcp/secrets.json`, and `XDG_CONFIG_HOME` points into
the container's home directory — *not* into the volume. The entrypoint deletes the
store and re-injects every secret on every boot. Three consequences worth stating:

- The volume holds an index and a config file, and never a credential. Losing it
  costs a reindex, not a rotation.
- The file store's honest weakness — its key is derived in the binary, not held by
  you; see *What the encrypted-file fallback actually protects against* in
  [CONFIGURATION.md](../CONFIGURATION.md) — stops mattering here, because the
  ciphertext exists only for the life of a container that already holds the
  plaintext in `/run/secrets`.
- A secret removed from the orchestrator fails **loudly** on the next boot instead of
  quietly continuing to work from a copy nobody remembers storing.

The bearer token is handled differently, and not by preference: `secrets set --target
auth-token` can only rotate an `auth.tokenRef` the config already declares, and the
one command that creates one (`setup-service --auth`) refuses a config with a mount
table. `DEEP_OBSIDIAN_AUTH_TOKEN` is the path the server documents for exactly this
case — it enables auth and overrides any configured reference. Because the
*entrypoint* exports it rather than the image or the compose file, it does not appear
in `docker inspect`; it is visible in `/proc/1/environ` inside the container, which is
the same trust boundary as the store itself.

**Auth is required.** With no `auth_token` file and no `DO_INSECURE_NO_AUTH=1`, the
container refuses to start:

```
[entrypoint] FATAL: no bearer token: mount a non-empty secret at /run/secrets/auth_token
(compose: 'secrets: [auth_token]'), or set DO_INSECURE_NO_AUTH=1 to serve without
authentication on purpose. Refusing to start: this container binds 0.0.0.0, and an
unauthenticated MCP endpoint on a routable address exposes the whole vault.
```

`DO_INSECURE_NO_AUTH=1` is honoured, loudly, and only when no token was supplied (a
token always wins over it). Use it for a container on a private network you control,
or for tests.

## Degraded until the first client sync

For a `couchdb` root, a freshly created database is **not** a LiveSync vault: it has
no `obsydian_livesync_version` document and no `_local/obsydian_livesync_milestone`.
Nothing in this project can invent them — only an Obsidian client with the LiveSync
plugin creates them, on its first sync. Until then:

- `/healthz` → **200**. The server is alive and its non-remote mounts serve.
- `/readyz` → **503**, naming the mount and the compatibility status
  (`unknown-schema`).
- Vault tools over that mount refuse with that reason rather than returning empty
  results.

This is the expected first-run state, not a misconfiguration. `unknown-schema` is
classified as recoverable-without-reconfiguration, so the sidecar supervisor re-runs
its handshake on a backoff and the mount readies itself minutes after the first
client sync — **no restart**. `scripts/docker-smoke-test.sh` proves exactly this
path: it boots against an empty database, asserts the 503, PUTs the two documents by
hand, and waits for `/readyz` to turn 200 on its own.

Pair the client and the server by pointing both at the same database:

| LiveSync plugin setting | Value |
| --- | --- |
| URI | the same CouchDB the container reaches (`http://localhost:5984` for the example compose) |
| Username / Password | `COUCHDB_USER` / the contents of `secrets/couchdb_password` |
| Database | `DO_COUCHDB_DATABASE` |
| End-to-end encryption | if on, set `DO_COUCHDB_E2EE=true` and mount `e2ee_passphrase` |

## TLS is the proxy's job

The container speaks plain HTTP and terminates no TLS. The example compose publishes
both ports on `127.0.0.1` only, which is the right default for a single host. To
expose the MCP endpoint, put a reverse proxy in front of it and let the proxy hold
the certificate:

```
client --TLS--> caddy/traefik/nginx --plain HTTP--> deep-obsidian:4100
```

Keep the bearer token on regardless: the proxy protects the transport, the token
decides who may talk to the vault. Two proxy details that matter here — pass the
`Authorization` header through untouched, and do not buffer request bodies
aggressively if you use vault uploads (`PUT /upload/{token}` streams).

## Running one-off commands

Any argument other than `serve` is run through the CLI after the same preparation
(config validated, secrets injected), with `--config` already pointing at the right
file:

```bash
docker compose run --rm deep-obsidian secrets check
docker compose run --rm deep-obsidian mounts list
docker compose run --rm deep-obsidian doctor --probe-remote
docker compose run --rm deep-obsidian print-config
```

`doctor --probe-remote` is the one to reach for first when a mount is degraded: it
resolves the credential, opens a read-only connection and reports what the remote
said.

## Volumes, uids and the vault

The image runs as `deepobsidian` (uid/gid `10001`) and the directories it owns are
`chown`ed at build time. Two cases behave differently:

- **A named volume** (`deep-obsidian-state:/var/lib/deep-obsidian-mcp`) inherits the
  image directory's ownership when it is first created, so it just works.
- **A bind mount** (`./state:/var/lib/deep-obsidian-mcp`) keeps the host's
  ownership. Either `chown -R 10001:10001 ./state` on the host, or run the service
  as your own uid with `user: "1000:1000"` in compose.

The vault bind mount can be **read-only** (`-v "$VAULT:/vault:ro"`): the index is
written to `/var/lib/deep-obsidian-mcp/index`, never into the vault. That is why the
entrypoint passes `--index-dir` explicitly — the default for a filesystem vault is
`<vault>/.deep-obsidian-mcp`, which would both fail on a read-only mount and put a
container's index into a directory Obsidian syncs.

## Building and testing locally

```bash
docker build -t deep-obsidian-mcp:dev .
scripts/docker-smoke-test.sh deep-obsidian-mcp:dev              # full run, needs to pull couchdb:3
scripts/docker-smoke-test.sh deep-obsidian-mcp:dev --no-couchdb # image + entrypoint contract only
```

The smoke test is the same script CI runs. It asserts, on the image, what
`scripts/linux-smoke-test.sh` asserts on the `.deb` — the binary runs, ripgrep is
present, and the sidecar bundle is discoverable **by the binary's own exe-relative
probe** from `/` at the image's prefix — plus the container-only contract: the auth
refusal, the insecure escape hatch, mounted-config precedence, an index on the volume
and no secret store on it, and the degraded-start/self-heal path above. That parity
job is the whole justification for maintaining a second build path beside the `.deb`.

The CI job (`docker` in `.github/workflows/ci.yml`) builds natively on
`ubuntu-24.04` and `ubuntu-24.04-arm` — a matrix rather than one buildx
`platforms:` list, because QEMU would make the Rust release build take tens of
minutes and because an image nobody can *run* is an image nobody has checked.

## What the image contains

Three stages, one runtime:

| Stage | Base | Produces |
| --- | --- | --- |
| `rust-builder` | `rust:1-bookworm` | `deep-obsidian-mcp`, `--release`, deps cooked in a cached layer before sources |
| `sidecar-builder` | `node:20-slim` | `sidecar/livesync-sidecar/dist/sidecar.mjs` via `npm ci && npm run build` |
| `runtime` | `node:20-slim` | the binary, the bundle, `ripgrep`, `curl`, the packaged skills/snippets/assets |

`node:20-slim` is the runtime base because the couchdb mount kind needs `node` for
the sidecar, and taking it from the official image beats adding a NodeSource apt
repository. Node 20 is the sidecar's declared floor.

The install prefix is `/opt/deep-obsidian-mcp`, and that is a contract rather than a
preference: `PACKAGED_BUNDLE_PREFIX` (`share/deep-obsidian-mcp`) is resolved against
every ancestor of the executable's directory, so
`/opt/deep-obsidian-mcp/bin/deep-obsidian-mcp` finds
`/opt/deep-obsidian-mcp/share/deep-obsidian-mcp/sidecar/livesync-sidecar/dist/sidecar.mjs`
— the same relative arrangement `/usr/bin` + `/usr/share` gives the `.deb` and
`<prefix>/bin` + `<prefix>/share` gives Homebrew. No environment variable points at
the bundle in any of the three channels.

## Known limitations

- **No published image yet.** The GHCR push step exists in `ci.yml` but is commented
  out, together with its tag policy (`vX.Y.Z` + `latest`, on tags only), pending the
  release decision. Build locally in the meantime — the example compose does, via
  `build: .`.
- **A remote ROOT mount's config is written by the entrypoint, not by the CLI.**
  `mounts add` cannot create the first mount of an empty config
  (`allow_empty_base: false`), and adding a remote at `mountAt ""` beside a
  filesystem root fails validation; the only code path that can is the first-init
  wizard, which requires a terminal. So the entrypoint emits that small JSON itself
  and hands it to `print-config` for validation by the real loader. A
  non-interactive way to declare the root mount (`mounts add --allow-empty-base`, or
  `setup-service --root <kind>`) would let the entrypoint collapse into two
  `mounts add` calls.
- **One remote mount per container, for secret injection.** The four secret
  filenames map onto whichever mount of that kind the config declares; a config with
  two couchdb mounts is refused with a message naming them, because there is no
  per-mount filename convention yet. Rotate those by hand with
  `docker compose run --rm deep-obsidian secrets set --mount <id>`.
- **CouchDB's own credentials come from the environment**, not from a file: the
  official image supports no `*_FILE` variant. The example compose generates `.env`
  from the secret file so there is still one source of truth.
