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
| `recallWeight` | Optional. This mount's weight in **federated** recall's fusion stage (see below). Defaults to `1.0`. Must be a finite number greater than `0`. |

Three rules the config validator enforces:

- **`vaultPath` and `mounts` are mutually exclusive** (an empty `mounts` array counts as
  absent). See above.
- **Exactly one mount has `mountAt: ""`,** and it must be a `filesystem` one. A
  CouchDB or Algolia mount cannot be the vault root — the root is what stays serving
  when a remote mount is unreachable.
- **Ids and prefixes are unique.** Routing is longest-prefix, so `_Wiki` and
  `_Wiki/Decisions` can both be mounts and the more specific one wins; two mounts at
  the same prefix are rejected.
- **`recallWeight` is finite and positive.** Zero would remove the mount from every
  federated ranking while `vault_info` still reported it healthy, and a negative weight
  would order that mount's hits worst-first. Both are silent wrong answers, so the config
  is rejected instead of clamped.

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

### Federated recall: `recallWeight` and `federatedRerank`

| Key | Where | Meaning |
|---|---|---|
| `recallWeight` | per mount | That mount's weight in the fusion stage. Default `1.0`. |
| `federatedRerank` | top level | Whether the final rerank runs. Default `true`. Config-file only — there is no CLI flag or env var, because a per-invocation override of a retrieval-quality setting would make two runs of the same server rank differently with nothing in the config to explain it. |


On a multi-mount vault the ranking tools (`hybrid_search`, `load_knowledge`,
`search_artifacts`) answer an **unscoped** call by searching every mount and fusing the
results into one ranking. Pass `scope` naming a mount root to search that mount alone
instead.

It runs in two stages.

**1. Fusion** picks which candidates are worth looking at, by weighted Reciprocal Rank Fusion
over RANKS rather than scores: a candidate the mount ranked `r`-th (0-based) contributes
`recallWeight / (60 + r)`. Ranks are used because two independently built indexes produce
scores on incomparable scales, and averaging them would invent a comparison that does not
exist.

`recallWeight` is the only cross-mount preference available, and it applies to this stage. It
does **not** change a scoped search — a single backend's own ordering is unaffected by a
constant — it only decides which mount wins when two mounts each offer an equally-ranked hit.
Raise it above `1.0` for a mount you consider more authoritative; leave it unset for the
default.

**2. Rerank** decides the order, by rescoring the fused candidates against the query with a
scorer that knows nothing about mounts: cosine of one query vector against each candidate's
own stored chunk vector, fused with BM25 computed over the candidate set as its own corpus.
Both halves are computed by the server, over content it already holds to render the response;
no mount is ever asked to score another mount's content.

The rerank is not a refinement, it is what makes the answer ranked. Fusion alone cannot order
across mounts: every mount's best hit contributes the identical `weight / (60 + 0)`, and since
logical paths are namespaced no candidate ever appears in two mounts' lists, so nothing is ever
summed and the order collapses into a rank-for-rank interleave broken by mount id. An answer
then sits at the position its own mount happens to hold. Measured on a fixed 12-query eval over
one corpus split two and three ways, MRR against a single-vault baseline of 0.958:

| | recall@20 | recall@50 | MRR | nDCG@20 |
|---|---|---|---|---|
| one vault (baseline) | 1.000 | 1.000 | 0.958 | 0.969 |
| federated, fusion only | 1.000 | 1.000 | 0.600 / 0.469 | 0.703 / 0.601 |
| federated, with rerank | 1.000 | 1.000 | **0.958** | **0.969** |

Recall is identical in every row: fusion always retrieved the right notes, only their order was
wrong. Set `federatedRerank: false` to get the fusion-only ordering — useful to reproduce a
pure-fusion ranking, or to keep a federated query from issuing any query-time embedding — at
the quality shown above.

The response says what happened, so a partial answer is never presented as a complete one:

| Field | Meaning |
|---|---|
| `federated` | `true` on a fused answer. Absent on a scoped or single-mount one. |
| `mounts[]` | One entry per mount: `id`, `mountAt`, `source` (`local-index` or `native-recall`), `recallWeight`, `candidateCount`, `exhausted`, and `recallMode` for a mount that ranked for itself. |
| `mounts[].skipped` | The mount could never have contributed — an Algolia mount holds no binary files and therefore no artifacts. Not a shortfall, and it does not set `degraded`. |
| `mounts[].error` | The mount could have contributed and could not be reached. |
| `degraded` | `true` when any mount errored, or any mount's own retrieval fell back to lexical-only. Always present. |
| `missingBackends` | The mount ids behind a `degraded: true`. Emitted only when non-empty. |
| `candidateBudgetReached` | The search stopped on its work budget. Together with `degraded` it means a better hit may exist on a mount that was not read further. |
| `matches[].mountId` | Which mount produced this hit. |
| `rerank` | `"semantic+lexical"` or `"none"`. Which stage produced the order. |
| `rerankedCandidates` | How many candidates carried a semantic score, when reranked. |
| `matches[].score` | The number that produced THIS order — the rerank score when reranked, the fused score otherwise. |
| `matches[].rrfScore` | The fusion-stage score, kept so the ordering stays explainable. |

`rerank: "none"` with `degraded: false` means this deployment has no embedding backend
configured, so there was nothing to rerank with and nothing was lost. `rerank: "none"` with
`degraded: true` means the query could not be embedded and the answer fell back to fusion order.

Two tools stay per mount rather than federating, and both are deliberate:
`related_notes` and `graph_traverse` answer from the mount owning the `path` they are
given, because a wiki link from a note on one mount to a note on another is not an edge in
either index. `recommend_folder` refuses a multi-mount vault outright: it scores folders by
how much of the query's evidence sits under each one, and those counts are comparable only
within a single index.

### What multi-mount changes for a client

- `vault_info.mounts[]` lists each mount's id, prefix, backend kind, capabilities, and
  conflicted paths.
- The recall tools that rank (`hybrid_search`, `load_knowledge`, `search_artifacts`)
  gain an **optional** `scope` argument: omit it for the federated whole-vault answer,
  pass it to search one mount natively. A single-mount vault's tool surface is unchanged.
- `grep_search` and `find_files` search every mount too. Both produce matches or an
  enumeration rather than a ranking, so merging them needs no fusion. How a mount serves
  line search differs by backend — a filesystem mount spawns `rg`, a CouchDB mount runs an
  exhaustive virtual scan that costs a full corpus read per query, and an Algolia mount is
  candidate-bounded and says so with `exhaustive: false`. See
  [docs/behavior-contract.md](./docs/behavior-contract.md#line-search-per-backend).
- Tools whose whole purpose depends on a capability appear only when some mount has
  it. See
  [docs/mcp-reference.md](./docs/mcp-reference.md#conditionally-advertised-tools).
