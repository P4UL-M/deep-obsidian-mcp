# Brew Service Refactor Status

Date: 2026-04-01

## Current Step

The project has completed the Node service refactor, has partial Homebrew scaffolding in place, and now has behavioral parity at the Rust entrypoint through a managed TypeScript compatibility backend.

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
- Added a Rust workspace and entrypoint with shared config/types, a service CLI surface, and compatibility-backed HTTP plus stdio parity.
- Verified the Rust entrypoint with:
  - `cargo check --workspace`
  - `npm run verify:rust -- --launcher cargo`
  - a stdio smoke check against `initialize` and `tools/list`

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
  - Rust is now the outer service/CLI shell
  - HTTP and stdio parity are preserved by delegating the MCP implementation to the existing TypeScript backend

This is behavioral parity, not native-Rust parity. The TypeScript implementation still provides the actual MCP logic behind the Rust entrypoint.

## Not Started

- Real Homebrew release artifact and install layout
- Full `brew install` / `brew services start` validation from a tap
- Native Rust parity for indexing, search, resources, and the broader MCP tool surface
- Removal of the TypeScript compatibility backend
- Rust cutover planning and config migration

## Recommended Next Step

Finish either Phase 2 packaging or move from compatibility parity to native-Rust parity. On the Rust track, the next concrete step is:

- replace the compatibility backend one capability cluster at a time
- start with file/path tools, then index/search, then graph/related-note features
- keep the parity verifier running against the Rust entrypoint as each TypeScript-backed path is retired

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
- `6358c21` feat: integrate rust workspace prototype
- `29156e1` docs: record rust prototype verification
