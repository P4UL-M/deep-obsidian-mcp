# Changelog

All notable changes to deep-obsidian-mcp are documented here.

## v0.1.0-alpha.10 — 2026-06-12

### ⚠️ Breaking changes (MCP tool surface: 19 → 18 tools)

- **Removed `find_similar_notes`.** The editorial style/structure/tone/format
  similarity tool had no internal callers and overlapped conceptually with
  `related_notes` (subject similarity). For content-relevant neighbours use
  `related_notes` (by note path) or `hybrid_search` (by query).

## v0.1.0-alpha.9

A large release centred on a retrieval-pipeline overhaul, a security pass, and a
tighter MCP tool surface. Existing indexes rebuild automatically on first run.

### ⚠️ Breaking changes (MCP tool surface: 24 → 19 tools)

Six tools were removed or merged. Migrate as follows:

| Removed tool | Use instead |
|---|---|
| `write_file_to_vault` | `request_vault_upload` (binary/large) · `upsert_note` (markdown) |
| `bm25_search` | `hybrid_search` with `semanticWeight: 0` |
| `semantic_search` | `hybrid_search` with `bm25Weight: 0` |
| `list_folders` | `list_children` with `foldersOnly: true` |
| `backlinks` | `graph_traverse` with `direction: "incoming", depth: 1` |
| `read_chunk` | `read_file` with `startLine` / `endLine` |

- **New tools:** `request_vault_upload` (capability-token binary upload) and
  `search_artifacts` (semantic search over non-markdown artifacts).
- Artifact-scope semantic search (formerly `semantic_search` `scope:"artifacts"`)
  is now exposed via the dedicated `search_artifacts` tool.

### Security (issue #22 — resolved)

- **Fixed a verified RCE** in `grep_search`: a query beginning with `-`/`--`
  (e.g. `--pre=…`) was parsed by ripgrep as a flag, enabling arbitrary command
  execution. Queries and paths are now passed after a `--` end-of-options guard.
- **Fixed symlink vault-escape**: `ensure_inside_vault` now canonicalizes and
  verifies the target stays under the vault root (handles not-yet-existing write
  targets and symlinked vault roots).
- Upload-store lock sites recover from mutex poisoning instead of propagating a panic.

### Retrieval pipeline overhaul (issue #6 — resolved)

- **Heading-aware chunking** — section-based chunks that never split fenced code or
  tables; embedding text carries the heading path.
- **Reciprocal Rank Fusion** for hybrid search (scale-free, replaces weighted-sum).
- **Asymmetric query encoding** for instruction-tuned embedding models (qwen3).
- **Small-to-big retrieval** — match at chunk granularity, return the enclosing section.
- **Note-level dense vector dropped**; **`related_notes` reimplemented as late-interaction
  (max-sim)** over chunk vectors (semantic, no stored note vector).
- **Graph-aware re-rank** — lightly boost candidates one wikilink hop from top hits.
- **Deterministic retrieval-quality eval harness** + manual real-model protocol.

### Reliability

- **Embedding reindex robustness** — request timeout, per-batch partial progress
  (one failed note no longer aborts the reindex), partial-index load, `Sparse`
  downgrade + auto-recovery on total failure.
- **Query-time graceful degradation (#4-#3)** — `hybrid_search`/`load_knowledge`
  fall back to BM25 with a `degraded` flag when the embedding backend is down;
  `search_artifacts` returns a clear message; `vault_info` reports backend
  reachability non-fatally.
- **`grep_search` rg-or-disabled (#5 — resolved)** — resolve ripgrep at startup;
  hide the tool when unavailable; never the misleading `No such file (os error 2)`.

### Agent ergonomics (issue #4)

- `request_vault_upload` — binary files via an out-of-band capability-token URL (#4-#0).
- `read_file` conditional reads via `knownHash` (skip unchanged bodies) (#4-#2).
- Aggregate output caps with truncation-with-continuation on search tools (#4-#6).
- Descriptive required-argument errors + conditional `heading` schema for
  `update_note_section` (#4-#5).

### Migration notes

- **Index auto-migration:** the index schema version was bumped (v4 → v6); existing
  indexes fail the version check and rebuild automatically on first run — no manual
  action required.
- **`vault_info`** now performs a bounded (~3s) embedding-backend reachability probe
  on an embedding backend (previously pure-local), reported as `embeddingBackendStatus`.

### Internal

- Dead-code audit removing stale `#[allow(dead_code)]` and an unused chunker parameter.

### Known / not in this release

- Open enhancements (non-blocking): `update_note_section` batch edits / section-scoped
  hashing (#4-#4) and basename/fuzzy path resolution (#4-#5 basename).
- Out of scope: automatic restart of the external embedding (llama-server) process.
