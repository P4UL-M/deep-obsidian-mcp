# Homebrew Service

This document describes the Homebrew operating model for `deep-obsidian-mcp`.

The Node implementation now exposes the service-oriented commands needed for this workflow. The remaining gap is packaging: the formula and release artifact story are still scaffolding rather than a finished distribution.

## Target Workflow

```bash
brew tap <tap-name>
brew install deep-obsidian-mcp
deep-obsidian-mcp setup-service --vault ~/Vault
brew services start deep-obsidian-mcp
deep-obsidian-mcp doctor
```

Optional inspection commands:

```bash
deep-obsidian-mcp print-config
deep-obsidian-mcp probe
```

## Intended Command Roles

- `setup-service` persists the resolved vault and service settings to a stable config file.
- `doctor` checks the vault path, config file, writable index directory, `rg` availability, port availability, and a running health endpoint.
- `print-config` shows the normalized resolved config so the user can see what the service will actually read.
- `probe` performs a minimal health or MCP connectivity check.
- `serve` starts the long-lived HTTP service using the resolved config.

## Configuration Model

The plan in `docs/brew-service-refactor-plan.md` treats configuration as the shared contract across the Node and Rust implementations.

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

Planned precedence:

1. CLI flags
2. config file
3. environment variables
4. defaults

The config file path in the current implementation is `~/.config/deep-obsidian-mcp/config.json`. Homebrew-managed installs may also want a tap-owned support path or a generated environment file, but the config file remains the canonical source for the service.

## Service Expectations

The Homebrew service should:

- start HTTP mode only
- read config from a stable file
- avoid interactive prompts during `brew services start`
- log to predictable locations
- keep the MCP and health endpoints stable across upgrades

## Transitional Notes

The existing `scripts/install-launchd-service.sh` and `scripts/run-http-service.sh` now delegate to the same `serve` command and are still useful for local development, but they are not the target product workflow.

The formula scaffold in `Formula/deep-obsidian-mcp.rb` is intentionally conservative until the Node refactor lands the service CLI and config loader.
