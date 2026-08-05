# docs/

Reference and developer/maintainer documentation. **New users should start with
the top-level guides instead:** [README](../README.md) ·
[INSTALL](../INSTALL.md) · [USAGE](../USAGE.md) · [CONFIGURATION](../CONFIGURATION.md).

## Reference

- [demo.md](./demo.md) — `scripts/demo-multi-backend.sh`: a one-command, runnable tour of the multi-backend architecture (filesystem + CouchDB + Algolia under one namespace).
- [mcp-reference.md](./mcp-reference.md) — MCP tools, resources, and prompts.
- [architecture.md](./architecture.md) — indexing model, semantic backends, roadmap.
- [algolia-mounts.md](./algolia-mounts.md) — the experimental shared Algolia corpus: design, versioning, CLI, security, limits.
- [agent-workflows.md](./agent-workflows.md) — agent workflow patterns.

## Service & packaging

- [homebrew-service.md](./homebrew-service.md) — full Homebrew service model and troubleshooting.
- [debian-package.md](./debian-package.md) — Debian/Ubuntu `.deb` and APT repository details.

## Maintainer / internal

- [behavior-contract.md](./behavior-contract.md) — server behavior contract, including the
  [multi-mount rules](./behavior-contract.md#multi-mount-vaults) a client can rely on.
- [retrieval-eval.md](./retrieval-eval.md) — retrieval evaluation harness.
- [release-checklist.md](./release-checklist.md) — release steps.
- [homebrew-gap-todo.md](./homebrew-gap-todo.md) — outstanding release-artifact gaps.
- [FIX_EMBEDDING_CONTEXT_CRASH.md](./FIX_EMBEDDING_CONTEXT_CRASH.md) — incident note.
