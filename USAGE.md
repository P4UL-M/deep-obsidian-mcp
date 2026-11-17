# Using deep-obsidian-mcp

This guide covers first setup, agent connections and running the service.
Install first using [INSTALL.md](./INSTALL.md). For day-to-day management and
command-specific help, use the [CLI guide](docs/cli.md).

1. [Set up your vault](#1-set-up-your-vault)
2. [Connect your agent](#2-connect-your-agent)
3. [Run it as a service](#3-run-it-as-a-service)
4. [Check it's working & troubleshoot](#4-check-its-working--troubleshoot)

For embeddings, authentication, and tuning, see
[CONFIGURATION.md](./CONFIGURATION.md). For the full list of tools your agent
gets, see [docs/mcp-reference.md](./docs/mcp-reference.md).

## Already have a backend?

```sh
deep-obsidian-cli link
deep-obsidian-cli doctor
deep-obsidian-cli mounts list
```

Linking chooses a running Docker backend or known local configuration. For
remote hosts, Docker contexts, explicit targets and local overrides, see
[Choose a backend](docs/cli.md#choose-a-backend).

## 1. Set up your vault

`setup-service` writes a config file describing your vault and options. Run the
interactive wizard for a first install:

```bash
deep-obsidian-cli setup-service --wizard
```

Choose the storage, optional embeddings, transport and authentication, then
review the proposed configuration before saving. You can also register local
agent clients, install skills and enable vault snippets. Multiuser activation
asks for explicit users and vault roles; see [Multiuser](docs/multiuser.md).

Enter accepts a default. Cancelling before confirmation leaves the existing
configuration and identities unchanged. The [configuration reference](CONFIGURATION.md#the-setup-wizard)
explains storage validation and credential handling.

The wizard does **not** edit a config that already declares a mount table — it
points you at `mounts add` / `mounts list` / `mounts remove`, which change a table
without regenerating the rest of the file.

Or pass the choices directly (good for scripts):

```bash
deep-obsidian-cli setup-service --vault ~/Vault --mcp --skills --vault-snippets
```

Run `deep-obsidian-cli setup-service --help` for setup options, and add
`--dry-run` to preview changes. `--mcp`, `--skills` and `--vault-snippets`
install their respective local client integrations.

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
deep-obsidian-cli doctor        # diagnose config, vault access, index, dependencies
deep-obsidian-cli probe         # exercise the HTTP health and MCP endpoints
deep-obsidian-cli print-config  # show the resolved configuration (secrets redacted)
deep-obsidian-cli mounts list   # what is mounted where (experimental multi-backend vaults)
deep-obsidian-cli secrets check # does every stored credential still resolve? (no value printed)
```

`secrets check` is also how you rotate one: it names the `secrets set` command for any
reference it reports as `MISSING`. For a lost bearer token, see
[Token recovery](docs/authentication.md#recover-a-lost-bearer-token). For replacement, see
[CONFIGURATION.md § Rotating a stored secret](./CONFIGURATION.md#rotating-a-stored-secret).

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
