# Rust Migration Checklist

Date: 2026-04-01

## Phase 3: Prototype

- [x] Rust workspace exists.
- [x] Shared config model matches the Node contract for the prototype scope.
- [x] CLI accepts the service commands.
- [x] HTTP bootstrap starts successfully.
- [x] `GET /healthz` returns `ok`.
- [x] MCP endpoint responds.
- [x] `vault_info` is exposed.
- [x] Rust launch verifier passes.

## Phase 4: Parity

- [x] `read_file` works.
- [x] `read_chunk` works.
- [x] `find_files` works.
- [x] `grep_search` works.
- [x] `build_index` works.
- [x] `bm25_search` works.
- [x] `semantic_search` works.
- [x] `hybrid_search` works.
- [x] `related_notes` works.
- [x] `backlinks` works.
- [x] `graph_traverse` works.
- [x] `setup-service` writes config.
- [x] `doctor` checks the runtime correctly.
- [x] `probe` checks health and MCP.

## Cutover Gate

- [x] Black-box tests pass against the Rust entrypoint.
- [ ] Config migration is handled.
- [ ] Service operations are simpler than Node.
- [ ] Packaging is reproducible on supported platforms.
- [ ] Homebrew formula can switch without changing user workflow.

## Current Caveat

- [ ] Native Rust parity has replaced the TypeScript compatibility backend.
