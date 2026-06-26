# Installing deep-obsidian-mcp

Pick the method for your platform. After installing, continue with the
[Usage guide](./USAGE.md) to point it at your vault and connect your agent.

- [macOS (Homebrew)](#macos-homebrew)
- [Debian / Ubuntu (apt)](#debian--ubuntu-apt)
- [From source](#from-source)
- [Updating](#updating)
- [Uninstalling](#uninstalling)

Runtime dependency: [ripgrep](https://github.com/BurntSushi/ripgrep) (`rg`).
The Homebrew and apt packages install it automatically.

## macOS (Homebrew)

```bash
brew tap P4UL-M/tap
brew install deep-obsidian-mcp
```

Next: [set up your vault](./USAGE.md#1-set-up-your-vault) and
[start the service](./USAGE.md#3-run-it-as-a-service).
Full service model and troubleshooting: [docs/homebrew-service.md](./docs/homebrew-service.md).

## Debian / Ubuntu (apt)

Packages are published for **amd64** and **arm64**.

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
