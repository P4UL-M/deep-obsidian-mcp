# Debian / Ubuntu (`apt`) packaging

The Deep Obsidian MCP ships a `.deb` alongside the Homebrew tap, so it can be
installed with `apt` on Debian/Ubuntu (and derivatives).

Packages are published for **amd64** and **arm64**.

## Install from the APT repository (recommended)

The project hosts a signed APT repository on GitHub Pages, so you get updates
through normal `apt upgrade`:

```bash
# 1. Trust the repository signing key
curl -fsSL https://p4ul-m.github.io/deep-obsidian-mcp/deep-obsidian-mcp.gpg \
  | sudo gpg --dearmor -o /usr/share/keyrings/deep-obsidian-mcp.gpg

# 2. Add the repository (architecture is detected automatically)
echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/deep-obsidian-mcp.gpg] https://p4ul-m.github.io/deep-obsidian-mcp stable main" \
  | sudo tee /etc/apt/sources.list.d/deep-obsidian-mcp.list

# 3. Install
sudo apt update
sudo apt install deep-obsidian-mcp
```

## Install a single `.deb` (alternative)

Or download `deep-obsidian-mcp_<version>_<arch>.deb` from the
[GitHub release](https://github.com/P4UL-M/deep-obsidian-mcp/releases) and install it directly:

```bash
sudo apt install ./deep-obsidian-mcp_<version>_amd64.deb   # or _arm64.deb
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

CI builds both architectures natively (`ubuntu-24.04` for amd64,
`ubuntu-24.04-arm` for arm64), installs and smoke-tests each `.deb`, and
validates a signed APT repo by installing from it over `file://`
(`.github/workflows/release-deb.yml`).

## Maintainer: publishing the APT repository

On a pushed `v*` tag the workflow builds the signed repo and deploys it to
GitHub Pages, and attaches the `.deb`s to the release. One-time setup:

1. Generate a signing key and add it as repository secrets:
   - `APT_GPG_PRIVATE_KEY` — the armored private key (`gpg --armor --export-secret-keys <id>`)
   - `APT_GPG_PASSPHRASE` — optional, if the key is passphrase-protected
2. Ensure GitHub Pages is allowed to deploy from Actions (the workflow calls
   `configure-pages` with `enablement: true`, which turns it on automatically).

To rebuild the repo locally from a directory of `.deb`s:

```bash
scripts/build-apt-repo.sh <deb-dir> <out-dir> <gpg-key-id>
```

Note: each release publishes the current version(s); the Pages site is replaced
on every deploy, so only the latest release is served from the repo (older
versions remain available as GitHub release assets).

## Notes

- The systemd unit runs `serve --packaged --transport http`; vault path, port,
  embeddings, and auth all come from `~/.config/deep-obsidian-mcp/config.json`,
  so run `setup-service` before enabling the unit.
- macOS users should use the [Homebrew tap](./homebrew-service.md) instead.
