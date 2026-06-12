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
- `find_similar_notes`
- `graph_traverse` (with `direction:"incoming"` for backlinks)
- `upsert_note`
- `update_note_section`
- `request_vault_upload`
- `upsert_session_note`

`upsert_note` must preserve explicit author control. If `content` is provided, it must be written as-is. If `title` or `frontmatter` are provided, they must only be written when explicitly requested.

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
- `cargo run -p deep-obsidian-cli --bin deep-obsidian-mcp -- doctor --vault tests/fixtures/vault`
- `cargo run -p deep-obsidian-cli --bin deep-obsidian-mcp -- print-config --vault tests/fixtures/vault`
- `cargo build --release -p deep-obsidian-cli --bin deep-obsidian-mcp`
- `codesign --force --sign - --timestamp=none target/release/deep-obsidian-mcp`
- `codesign --verify --verbose=2 target/release/deep-obsidian-mcp`

The maintained runtime path is Rust only.
