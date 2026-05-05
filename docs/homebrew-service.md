# Homebrew Service

This document describes the Homebrew operating model for `deep-obsidian-mcp`.

The Rust implementation exposes the service-oriented commands needed for this workflow. The formula service stanza now runs the CLI in packaged HTTP mode; the only release-time inputs still owned by the tap are the real source artifact URL/checksum and clean-machine validation.

## Target Workflow

```bash
brew tap <tap-name>
brew install deep-obsidian-mcp
deep-obsidian-mcp setup-service --vault ~/Vault --mcp --skills --vault-snippets
brew services start deep-obsidian-mcp
deep-obsidian-mcp doctor
```

Optional inspection commands:

```bash
deep-obsidian-mcp print-config
deep-obsidian-mcp probe
curl -fsS http://127.0.0.1:4100/healthz
curl -fsS http://127.0.0.1:4100/readyz
```

## Intended Command Roles

- `setup-service` persists the resolved vault and service settings to a stable config file.
- `setup-service --mcp` also configures local coding-agent MCP clients: Codex through `~/.codex/config.toml`, and Claude Code through `claude mcp add` when the `claude` CLI is available.
- `setup-service --skills` installs packaged skills into `~/.codex/skills` and `~/.claude/skills`.
- `setup-service --vault-snippets` installs packaged Obsidian CSS snippets into `<vault>/.obsidian/snippets` and enables them in `<vault>/.obsidian/appearance.json`.
- `doctor` checks the vault path, config file, writable index directory, local SQLite index diagnostics, `rg` availability, port availability, and a running health endpoint when one is available.
- `print-config` shows the normalized resolved config so the user can see what the service will actually read.
- `probe` performs a minimal health or MCP connectivity check.
- `serve` starts the long-lived HTTP service using the resolved config.

## Configuration Model

Expected fields:

- `vault_path`
- `index_dir`
- `transport`
- `http.host`
- `http.port`
- `http.mcp_path`
- `http.health_path`
- `auto_reindex.enabled`
- `auto_reindex.debounce_ms`
- `auto_reindex.interval_ms`
- `embedding.provider`
- `embedding.model`
- `embedding.base_url`
- `embedding.api_key_env`

Maintained precedence:

1. CLI flags
2. config file
3. environment variables
4. defaults

The config file path in the current implementation is `~/.config/deep-obsidian-mcp/config.json`. Homebrew-managed installs may also want a tap-owned support path or a generated environment file, but the config file remains the canonical source for the service.

For newly generated service configs, `setup-service` keeps explicit `--index-dir`, config file, and environment values intact. When no index directory is provided, it writes a packaged default outside the vault under `~/Library/Application Support/deep-obsidian-mcp/indexes/<vault-hash>`.

Packaged services also pass `--packaged` and set `DEEP_OBSIDIAN_PACKAGED=1`. In that mode, any otherwise-default `index_dir` resolves to the same Application Support location. Direct ad-hoc commands that only pass `--vault` and do not opt into packaged mode still use the vault-local runtime default for compatibility.

`doctor` never prints inline embedding API keys. JSON output includes source attribution for resolved config fields, auto-reindex debounce and interval settings, MCP/health/readiness endpoint URLs, index file path/size/schema metadata when the SQLite file exists, and health/readiness payload details when the service is already running.

## Release Binary And Codesign

For local launchd validation outside a finalized bottle, build and ad-hoc sign the release binary before installing or restarting the service:

```bash
cargo build --release -p deep-obsidian-cli --bin deep-obsidian-mcp
codesign --force --sign - --timestamp=none target/release/deep-obsidian-mcp
codesign --verify --verbose=2 target/release/deep-obsidian-mcp
```

The ad-hoc signature is not a distribution notarization story. It is a local macOS service hygiene step so launchd runs the exact release binary consistently during development and packaging validation. A finished Homebrew bottle should own its normal release signing/notarization expectations separately.

## Service Expectations

The Homebrew service should:

- start HTTP mode only
- read config from a stable file
- pass `--packaged` so defaults are service-safe
- avoid interactive prompts during `brew services start`
- log to predictable locations
- keep the MCP and health endpoints stable across upgrades
- expose `/healthz` for lightweight process health and `/readyz` for index readiness
- keep generated indexes outside the vault by default for service configs

## Restart And Diagnostics

After changing config, upgrading the binary, or changing signing state, restart the service and check readiness:

```bash
brew services restart deep-obsidian-mcp
deep-obsidian-mcp doctor
deep-obsidian-mcp probe
curl -fsS http://127.0.0.1:4100/readyz
```

For the legacy launchd development scripts, use the service label directly:

```bash
launchctl kickstart -k "gui/$(id -u)/io.deep-obsidian-mcp"
tail -f ~/Library/Logs/io.deep-obsidian-mcp/stdout.log
tail -f ~/Library/Logs/io.deep-obsidian-mcp/stderr.log
```

For the Homebrew formula service, logs are expected under Homebrew's `var/log/deep-obsidian-mcp/` path, matching the formula service stanza.

Readiness is the packaging gate: `/healthz` can be healthy while the index is still loading or degraded, but `/readyz` must become successful before the service is considered usable by MCP clients.

## Transitional Notes

The existing `scripts/install-launchd-service.sh` and `scripts/run-http-service.sh` now delegate to the same `serve` command and are still useful for local development, but they are not the target product workflow.

The formula in `Formula/deep-obsidian-mcp.rb` installs the CLI, installs project icon assets under `pkgshare/assets`, installs packaged agent skills under `pkgshare/skills`, installs packaged Obsidian CSS snippets under `pkgshare/obsidian-snippets`, creates the Homebrew log directory, runs `serve --packaged --transport http`, sets `DEEP_OBSIDIAN_PACKAGED=1`, and validates the packaged binary with `help` and `version` smoke tests. Users can then run `setup-service --mcp --skills --vault-snippets` to install the MCP client entries, agent skills, and vault snippets from the packaged assets.

Current packaging gaps are tracked in [docs/homebrew-gap-todo.md](./homebrew-gap-todo.md).
