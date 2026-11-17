# Documentation

Start with the guide for the task you are doing. CLI help stays short; use
`deep-obsidian-cli <command> --help` for the options of one command.

## Get started

| Task | Guide |
|---|---|
| Understand the project | [README](../README.md) |
| Install or update | [Installation](../INSTALL.md) |
| Set up a vault, connect an agent and run the service | [Usage](../USAGE.md) |

## Operate a backend

| Task | Guide |
|---|---|
| Choose a local/Docker backend and find a command | [CLI guide](cli.md) |
| Recover a token, replace credentials or reconnect | [Authentication](authentication.md) |
| Manage private/shared vault access | [Multiuser](multiuser.md) |
| Change embeddings, indexing, mounts and server settings | [Configuration reference](../CONFIGURATION.md) |
| Run a Docker deployment | [Docker](docker.md) |
| Configure the Homebrew service | [Homebrew service](homebrew-service.md) |
| Work with Debian packages and APT | [Debian packaging](debian-package.md) |
| Export/restore a mount or prepare rollback | [Migration and rollback](migration-and-rollback.md) |

## Reference

- [OAuth protocol](oauth.md) — consent, client authorization and refresh behavior.

- [demo.md](./demo.md) — `scripts/demo-multi-backend.sh`: a one-command, runnable tour of the multi-backend architecture (filesystem + CouchDB + Algolia under one namespace).
- [mcp-reference.md](./mcp-reference.md) — MCP tools, resources, and prompts.
- [architecture.md](./architecture.md) — indexing model, semantic backends, roadmap.
- [algolia-mounts.md](./algolia-mounts.md) — the experimental shared Algolia corpus: design, versioning, CLI, security, limits.
- [agent-workflows.md](./agent-workflows.md) — agent workflow patterns.

## Maintainer / internal

- [behavior-contract.md](./behavior-contract.md) — server behavior contract, including the
  [multi-mount rules](./behavior-contract.md#multi-mount-vaults) a client can rely on.
- [retrieval-eval.md](./retrieval-eval.md) — retrieval evaluation harness.
- [release-checklist.md](./release-checklist.md) — release steps, and where the version lives.
- [stack-merge-procedure.md](./stack-merge-procedure.md) — landing a long linear PR stack on `main` before a tag.
- [homebrew-gap-todo.md](./homebrew-gap-todo.md) — outstanding release-artifact gaps.
- [FIX_EMBEDDING_CONTEXT_CRASH.md](./FIX_EMBEDDING_CONTEXT_CRASH.md) — incident note.
