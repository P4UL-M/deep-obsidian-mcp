# Debian / Ubuntu (`apt`) packaging

The Deep Obsidian MCP ships a `.deb` alongside the Homebrew tap, so it can be
installed with `apt` on Debian/Ubuntu (and derivatives).

## Install

Download the `deep-obsidian-mcp_<version>_amd64.deb` from the
[GitHub release](https://github.com/P4UL-M/deep-obsidian-mcp/releases) and install it:

```bash
sudo apt install ./deep-obsidian-mcp_<version>_amd64.deb
```

`apt` resolves the runtime dependency (`ripgrep`) automatically. The package
installs:

- `/usr/bin/deep-obsidian-mcp` — the CLI/server binary
- `/usr/share/deep-obsidian-mcp/{skills,obsidian-snippets,assets}` — packaged templates and assets
- `/usr/lib/systemd/user/deep-obsidian-mcp.service` — a systemd **user** service (not auto-started)
- `/usr/share/doc/deep-obsidian-mcp/` — README and this document

## Configure

Create the service config for your vault (stores indexes outside the vault in
packaged mode, under `$XDG_DATA_HOME/deep-obsidian-mcp/indexes/` — typically
`~/.local/share/deep-obsidian-mcp/indexes/`):

```bash
deep-obsidian-mcp setup-service --vault ~/Vault --mcp --skills --vault-snippets
# Optional: enable HTTP bearer auth (prints a token once):
deep-obsidian-mcp setup-service --vault ~/Vault --auth
```

## Run as a service

The package ships a systemd **user** unit (per-user vault and secret store).
Enable and start it:

```bash
systemctl --user daemon-reload
systemctl --user enable --now deep-obsidian-mcp
# Survive logout (run the user service without an active session):
loginctl enable-linger "$USER"
```

Verify:

```bash
deep-obsidian-mcp doctor
curl -fsS http://127.0.0.1:4100/readyz
journalctl --user -u deep-obsidian-mcp -f   # logs
```

Stop/disable:

```bash
systemctl --user disable --now deep-obsidian-mcp
```

## Build the `.deb` from source

Requires a Linux host (or container) with a Rust toolchain. `cargo-deb` is
installed automatically if missing:

```bash
scripts/build-deb.sh              # version from Cargo.toml
scripts/build-deb.sh 0.1.0-alpha.11   # stamp an explicit version
# Output: target/debian/deep-obsidian-mcp_<version>_<arch>.deb
```

CI builds, installs, and smoke-tests the package on every relevant PR and
attaches it to tagged releases (`.github/workflows/release-deb.yml`).

## Notes

- The systemd unit runs `serve --packaged --transport http`; vault path, port,
  embeddings, and auth all come from `~/.config/deep-obsidian-mcp/config.json`,
  so run `setup-service` before enabling the unit.
- macOS users should use the [Homebrew tap](./homebrew-service.md) instead.
