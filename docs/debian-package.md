# Debian / Ubuntu (`apt`) packaging

The Deep Obsidian MCP ships a `.deb` alongside the Homebrew tap, so it can be
installed with `apt` on Debian/Ubuntu (and derivatives).

Packages are published for **amd64** and **arm64**.

## Install from the APT repository (recommended)

The project hosts a signed APT repository on GitHub Pages, so you get updates
through normal `apt upgrade`. The one-liner adds the key + repository and
installs:

```bash
curl -fsSL https://p4ul-m.github.io/deep-obsidian-mcp/install.sh | sudo bash
```

If you'd rather not pipe a script to `bash`, run the same steps yourself:

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

(The one-liner just runs steps 1–3; the script is published at
`/install.sh` on the same Pages site.)

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
- `/usr/share/deep-obsidian-mcp/sidecar/livesync-sidecar/dist/sidecar.mjs` — the LiveSync sidecar bundle (see below)
- `/usr/lib/systemd/user/deep-obsidian-mcp.service` — a systemd **user** service (not auto-started)
- `/usr/share/doc/deep-obsidian-mcp/` — README and this document

### The LiveSync sidecar, and why `nodejs` is a `Recommends`

The bundle is used **only** by the experimental `couchdb` (Self-hosted LiveSync) mount
kind, which runs it in a Node child process. Every other mount — including the default
filesystem vault — needs no Node, so making Node a `Depends` would pull a ~30 MB runtime
onto every install for a feature behind an experimental flag. Hence:

```
Recommends: nodejs (>= 20)
```

Debian 12 ships nodejs 18, below the sidecar's floor
(`sidecar/livesync-sidecar/package.json` → `engines.node`), so on bookworm `apt` cannot
satisfy that recommendation and skips it. Install Node 20+ from backports or NodeSource
only if you want a couchdb mount.

**The bundle path is not arbitrary, and no environment variable points at it.** The binary
derives it from its own location: `/usr/bin/deep-obsidian-mcp` walks up to `/usr` and
appends `share/deep-obsidian-mcp/sidecar/livesync-sidecar/dist/sidecar.mjs`
(`PACKAGED_BUNDLE_PREFIX` in `rust/crates/deep-obsidian-backend/src/sidecar.rs`). The same
rule resolves to Homebrew's `pkgshare`, which is why one constant covers both channels and
the systemd unit carries no `Environment=DEEP_OBSIDIAN_LIVESYNC_SIDECAR` line.
`DEEP_OBSIDIAN_LIVESYNC_SIDECAR` remains a user-facing override for a hand-built bundle.

Check the result — no CouchDB and no credentials required:

```bash
deep-obsidian-mcp doctor        # per couchdb mount: bundle located? node >= 20?
```

`scripts/linux-smoke-test.sh` asserts exactly this in CI, running `doctor` from `/` against
a hand-written couchdb mount config so that a source checkout in the working directory
cannot satisfy the probe by accident.

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

Requires a Linux host (or container) with a Rust toolchain **and Node 20+**, which builds
the sidecar bundle the package ships. `cargo-deb` is installed automatically if missing:

```bash
scripts/build-deb.sh              # version from Cargo.toml
scripts/build-deb.sh 0.1.0-alpha.11   # stamp an explicit version
# Output: target/debian/deep-obsidian-mcp_<version>_<arch>.deb
```

The script runs `npm ci && npm run build` in `sidecar/livesync-sidecar` before invoking
cargo-deb (which declares the bundle as an asset and would abort on a missing one), and
skips that step if `dist/sidecar.mjs` already exists. With no Node, or Node below 20, it
**fails with a message rather than building a package without the bundle** — such a `.deb`
would install and serve filesystem vaults perfectly and fail only once someone configured a
couchdb mount, by which time it is published.

CI builds both architectures natively (`ubuntu-24.04` for amd64,
`ubuntu-24.04-arm` for arm64) inside a Debian bookworm container, installs and
smoke-tests each `.deb`, and validates a signed APT repo by installing from it
over `file://` (`.github/workflows/release-deb.yml`). The container image has no Node and
bookworm's apt `nodejs` is 18, so the workflow uses `actions/setup-node@v4` to supply
Node 20.

**glibc compatibility:** the package links against the build host's glibc, and
`cargo-deb` stamps the required symbol versions as the `libc6` floor. Building
inside bookworm keeps the requirement low (currently `libc6 (>= 2.34)`), so
the package installs on Debian 12+ and Ubuntu 22.04+. Building directly on an
Ubuntu 24.04 host would stamp `libc6 (>= 2.39)` and lock out everything older
than Ubuntu 24.04 — the CI smoke test fails if the floor rises above 2.35.

## Local integration test (Docker)

To reproduce the full install path locally — build the `.deb` from HEAD in a
bookworm container, then install and exercise it (binary, packaged files,
systemd unit, `setup-service`, HTTP + stdio MCP transports) in clean
Debian 12, Ubuntu 24.04, and Ubuntu 22.04 containers:

```bash
scripts/run-linux-integration-docker.sh                # locally built .deb
scripts/run-linux-integration-docker.sh --published    # also test the live APT repo
```

Logs land in `outputs/linux-integration/`. The per-image smoke test is
`scripts/linux-smoke-test.sh`, the same script CI runs after installing the
package.

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
