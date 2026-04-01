# Rust Prototype

Date: 2026-04-01

This document defines the current Rust feasibility prototype scope for `deep-obsidian-mcp`.

## Covers

- A Rust workspace skeleton that is intended to mirror the Node-side service boundary.
- Shared config and type modeling for the current service shape.
- A CLI surface that should eventually mirror:
  - `serve`
  - `setup-service`
  - `doctor`
  - `print-config`
  - `probe`
- A minimal HTTP service bootstrap with:
  - `GET /healthz`
  - an MCP HTTP endpoint
- Feasibility verification that starts the Rust binary and probes the live HTTP endpoint.

## Does Not Cover

- Full indexing parity.
- Full search parity.
- Full MCP tool parity.
- Graph traversal parity.
- Embedded vector search parity.
- Release packaging parity.
- Homebrew formula cutover.
- Config migration from Node to Rust.

## Prototype Rules

- The Rust prototype is a decision aid, not a rewrite commitment.
- The prototype should prove process shape, config loading, and service startup before search/indexing work.
- The Node implementation remains the production path until the Rust prototype proves it is simpler and safer to maintain.

## Acceptance Signal

The prototype is useful if it can:

1. Parse the current service config shape.
2. Start an HTTP service.
3. Serve `/healthz`.
4. Answer a minimal MCP probe.
5. Pass the Rust launch verifier.

If any of those are missing, the prototype is not yet ready to justify migration.

## Verification

Run the prototype verifier from the repository root:

```bash
npm run verify:rust -- --launcher cargo
```

If you already have a built binary, point the verifier at it:

```bash
npm run verify:rust -- --launcher binary --binary rust/target/debug/deep-obsidian-cli
```

The verifier intentionally uses the same HTTP probe pattern as the Node service checks so the prototype can be compared against the existing service behavior.
