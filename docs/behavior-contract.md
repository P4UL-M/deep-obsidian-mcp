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
- `embedding.apiKeyEnv`

Resolution precedence:

1. CLI flags
2. config file
3. environment variables
4. defaults

Rules:

- `vaultPath` is required before service startup.
- `transport` must default to `stdio` for subprocess use and `http` for service wrappers.
- `http.mcpPath` and `http.healthPath` must normalize to leading-slash paths.
- `embedding.apiKeyEnv` stores the environment variable name, not the secret itself.
- legacy environment names remain acceptable where they already exist, but the normalized config must be the single runtime input.

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

The service should fail fast when required config is missing or the vault cannot be read.

## MCP Contract

The black-box surface must preserve:

- `vault_info`
- `load_knowledge`
- `recommend_folder`
- `list_children`
- `list_folders`
- `read_file`
- `read_chunk`
- `find_files`
- `grep_search`
- `build_index`
- `bm25_search`
- `semantic_search`
- `hybrid_search`
- `related_notes`
- `find_similar_notes`
- `backlinks`
- `graph_traverse`
- `upsert_note`
- `update_note_section`
- `write_file_to_vault`
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

The maintained runtime path is Rust only.
