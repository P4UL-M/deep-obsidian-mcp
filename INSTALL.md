# Installing deep-obsidian-mcp

Pick the method for your platform. After installing, continue with the
[Usage guide](./USAGE.md) to point it at your vault and connect your agent.

- [macOS (Homebrew)](#macos-homebrew)
- [Debian / Ubuntu (apt)](#debian--ubuntu-apt)
- [From source](#from-source)
- [Optional: CouchDB mounts](#optional-couchdb-mounts)
- [Updating](#updating)
- [Uninstalling](#uninstalling)

Runtime dependency: [ripgrep](https://github.com/BurntSushi/ripgrep) (`rg`).
The Homebrew and apt packages install it automatically.

**Optional: Node.js 20+**, and only for the experimental `couchdb` (Self-hosted LiveSync)
mount kind, which runs a small Node sidecar. A filesystem vault — the default and the only
non-experimental configuration — needs no Node at all, which is why no package makes it a
hard dependency. See [Optional: CouchDB mounts](#optional-couchdb-mounts).

## macOS (Homebrew)

```bash
brew tap P4UL-M/tap
brew install deep-obsidian-mcp
```

Next: [set up your vault](./USAGE.md#1-set-up-your-vault) and
[start the service](./USAGE.md#3-run-it-as-a-service).
Full service model and troubleshooting: [docs/homebrew-service.md](./docs/homebrew-service.md).

## Debian / Ubuntu (apt)

Packages are published for **amd64** and **arm64**, and support Debian 12+
and Ubuntu 22.04+ (glibc 2.35 or newer).

The one-liner adds the signed APT repository and installs the package (you then
get updates through normal `apt upgrade`):

```bash
curl -fsSL https://p4ul-m.github.io/deep-obsidian-mcp/install.sh | sudo bash
```

Prefer not to pipe a script to `bash`? Do the same steps yourself:

```bash
curl -fsSL https://p4ul-m.github.io/deep-obsidian-mcp/deep-obsidian-mcp.gpg \
  | sudo gpg --dearmor -o /usr/share/keyrings/deep-obsidian-mcp.gpg
echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/deep-obsidian-mcp.gpg] https://p4ul-m.github.io/deep-obsidian-mcp stable main" \
  | sudo tee /etc/apt/sources.list.d/deep-obsidian-mcp.list
sudo apt update && sudo apt install deep-obsidian-mcp
```

Or grab a single `.deb` from the
[releases page](https://github.com/P4UL-M/deep-obsidian-mcp/releases) and install
it directly:

```bash
sudo apt install ./deep-obsidian-mcp_<version>_amd64.deb   # or _arm64.deb
```

The package installs the binary to `/usr/bin`, packaged templates to
`/usr/share/deep-obsidian-mcp/`, and a systemd **user** unit to
`/usr/lib/systemd/user/`. Deeper detail (systemd, building the `.deb`, the APT
repo internals): [docs/debian-package.md](./docs/debian-package.md).

The `.deb` also ships the LiveSync sidecar bundle at
`/usr/share/deep-obsidian-mcp/sidecar/livesync-sidecar/dist/sidecar.mjs`, and `Recommends:
nodejs (>= 20)` rather than depending on it. Debian 12's archive has nodejs 18, so on
bookworm apt will skip that recommendation — install Node 20+ from backports or NodeSource
only if you want a `couchdb` mount.

## From source

Requires a [Rust toolchain](https://rustup.rs) and `ripgrep` on your `PATH`.

```bash
git clone https://github.com/P4UL-M/deep-obsidian-mcp.git
cd deep-obsidian-mcp
cargo build --release -p deep-obsidian-cli --bin deep-obsidian-mcp
```

The binary is at `target/release/deep-obsidian-mcp`. The `bin/deep-obsidian-mcp`
wrapper finds the release (or debug) build automatically, so you can run either:

```bash
./bin/deep-obsidian-mcp --vault /path/to/obsidian-vault
target/release/deep-obsidian-mcp --vault /path/to/obsidian-vault
```

Workspace commands:

```bash
cargo check --workspace
cargo test --workspace
```

## Optional: CouchDB mounts

Needed only if you configure an **experimental** `couchdb` (Self-hosted LiveSync) mount.
Such a mount reads the vault through a small Node sidecar, so it needs two things: the
built sidecar bundle, and Node 20 or newer.

**Where the bundle has to be.** The binary finds it relative to its own location: from
`<prefix>/bin/deep-obsidian-mcp` it looks for
`<prefix>/share/deep-obsidian-mcp/sidecar/livesync-sidecar/dist/sidecar.mjs`. No
environment variable is involved in a packaged install — neither the systemd unit nor the
`brew services` plist sets one.

| Install method | Bundle | What you need to do |
|---|---|---|
| apt / `.deb` | **Shipped** at `/usr/share/deep-obsidian-mcp/sidecar/livesync-sidecar/dist/sidecar.mjs` | Install Node 20+ (`Recommends`, so apt may have skipped it) |
| Homebrew | **Shipped** (since v0.2.0-alpha.1) at `$(brew --prefix)/share/deep-obsidian-mcp/sidecar/livesync-sidecar/dist/sidecar.mjs` — the formula fetches the prebuilt bundle attached to the release, because `brew install` cannot run `npm ci` | Install Node 20+ |
| Docker | **Shipped** at `/opt/deep-obsidian-mcp/share/deep-obsidian-mcp/sidecar/livesync-sidecar/dist/sidecar.mjs` | Nothing — the image carries Node |
| From source | Built by you | `npm ci && npm run build` (below) |

Building the bundle by hand is therefore only needed for a **source install**, or for a
Homebrew install pinned to a tag older than v0.2.0-alpha.1:

```bash
cd sidecar/livesync-sidecar
npm ci && npm run build      # writes dist/sidecar.mjs
```

A source checkout is found automatically. To use a hand-built bundle with a Homebrew
install, copy it over the probed location:

```bash
mkdir -p "$(brew --prefix)/share/deep-obsidian-mcp/sidecar/livesync-sidecar/dist"
cp dist/sidecar.mjs "$(brew --prefix)/share/deep-obsidian-mcp/sidecar/livesync-sidecar/dist/"
```

...or point at it explicitly, per mount with `"sidecarPath"` in `config.json`, or globally:

```bash
export DEEP_OBSIDIAN_LIVESYNC_SIDECAR=/path/to/dist/sidecar.mjs
export DEEP_OBSIDIAN_NODE=/path/to/node     # only if `node` is not on the service's PATH
```

Verify — this contacts no CouchDB and needs no credentials:

```bash
deep-obsidian-mcp doctor
```

It prints, per couchdb mount, whether the bundle was located and whether Node satisfies
the `>= 20` floor. Neither is reported as a failure: a couchdb mount is experimental and
cannot be the vault root, so the root vault keeps serving without it. Add
`--probe-remote` to also contact the remote read-only once credentials are configured.

Moving notes onto — and back off — a remote mount:
[docs/migration-and-rollback.md](./docs/migration-and-rollback.md).

## Updating

```bash
# Homebrew
brew upgrade deep-obsidian-mcp && brew services restart deep-obsidian-mcp

# apt (repository install)
sudo apt update && sudo apt upgrade deep-obsidian-mcp
```

## Uninstalling

```bash
# Homebrew
brew services stop deep-obsidian-mcp
brew uninstall deep-obsidian-mcp

# apt
systemctl --user disable --now deep-obsidian-mcp   # if you enabled the service
sudo apt remove deep-obsidian-mcp
```
