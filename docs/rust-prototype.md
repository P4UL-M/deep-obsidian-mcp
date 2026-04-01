# Rust Prototype

Date: 2026-04-01

This document defines the current Rust feasibility and compatibility scope for `deep-obsidian-mcp`.

## Covers

- A Rust workspace skeleton that is intended to mirror the Node-side service boundary.
- Shared config and type modeling for the current service shape.
- A CLI surface that mirrors:
  - `serve`
  - `setup-service`
  - `doctor`
  - `print-config`
  - `probe`
- A Rust HTTP compatibility proxy that:
  - serves `GET /healthz`
  - serves an MCP HTTP endpoint
  - launches the TypeScript implementation as an internal compatibility backend
  - preserves the full TypeScript MCP tool and resource surface through the Rust entrypoint
- A Rust stdio compatibility path that delegates stdio transport to the TypeScript implementation.
- Verification that starts the Rust binary and probes the live HTTP endpoint plus MCP resources.

## Does Not Cover

- Native Rust implementations of the full indexing and search stack.
- Native Rust implementations of graph traversal and embedding-backed retrieval.
- Release packaging parity.
- Homebrew formula cutover.
- Config migration from Node to Rust.

## Prototype Rules

- The Rust entrypoint now provides behavioral parity by running the TypeScript implementation as a managed compatibility backend.
- This is still not a native-Rust parity result; the search/indexing logic still lives in the TypeScript sidecar.
- The Node implementation remains the production logic path until the compatibility backend is progressively replaced or the project explicitly accepts that architecture.

## Acceptance Signal

The prototype is useful if it can:

1. Parse the current service config shape.
2. Start an HTTP service.
3. Serve `/healthz`.
4. Preserve the current MCP tool and resource surface through the Rust entrypoint.
5. Support stdio compatibility mode.
6. Pass the Rust launch verifier.

If any of those are missing, the prototype is not yet ready to justify migration.

## Verification

Run the prototype verifier from the repository root:

```bash
npm run verify:rust -- --launcher cargo
```

If you already have a built binary, point the verifier at it:

```bash
npm run verify:rust -- --launcher binary --binary rust/target/debug/deep-obsidian-mcp
```

The verifier intentionally uses the same HTTP probe pattern as the Node service checks so the prototype can be compared against the existing service behavior.
