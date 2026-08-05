# Changelog

All notable changes to deep-obsidian-mcp are documented here.

## Unreleased

### Added

- **Multi-backend vaults (experimental, off by default).** A vault can now be
  composed of several **mounts**, each grafting other storage onto a folder of
  your vault while your agent keeps seeing one namespace. Nothing changes for a
  config with no `mounts` table: it resolves to exactly one filesystem mount at
  the root and every payload stays byte-identical to what it was. Every backend
  beyond the filesystem is behind its own `experimental` flag and is read-only
  until you opt in per mount.

  - **A `VaultBackend` boundary.** All vault IO — reads, listings, writes,
    uploads, grep, recall — goes through one trait, with the filesystem as its
    first implementation and a parameterized conformance suite every backend must
    pass. Indexing was generalized behind a `NoteSource` trait so a mount's index
    no longer assumes a directory.
  - **A multi-mount router.** Longest-prefix routing over `mountAt`: exactly one
    mount owns any path, each mount gets its own runtime and index in its own
    directory, and a write lands in the single mount that owns its path.
  - **CouchDB / Self-hosted LiveSync mounts** (`experimental.couchdbVaults`).
    Mount a LiveSync database as a folder of your vault. Served by a supervised
    Node **sidecar** speaking a versioned stdio protocol, pinned to the upstream
    library triple it was built against so a schema drift fails closed instead of
    reassembling notes wrong. Reads, listings, live change feed, revision-guarded
    writes, soft deletes, conflict enumeration, binary uploads, and export/restore.
    E2EE and path obfuscation supported. **`grep_search` works on a LiveSync
    mount**: with no files for ripgrep to open, the backend imitates ripgrep
    in-process over note text read back through the sidecar — same pattern
    semantics (`regex`/`fixedStrings`/`caseSensitive`, ripgrep's own glob engine),
    same match shape, same context lines. It is exhaustive in the same sense
    ripgrep is, so its payload carries no `exhaustive` key, and a differential test
    asserts its output is byte-identical to real ripgrep's over the same corpus.
    The cost is stated rather than hidden: one full corpus read per query, so
    narrow the `glob` or lower the `limit` on a large vault.
  - **Algolia mounts** (`experimental.algoliaVaults`). Mount a **shared,
    Markdown-only** corpus that a whole team can mount at once, with per-note
    version history, guarded fork-on-stale writes, divergence recording and
    resolution, retention, and a `note`/`chunk` record model. Ranking is served by
    Algolia itself rather than by a local index.
  - **Federated recall.** An unscoped `hybrid_search` / `load_knowledge` /
    `search_artifacts` asks every mount and fuses the answers with weighted
    reciprocal-rank fusion plus a final single-ranker rerank, so one mount's best
    hit can actually be compared with another's. Optional per-mount
    `recallWeight`; `federatedRerank: false` exposes the pure fusion order. Recall
    quality is gated against the single-index baseline by a dedicated eval suite.
  - **Capability-gated tools.** `note_history`, `read_version`,
    `resolve_divergence` and `delete_note` are advertised only when a mount can
    actually serve them, and refuse per-path by naming the mount, its backend, and
    the mounts that do support the operation. A tool that could only ever refuse is
    not advertised at all.
  - **Packaging and diagnostics.** The sidecar ships in the `.deb`, `doctor`
    reports per-mount status, and readiness degrades by mount name (`503`,
    `degradedMounts`) while the vault root keeps serving.

### Improved

- **Answers say what they could not do.** A federated answer that lost a mount
  carries `degraded`, `missingBackends` and a `degradationReason` naming it; a
  mount that legitimately holds nothing a tool asks for is reported as *skipped*
  and the answer is **not** degraded. A truncated folder listing sets
  `foldersTruncated`; a non-exhaustive grep sets `exhaustive: false`. Every one of
  these fields appears only in the case it describes, so a backend that cannot
  produce that case emits the payloads it always emitted. `docs/behavior-contract.md`
  now has a consolidated **Multi-mount vaults** section stating the rules a client
  can rely on.
- **Resilience is now tested end to end.** A sidecar child killed outright is
  restarted by the next call and an edit made while it was down still arrives, via
  a `changesSince` catch-up. A remote answering `500`s — or dropping sockets —
  fails reads honestly and recovers when it stops, without recycling the child. A
  mount unreadable at startup recovers when its vault appears, and an Algolia mount
  taken down mid-session refuses reads rather than serving its own cache, then
  serves again when the backend returns. Two limits are asserted rather than
  papered over: change cursors are not persisted across a server restart (a rebuilt
  backend replays from the beginning, so it misses nothing), and a CouchDB mount
  whose remote was unreachable at handshake time stays degraded until the child
  re-hand-shakes.
- **The black-box MCP surface is frozen** by a golden-based contract suite, so a
  single-mount vault's `tools/list` and payloads cannot drift while multi-mount
  work lands. Unknown config fields are retained rather than dropped on rewrite.

### Fixed

- **Secrets stored in the OS keyring actually persisted.** `keyring` was declared
  without its platform features, so every `put()` landed in the crate's in-memory
  mock store, reported success, and evaporated with the process — and the
  encrypted-file fallback never engaged, because the mock returns `Ok`. HTTP auth
  tokens and embedding API keys were affected. Platform backends are now enabled
  (with a vendored dbus, so the `.deb` gains no runtime dependency).
- **`setup-service --wizard` no longer replaces a config with no way back.** The
  auth prompt always marked the config changed, bypassing the overwrite guard, and
  its hardcoded default silently turned auth off on Enter. Content-changing
  overwrites now back the previous file up to `config.json.bak`, and every prompt is
  prefilled from the existing config.
- **`grep_search` no longer reports an in-vault index directory's SQLite files as
  notes.** A custom `indexDir` inside the vault surfaced as phantom matches under
  caller-supplied globs. They are now excluded before the match limit, so a phantom
  can never displace a real note.

## v0.1.0-alpha.12 — 2026-07-02

### Improved

- **Vault IO errors now carry the offending path and permission remediation
  hints**, and the server warns when the startup scan stalls (#34).

### Fixed

- **The `.deb` installs again on Debian 12 and Ubuntu 22.04.** The alpha.11
  package was built on Ubuntu 24.04 runners, so it declared
  `libc6 (>= 2.39)` and `apt install` failed with unmet dependencies on every
  older distro. Release builds now run inside a Debian bookworm container
  (floor drops to `libc6 (>= 2.34)`), CI fails if the floor ever rises above
  2.35, and a local Docker harness
  (`scripts/run-linux-integration-docker.sh`) reproduces the full
  install + smoke test on `debian:12`, `ubuntu:24.04`, and `ubuntu:22.04`.
- **`upsert_note` no longer fails clients that send both `content` and
  `body`.** Some tool-callers (e.g. Grok) fill every schema property on each
  call, and the two fields looked interchangeable, so every call died on the
  server's mutual-exclusion check. Identical text is now accepted (writing
  `content`, with a `warning` in the result); diverging text still errors. The
  tool description, the `content`/`body`/`title`/`frontmatter` descriptions,
  and a new `oneOf` in the input schema now state the exclusivity explicitly.
- **A failed tool call no longer kills the stdio server.** The stdio loop
  treated any request-level error (failed tool call, bad params, unknown
  method) as fatal and exited, so the first error made every subsequent call
  fail until restart. Errors are now sent to the client as JSON-RPC error
  responses and the server keeps serving, matching the HTTP transport.

## v0.1.0-alpha.11 — 2026-06-26

### Added

- **Debian/Ubuntu packaging (`apt`), amd64 + arm64.** The server now installs
  via `apt` alongside the Homebrew tap. Add the signed APT repository hosted on
  GitHub Pages and `apt install deep-obsidian-mcp`, or grab a single `.deb` from
  the release. It installs the binary to `/usr/bin`, packaged
  skills/snippets/assets to `/usr/share/deep-obsidian-mcp/`, and a systemd
  **user** unit to `/usr/lib/systemd/user/`
  (`systemctl --user enable --now deep-obsidian-mcp`). Built with `cargo-deb`
  (`scripts/build-deb.sh`); a `release-deb` GitHub Actions workflow builds both
  architectures natively, smoke-tests each package, validates the signed repo by
  installing from it, and on tags publishes the repo to Pages and attaches the
  `.deb`s to the release. See [docs/debian-package.md](./docs/debian-package.md).
- **Optional HTTP bearer authentication** for the HTTP transport (off by
  default). Enable via `setup-service --wizard` or `setup-service --auth`
  (generates, stores, and prints a token once); disable with
  `setup-service --no-auth`. `DEEP_OBSIDIAN_AUTH_TOKEN` provides a literal-token
  override. Protected routes (`/mcp`, `/upload`) require the token; health stays
  open. Includes Origin validation and a fail-closed guard that refuses to bind a
  non-loopback host without auth (`--insecure-no-auth` to override).

### Changed

- **Packaged index location is now platform-native.** On Linux, packaged-mode
  indexes live under `$XDG_DATA_HOME/deep-obsidian-mcp/indexes/` (default
  `~/.local/share/...`) instead of the macOS-only `Application Support` path.

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
