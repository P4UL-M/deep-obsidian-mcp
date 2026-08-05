# Configuring deep-obsidian-mcp

- [Config file & precedence](#config-file--precedence)
- [Semantic search (embeddings)](#semantic-search-embeddings)
- [Authentication](#authentication)
- [Automatic reindexing](#automatic-reindexing)
- [Transport & stdio modes](#transport--stdio-modes)
- [Multiple vaults (mounts)](#multiple-vaults-mounts) — **experimental**

## Config file & precedence

`setup-service` writes normalized JSON to
`~/.config/deep-obsidian-mcp/config.json` (override with `--config <path>`).
Inspect the resolved config any time with `deep-obsidian-mcp print-config`
(secrets are redacted).

Settings resolve in this order:

1. CLI flags
2. config file
3. environment variables
4. built-in defaults

### Secrets are references, never values

Secrets (embedding API keys, the auth token, a mount's CouchDB password or Algolia
API key) are **never** stored in the config file. The config only holds a
reference; the value lives in the OS keyring when available, or an encrypted local
file as a fallback. The encrypted-file fallback is weaker than the OS keyring
because the application carries the decryption key.

A reference is one of two shapes:

```json
{ "kind": "osKeyring", "service": "deep-obsidian-mcp", "account": "algolia-team-wiki" }
{ "kind": "encryptedFile", "id": "algolia-team-wiki" }
```

This is what makes `print-config` safe by construction rather than by careful
redaction: there is nothing secret in the persisted config to redact. Everything
else in a mount definition — a CouchDB URL and user name, an Algolia app id and
index name — is an identifier, not a credential, and is printed verbatim.

## Semantic search (embeddings)

The server has two semantic modes:

- **Sparse fallback** (default) — local term vectors, no external dependency.
- **Embedding-backed** — an OpenAI-compatible `/embeddings` endpoint, with
  similarity ranking executed through `sqlite-vec`.

Enable embeddings through the wizard (it also stores the API key securely):

```bash
deep-obsidian-mcp setup-service --wizard
```

Or configure them with flags / environment variables:

```bash
deep-obsidian-mcp serve --vault ~/Vault \
  --embedding-provider openai-compatible \
  --embedding-model nomic-embed-text \
  --embedding-base-url http://localhost:11434/v1
```

Environment variables (useful for the service wrapper and containers):

| Purpose | Variables (first match wins) |
|---|---|
| Provider | `DEEP_OBSIDIAN_EMBEDDING_PROVIDER`, `EMBEDDING_PROVIDER` |
| Model | `DEEP_OBSIDIAN_EMBEDDING_MODEL`, `EMBEDDING_MODEL`, `OPENAI_EMBEDDING_MODEL` |
| Base URL | `DEEP_OBSIDIAN_EMBEDDING_BASE_URL`, `EMBEDDING_BASE_URL`, `OPENAI_BASE_URL` |
| API key | `DEEP_OBSIDIAN_EMBEDDING_API_KEY`, `EMBEDDING_API_KEY`, `OPENAI_API_KEY` |

A blank API key is allowed for local OpenAI-compatible endpoints such as Ollama.

## Authentication

HTTP bearer authentication is **optional and disabled by default**, so loopback
(`127.0.0.1`) setups keep working unchanged. Enable it when you expose the
service beyond the local machine (binding `0.0.0.0` or fronting it with a
tunnel).

Enable it (generates a 256-bit token, stores it securely, prints it once):

```bash
deep-obsidian-mcp setup-service --wizard     # answer yes to authentication
deep-obsidian-mcp setup-service --vault ~/Vault --auth   # non-interactive
```

Disable it again (also deletes the stored token):

```bash
deep-obsidian-mcp setup-service --no-auth
```

Send the token from your client:

```
Authorization: Bearer <token>
```

When enabled, `POST /mcp` and `PUT /upload/{token}` require the token;
`/healthz` and `/readyz` stay open for liveness probes. A missing or invalid
token gets `401` with a `WWW-Authenticate: Bearer` challenge.

Two guards reduce the chance of accidental exposure:

- **Fail-closed bind** — the server refuses to start on a non-loopback host with
  auth disabled. Override deliberately with `--insecure-no-auth` or
  `DEEP_OBSIDIAN_ALLOW_INSECURE=1`.
- **Origin validation** — requests carrying a browser `Origin` header are
  rejected unless that origin is in `auth.allowedOrigins` (DNS-rebinding
  defence). Non-browser clients (Claude Code, curl) omit `Origin` and are
  unaffected.

For containers, tunnels, or headless hosts where the OS keyring is unavailable,
set `DEEP_OBSIDIAN_AUTH_TOKEN` to a literal token; it enables auth and overrides
any configured token reference.

## Automatic reindexing

The local index updates itself in the background: an initial build/check at
startup, debounced rebuilds after vault changes, and a periodic catch-up sync.
It's on by default. Tune or disable it:

```bash
deep-obsidian-mcp serve --vault ~/Vault \
  --auto-reindex true \
  --reindex-debounce-ms 1500 \
  --reindex-interval-ms 30000

deep-obsidian-mcp serve --vault ~/Vault --auto-reindex false
```

The `build_index` tool is still available for an explicit forced rebuild.

## Transport & stdio modes

The server speaks both the HTTP (Streamable) transport and stdio. For stdio
clients you can pin the framing:

```bash
deep-obsidian-mcp --vault ~/Vault --stdio-mode auto      # default
deep-obsidian-mcp --vault ~/Vault --stdio-mode framed
deep-obsidian-mcp --vault ~/Vault --stdio-mode newline
```

For the HTTP service, see [USAGE.md](./USAGE.md#3-run-it-as-a-service).

## Multiple vaults (mounts)

> **EXPERIMENTAL.** Everything in this section is behind `experimental` flags,
> defaults to off, and may change. A config with no `mounts` table behaves exactly
> as it always has.

A single vault needs nothing here: `vaultPath` alone still works and is still the
recommended shape. A `mounts` table lets you graft **other** storage onto folders of
that vault, so an agent sees one namespace while the content behind different
prefixes lives in different places.

A config sets **either** `vaultPath` **or** `mounts`, never both: both spell "where the
vault root is", and silently preferring one would let someone who added `mounts` to an
existing config keep serving the old vault with no signal at all. Migrating means moving
`vaultPath` into the root mount, as below.

```json
{
  "indexDir": "~/.local/share/deep-obsidian-mcp",
  "experimental": {
    "multiVault": true,
    "couchdbVaults": true,
    "algoliaVaults": true
  },
  "mounts": [
    {
      "id": "vault",
      "mountAt": "",
      "backend": { "kind": "filesystem", "vaultPath": "~/Vault" }
    },
    {
      "id": "phone",
      "mountAt": "LiveSync",
      "backend": {
        "kind": "couchdb",
        "url": "https://couch.example",
        "database": "obsidian",
        "username": "obsidian",
        "passwordRef": { "kind": "osKeyring", "service": "deep-obsidian-mcp", "account": "livesync-phone" },
        "writable": false
      }
    },
    {
      "id": "team-wiki",
      "mountAt": "_Wiki",
      "backend": {
        "kind": "algolia",
        "appId": "ABC1234XYZ",
        "indexName": "team-wiki",
        "apiKeyRef": { "kind": "osKeyring", "service": "deep-obsidian-mcp", "account": "algolia-team-wiki" },
        "writable": false
      }
    }
  ]
}
```

### The table

| Field | Meaning |
|---|---|
| `id` | Stable, user-chosen slug. Appears in error messages, in `vault_info.mounts[]`, and as `--mount <id>` on the CLI. |
| `mountAt` | The logical vault-relative folder this mount appears at. `""` is the vault root. Stored without leading or trailing slashes. |
| `backend` | One of the three kinds below, discriminated by `kind`. |

Three rules the config validator enforces:

- **`vaultPath` and `mounts` are mutually exclusive** (an empty `mounts` array counts as
  absent). See above.
- **Exactly one mount has `mountAt: ""`,** and it must be a `filesystem` one. A
  CouchDB or Algolia mount cannot be the vault root — the root is what stays serving
  when a remote mount is unreachable.
- **Ids and prefixes are unique.** Routing is longest-prefix, so `_Wiki` and
  `_Wiki/Decisions` can both be mounts and the more specific one wins; two mounts at
  the same prefix are rejected.

### Backend kinds

**`filesystem`** — a vault rooted at a local directory. The only kind that may be the
root mount, the only one that stores binary attachments, and the only one with no
experimental gate.

| Field | Meaning |
|---|---|
| `vaultPath` | The directory. Accepts `~`. |
| `indexDir` | Where this mount's search index lives. |

**`couchdb`** — a Self-hosted LiveSync vault in CouchDB, reached through the
supervised Node sidecar. Needs `experimental.couchdbVaults`. See
[docs/homebrew-service.md](./docs/homebrew-service.md) and
`deep-obsidian-mcp couchdb export --help`.

| Field | Meaning |
|---|---|
| `url` | Server origin **without** the database path. Validated to carry no userinfo, because it is printed verbatim by `doctor` and `print-config`. |
| `database` | The LiveSync database name. |
| `username` | A CouchDB user name — an identifier, not a credential, so it is plaintext. |
| `passwordRef` | Secret reference to the password. |
| `e2ee` | `{ "passphraseRef": …, "obfuscatePassphraseRef": … }` when the vault is encrypted or path-obfuscated. |
| `sidecarPath` | Explicit path to the built sidecar bundle. |
| `options` | Chunking / hashing knobs forwarded to the sidecar. **Must match how the vault was written.** |
| `writable` | **Defaults to `false`.** Setting it is what makes the sidecar initialize read-write; nothing else unlocks a write. |
| `indexDir` | Defaults to `<root indexDir>/mounts/<id>`. |

**`algolia`** — a shared, **Markdown-only** corpus stored as records in an Algolia
index, which several participants can mount at once. Needs
`experimental.algoliaVaults`. Full documentation, including the binary exception and
the security model, is in
[docs/algolia-mounts.md](./docs/algolia-mounts.md).

| Field | Meaning |
|---|---|
| `appId` | Algolia application id. Not a credential. |
| `indexName` | The main index. Its `_history` sibling is derived from this name. |
| `apiKeyRef` | Secret reference to the API key. `$DEEP_OBSIDIAN_ALGOLIA_API_KEY` shadows it, with a `warn` when it does. |
| `baseUrl` | Override the REST endpoint. Must carry no userinfo. |
| `writable` | **Defaults to `false`.** |
| `participantId` | Who you are in the corpus's audit trail; lands on every record you write. Defaults to `<user>@unknown`. |
| `cache` | `{ "maxBytes": …, "pinnedPrefixes": [ … ] }` for the local hydrated-note cache. |
| `retention` | `{ "minVersions": 5, "maxAgeDays": 90 }` — how much version history to keep. |
| `indexDir` | Holds the **cache only**; there is no local search index for this mount. |

### `writable` is per mount, on purpose

The experimental flag answers *may this build talk to this storage at all* — a
question about the backend's maturity, with the same answer for every mount.
`writable` answers *may the agent edit **this** vault* — a question about one vault's
role. A read-only archive and a writable scratch vault in one table is a reasonable
thing to want, and a single global flag could not express it.

Both default to off, so every pre-existing config keeps exactly the behaviour it has
today.

### `indexDir` per mount

Each mount gets its own index directory, defaulting to `<root indexDir>/mounts/<id>`
— keyed by mount id so two mounts cannot collide. The **root** mount uses the
top-level `indexDir`, which is what keeps a single-mount config's index exactly where
it has always been.

An Algolia mount is the exception: it has no local search index (the remote index *is*
the corpus), so its directory holds only the hydrated-note cache. The derivation is
the same regardless, so there is one rule to remember rather than one per backend.

### What multi-mount changes for a client

- `vault_info.mounts[]` lists each mount's id, prefix, backend kind, capabilities, and
  conflicted paths.
- The recall tools that rank (`hybrid_search`, `load_knowledge`, `search_artifacts`)
  gain a **required** `scope` argument, because ranking across storages with
  incomparable scores is not something to do silently. A single-mount vault's tool
  surface is unchanged.
- Tools whose whole purpose depends on a capability appear only when some mount has
  it. See
  [docs/mcp-reference.md](./docs/mcp-reference.md#conditionally-advertised-tools).
