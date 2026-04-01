# Rust Migration Checklist

Date: 2026-04-01

## Phase 3: Prototype

- [ ] Rust workspace exists.
- [ ] Shared config model matches the Node contract.
- [ ] CLI accepts the service commands.
- [ ] HTTP bootstrap starts successfully.
- [ ] `GET /healthz` returns `ok`.
- [ ] MCP endpoint responds.
- [ ] `vault_info` is exposed.
- [ ] Rust launch verifier passes.

## Phase 4: Parity

- [ ] `read_file` works.
- [ ] `read_chunk` works.
- [ ] `find_files` works.
- [ ] `grep_search` works.
- [ ] `build_index` works.
- [ ] `bm25_search` works.
- [ ] `semantic_search` works.
- [ ] `hybrid_search` works.
- [ ] `related_notes` works.
- [ ] `backlinks` works.
- [ ] `graph_traverse` works.
- [ ] `setup-service` writes config.
- [ ] `doctor` checks the runtime correctly.
- [ ] `probe` checks health and MCP.

## Cutover Gate

- [ ] Black-box tests pass against Rust.
- [ ] Config migration is handled.
- [ ] Service operations are simpler than Node.
- [ ] Packaging is reproducible on supported platforms.
- [ ] Homebrew formula can switch without changing user workflow.
