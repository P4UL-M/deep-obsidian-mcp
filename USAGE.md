# Using deep-obsidian-mcp

This guide takes you from a fresh install to a running server your agent can
use. If you haven't installed it yet, start with [INSTALL.md](./INSTALL.md).

1. [Set up your vault](#1-set-up-your-vault)
2. [Connect your agent](#2-connect-your-agent)
3. [Run it as a service](#3-run-it-as-a-service)
4. [Check it's working & troubleshoot](#4-check-its-working--troubleshoot)

For embeddings, authentication, and tuning, see
[CONFIGURATION.md](./CONFIGURATION.md). For the full list of tools your agent
gets, see [docs/mcp-reference.md](./docs/mcp-reference.md).

## 1. Set up your vault

`setup-service` writes a config file describing your vault and options. Run the
interactive wizard for a first install:

```bash
deep-obsidian-mcp setup-service --wizard
```

It asks for your vault path and whether to configure MCP clients, install
packaged skills and Obsidian snippets, enable embeddings, and enable
authentication.

Or pass the choices directly (good for scripts):

```bash
deep-obsidian-mcp setup-service --vault ~/Vault --mcp --skills --vault-snippets
```

- `--mcp` — register the server with local agents (Codex in `~/.codex/config.toml`, and Claude Code via `claude mcp add` when the CLI is present).
- `--skills` — install packaged agent skills into `~/.codex/skills` and `~/.claude/skills`.
- `--vault-snippets` — install Obsidian CSS snippets into your vault and enable them.
- `--auth` / `--no-auth` — enable or disable HTTP bearer auth (see [CONFIGURATION.md](./CONFIGURATION.md#authentication)).
- `--dry-run` — preview every change without writing.
- `--overwrite` — replace existing config, MCP entries, skills, or snippets.

The config is written to `~/.config/deep-obsidian-mcp/config.json` by default
(override with `--config <path>`). Settings resolve in this order: **CLI flag →
config file → environment variable → built-in default**.

## 2. Connect your agent

If you ran `setup-service --mcp`, your local agents are already configured.
Otherwise, connect manually.

**Over HTTP (recommended for the long-lived service).** Point your MCP client at:

```
http://127.0.0.1:4100/mcp
```

**As a stdio subprocess (e.g. Codex).** Add to `~/.codex/config.toml`:

```toml
[mcp_servers.deep_obsidian]
command = "/absolute/path/to/deep-obsidian-mcp"
args = ["/absolute/path/to/your/vault", "--stdio-mode", "auto"]
```

If your client runs MCP servers in a sandbox, the vault path must be reachable
from inside that sandbox.

## 3. Run it as a service

Running it as a background service keeps the index warm and gives clients a
stable endpoint.

**macOS (Homebrew):**

```bash
brew services start deep-obsidian-mcp
```

**Linux (systemd user service, from the apt package):**

```bash
systemctl --user enable --now deep-obsidian-mcp
loginctl enable-linger "$USER"   # keep it running after you log out
```

**From source / manual:**

```bash
./bin/deep-obsidian-mcp serve --vault ~/Vault --transport http
# or the helper wrapper:
./scripts/run-http-service.sh ~/Vault
```

On macOS you can also install a user LaunchAgent directly with
`./scripts/install-launchd-service.sh ~/Vault` (remove with
`./scripts/uninstall-launchd-service.sh`). The Homebrew flow is the recommended
packaged path — see [docs/homebrew-service.md](./docs/homebrew-service.md).

## 4. Check it's working & troubleshoot

```bash
deep-obsidian-mcp doctor        # diagnose config, vault access, index, dependencies
deep-obsidian-mcp probe         # exercise the HTTP health and MCP endpoints
deep-obsidian-mcp print-config  # show the resolved configuration (secrets redacted)
```

Health endpoints (no auth required):

- liveness: `http://127.0.0.1:4100/healthz`
- readiness: `http://127.0.0.1:4100/readyz`

```bash
curl -fsS http://127.0.0.1:4100/readyz
```

Service logs:

```bash
# macOS (Homebrew): see the log paths printed by `brew services info deep-obsidian-mcp`
# Linux (systemd user service):
journalctl --user -u deep-obsidian-mcp -f
```

`doctor` is the first thing to run when something's off — it reports vault
access problems (including macOS Full Disk Access prompts), a missing `ripgrep`,
and index status.
