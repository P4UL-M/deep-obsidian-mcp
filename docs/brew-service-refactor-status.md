# Brew Service Refactor Status

Date: 2026-04-01

## Current Step

The project has completed the Node service refactor, has partial Homebrew scaffolding in place, and now has a working Rust feasibility prototype for the service boundary.

## Completed

- Extracted the monolithic entrypoint into dedicated CLI, config, server, service, and command modules.
- Added a first-class JSON config model at `~/.config/deep-obsidian-mcp/config.json`.
- Implemented config precedence:
  1. CLI flags
  2. config file
  3. environment variables
  4. defaults
- Implemented service-oriented commands:
  - `setup-service`
  - `doctor`
  - `print-config`
  - `probe`
  - `serve`
- Extracted MCP server registration into `src/server.ts`.
- Updated the transitional shell wrappers to delegate to the new config-driven `serve` path.
- Added black-box verification assets and fixture vault data.
- Verified:
  - `npm run check`
  - `npm run verify:config`
  - `npm run verify:fixtures`
  - live HTTP launch and MCP probe
- Added a Rust workspace prototype with shared config/types, a service CLI surface, and a minimal HTTP/MCP server.
- Verified the Rust prototype with:
  - `cargo check --workspace`
  - `npm run verify:rust -- --launcher cargo`

## Partially Completed

- Homebrew packaging and documentation scaffolding:
  - `README.md`
  - `docs/homebrew-service.md`
  - `docs/release-checklist.md`
  - `Formula/deep-obsidian-mcp.rb`

These exist as scaffolding, but the packaged release artifact and full tap flow are not finished yet.
- Rust migration groundwork:
  - workspace and crate layout exist
  - config and transport modeling are shared
  - HTTP service bootstrap works
  - minimal MCP tool exposure is in place

This is still a prototype, not a cutover-ready replacement for the Node implementation.

## Not Started

- Real Homebrew release artifact and install layout
- Full `brew install` / `brew services start` validation from a tap
- Full Rust parity for indexing, search, and the broader MCP tool surface
- Rust cutover planning and config migration

## Recommended Next Step

Finish either Phase 2 packaging or move deeper into Rust Phase 4 parity. On the Rust track, the next concrete step is:

- implement `read_chunk`, `find_files`, and `grep_search`
- extend the verifier to cover those tools
- keep Node as the production path until parity and packaging are materially simpler

## Commit Reference

- `b3316b6` test: add brew service verification assets
- `aca9226` feat: add service refactor command modules
- `6469fce` feat: add config and cli foundation
- `d77d849` refactor: extract MCP server module
- `2130637` feat: wire config-driven service CLI
- `589b08b` test: harden service verification workflow
- `8e223e5` docs: add homebrew service scaffolding
- `085de3a` docs: add rust prototype verifier
- `d195d4a` feat: add rust workspace skeleton
- `cbdfae8` feat: add rust service prototype
