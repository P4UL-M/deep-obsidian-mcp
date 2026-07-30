# Behavior Contract

This document defines the maintained Rust service surface.

## Scope

The contract covers:

- configuration resolution
- service startup and health probing
- MCP tool and resource availability
- vault-relative note reads and graph traversal
- fixture-based verification assets used by Rust tests

It does not define internal module layout.

## Configuration

Canonical config shape:

- `vaultPath`
- `indexDir`
- `transport`
- `stdioMode`
- `http.host`
- `http.port`
- `http.mcpPath`
- `http.healthPath`
- `autoReindex.enabled`
- `autoReindex.debounceMs`
- `autoReindex.intervalMs`
- `embedding.provider`
- `embedding.model`
- `embedding.baseUrl`
- `embedding.apiKeyRef`
- `shared[]` (optional) — `mountAt`, `appId`, `indexName`, `keyRef`, `baseUrl`,
  `writable`, `participantId`, `cache.maxBytes`, `cache.pin[]`,
  `retention.minVersions`, `retention.maxAgeDays`

Resolution precedence:

1. CLI flags
2. config file
3. environment variables
4. defaults

Rules:

- `vaultPath` is required before service startup.
- `transport` must default to `stdio` for subprocess use and `http` for service wrappers.
- `http.mcpPath` and `http.healthPath` must normalize to leading-slash paths.
- `embedding.apiKeyRef` stores a reference to a secret, not the secret itself.
- `doctor`, `print-config`, `probe`, and readiness output must never print resolved secret values.
- Encrypted local secret storage prevents accidental plaintext exposure in config files. For stronger local protection, use the OS keyring provider. The encrypted-file fallback is not equivalent to OS keyring storage because the application carries the decryption key.
- `setup-service` and packaged mode choose an index directory outside the vault when no index directory is explicitly resolved; explicit CLI, config file, or environment values must be preserved.
- Packaged mode is opt-in through `--packaged` or `DEEP_OBSIDIAN_PACKAGED=1`; ad-hoc dev commands without that opt-in keep the vault-local default index directory for compatibility.
- `shared[]` is optional and absent by default. A config without it must behave exactly as before shared mounts existed.
- `shared[].keyRef` stores a reference to the Algolia API key, never the key. `DEEP_OBSIDIAN_ALGOLIA_API_KEY` overrides it for containers and tests.
- Writing a config must not silently drop a `shared[]` block it does not understand. (A binary predating the field will do so; that is a release-ordering constraint, not permitted behaviour.)

## Service Contract

The service must expose:

- an MCP endpoint over HTTP
- a health endpoint
- stdio MCP support for subprocess use

Health responses should include enough metadata to diagnose startup and indexing issues:

- `status`
- `vaultPath`
- `generatedAt` or equivalent index timestamp
- `semanticBackend`
- `autoReindex`

The health endpoint must be lightweight and read-only. It must not trigger index refresh, rebuild, embedding calls, or filesystem mutation.

Readiness is distinct from health. The service should expose a readiness endpoint, currently `/readyz`, that reports whether the index is usable, still loading, or degraded. Packaging and service managers should use readiness, not health alone, as the MCP usability gate.

## Agent Workflows

The MCP API may expose additive prompts for common Obsidian workflows. These prompts must not replace or rename existing tools. They should guide clients toward safe tool use: narrow retrieval, outline-first inspection, graph-aware context, dry-run for broad writes, and hash guards for existing-note updates.

Packaged skill templates are documentation-like agent instructions, not runtime configuration. Installing or omitting them must not change the server's tool behavior.

`doctor` should also report non-secret local diagnostics, including config source attribution, auto-reindex settings, MCP/health/readiness endpoint URLs, index SQLite path and size, index schema metadata when available, and the latest health/readiness payload when service endpoints respond.

The service should fail fast when required config is missing or the vault cannot be read.

## MCP Contract

The black-box surface must preserve:

- `vault_info`
- `load_knowledge`
- `recommend_folder`
- `list_children` (with `foldersOnly` flag for subfolder-only listing)
- `read_file`
- `find_files`
- `grep_search`
- `build_index`
- `hybrid_search` (with `bm25Weight`/`semanticWeight` flags for BM25-only or semantic-only ranking)
- `related_notes`
- `graph_traverse` (with `direction:"incoming"` for backlinks)
- `upsert_note`
- `update_note_section`
- `request_vault_upload`
- `upsert_session_note`

Tool availability is capability-gated, not unconditional. A tool is advertised
only when the running configuration can honour it:

- `grep_search` requires a resolved ripgrep binary.
- `delete_note`, `note_history`, `read_version`, and `resolve_divergence` require
  at least one connected shared mount, and must be absent otherwise.

Names and input schemas of the preserved tools above never change; only their
presence is conditional.

### Shared-mount behaviour

When `shared[]` is configured, a mounted prefix routes to the shared index while
every other path stays local (longest-prefix wins; the mount root matches with or
without its trailing slash). Required properties:

- Reads of a mounted path hydrate from the index and must reproduce the stored
  note byte-for-byte.
- Writes to a mounted path are append-only versions. A write must never destroy
  the previous version: it is superseded into a history index and stays readable
  via `note_history` / `read_version`. A write based on a superseded version must
  still succeed, recording `forkedFrom` and `hasDivergence` rather than being
  rejected or silently overwriting.
- `delete_note` is a soft delete: the note leaves every listing and search, its
  content stays recoverable, and it is restored by writing the note again. It
  must refuse local paths — the MCP surface exposes no local file deletion.
- Permanent removal of a note and its history is a CLI operation (`share
  retract`), never an MCP tool.
- `mount.writable: false` must reject writes before any network call.
- Retrieval over a mount must report its scope honestly: `hybrid_search` and
  `load_knowledge` expose the mount's `recallStage`, `grep_search` reports
  `exhaustive: false` with the prefilter used and refuses patterns with no
  literal anchor, and `list_children` reports `foldersTruncated` when facet
  enumeration hits Algolia's cap.
- A path a scoped key may not read must be indistinguishable from a path that
  does not exist, so a key holder cannot enumerate what is hidden from them.
- `count` in a retrieval result always describes the returned list, including
  federated mount hits.

`upsert_note` must preserve explicit author control. If `content` is provided, it must be written as-is. If `title` or `frontmatter` are provided, they must only be written when explicitly requested. `content` and the compose fields (`body`/`title`/`frontmatter`) are mutually exclusive; as a robustness concession to clients that fill every schema property, a call providing both `content` and `body` with identical text succeeds (writing `content`, with a `warning` in the result), while diverging text is rejected.

`upsert_session_note` must preserve the provided markdown body as-is, except for optional trailing `## Manual Notes` preservation when requested. It must not inject an implicit title or heading.

Resources must preserve:

- `obsidian://vault/info`
- `obsidian://note?path=...`
- `obsidian://heading?path=...&slug=...`
- `obsidian://block?path=...&id=...`

## Fixture Vault Contract

Verification scripts use a tiny fixture vault with these invariants:

- the vault contains a small graph of linked notes
- at least one note has a direct wiki-link path to another note
- the fixture names are stable and predictable
- the fixture is readable without any user-specific configuration

Expected fixture root for CLI and integration tests:

- `tests/fixtures/vault`

## Verification

Preferred verification commands:

- `cargo test --workspace`
- Live shared-mount checks (ignored by default, env-gated; they talk to a real
  Algolia account and must be run against a scratch index):
  `cargo test -p deep-obsidian-server --test shared_concurrency_live -- --ignored`
  and `--test shared_secured_key_live -- --ignored`
- `cargo run -p deep-obsidian-cli --bin deep-obsidian-mcp -- doctor --vault tests/fixtures/vault`
- `cargo run -p deep-obsidian-cli --bin deep-obsidian-mcp -- print-config --vault tests/fixtures/vault`
- `cargo build --release -p deep-obsidian-cli --bin deep-obsidian-mcp`
- `codesign --force --sign - --timestamp=none target/release/deep-obsidian-mcp`
- `codesign --verify --verbose=2 target/release/deep-obsidian-mcp`

The maintained runtime path is Rust only.
