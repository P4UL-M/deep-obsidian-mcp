# Behavior Contract

This document defines the service surface that must stay stable across the current Node implementation and any later Rust replacement.

## Scope

The contract covers:

- configuration resolution
- service startup and health probing
- MCP tool and resource availability
- vault-relative note reads and graph traversal
- fixture-based verification assets

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
- `read_file`
- `read_chunk`
- `find_files`
- `grep_search`
- `build_index`
- `bm25_search`
- `semantic_search`
- `hybrid_search`
- `related_notes`
- `backlinks`
- `graph_traverse`
- `upsert_session_note`

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

Expected fixture root:

- `tests/fixtures/vault`

## Verification

Preferred verification commands:

- `node scripts/verify-config-contract.mjs`
- `node scripts/verify-fixture-vault.mjs`
- `node scripts/verify-service-http.mjs --url http://127.0.0.1:4100/mcp --vault tests/fixtures/vault`
- `node scripts/verify-service-launch.mjs --vault tests/fixtures/vault --command node --entrypoint dist/index.js`

These scripts are intentionally framework-free so they can be reused during the Node-to-Rust migration.
