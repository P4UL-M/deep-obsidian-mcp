# deep-obsidian-mcp

A filesystem-first [MCP](https://modelcontextprotocol.io) server that gives AI
agents deep, indexed access to an [Obsidian](https://obsidian.md) vault.

Point it at your vault and your agent can search, explore, and write notes —
backed by a local index that stays fresh automatically. It runs entirely on your
machine; semantic search works offline, and embeddings are optional.

## Features

- **Search every way** — by filename, by content (ripgrep), by meaning
  (semantic), or hybrid lexical + semantic ranking.
- **Explore** — find related notes by subject, and traverse wiki-links and
  backlinks as a graph.
- **Write** — create and update notes, patch a single heading section, and
  upload binary/large files.
- **Fast & local** — a persistent SQLite index, kept fresh in the background; no
  external service required (embeddings optional via any OpenAI-compatible API).
- **Flexible transport** — run as a stdio subprocess or a long-lived HTTP
  service, with optional bearer authentication.
- **Packaged** — install on macOS (Homebrew) or Debian/Ubuntu (apt).

## Install

**macOS (Homebrew):**

```bash
brew tap P4UL-M/tap
brew install deep-obsidian-mcp
```

**Debian / Ubuntu (apt)** — amd64 & arm64:

```bash
curl -fsSL https://p4ul-m.github.io/deep-obsidian-mcp/install.sh | sudo bash
```

**From source:** `cargo build --release -p deep-obsidian-cli --bin deep-obsidian-mcp`

Full install options, manual apt steps, updating, and uninstalling are in
[INSTALL.md](./INSTALL.md).

## Quick start

```bash
# 1. Point it at your vault (and connect local agents)
deep-obsidian-mcp setup-service --wizard

# 2. Run it as a background service
brew services start deep-obsidian-mcp               # macOS
# systemctl --user enable --now deep-obsidian-mcp   # Linux (apt)

# 3. Confirm it's healthy
deep-obsidian-mcp doctor
curl -fsS http://127.0.0.1:4100/readyz
```

Then connect your MCP client to `http://127.0.0.1:4100/mcp`. The step-by-step
walkthrough — setting up your vault, connecting Claude Code / Codex, running the
service, and troubleshooting — is in [USAGE.md](./USAGE.md).

## Configuration

Optional features and tuning live in [CONFIGURATION.md](./CONFIGURATION.md):

- **Semantic search** via an OpenAI-compatible embeddings endpoint
- **Authentication** (bearer token) for non-loopback / tunnelled deployments
- **Automatic reindexing** behaviour

## Documentation

| Guide | What's in it |
|---|---|
| [INSTALL.md](./INSTALL.md) | Install on macOS, Debian/Ubuntu, or from source |
| [USAGE.md](./USAGE.md) | Set up your vault, connect an agent, run the service, troubleshoot |
| [CONFIGURATION.md](./CONFIGURATION.md) | Embeddings, authentication, reindexing |
| [docs/mcp-reference.md](./docs/mcp-reference.md) | MCP tools, resources, and prompts |
| [docs/architecture.md](./docs/architecture.md) | Indexing model and internals |
| [CHANGELOG.md](./CHANGELOG.md) | Release history |

More reference and maintainer notes are indexed in [docs/](./docs/README.md).

## License

MIT.
