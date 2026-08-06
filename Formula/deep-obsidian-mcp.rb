# typed: false

class DeepObsidianMcp < Formula
  desc "Filesystem-first MCP server for deep Obsidian vault access"
  homepage "https://github.com/P4UL-M/deep-obsidian-mcp"
  url "https://github.com/P4UL-M/deep-obsidian-mcp/archive/refs/tags/v0.2.0-alpha.1.tar.gz"
  # PLACEHOLDER — sha256 of the v0.2.0-alpha.1 source tarball, which does not exist
  # until the tag is pushed. Filled in at release time; see docs/release-checklist.md
  # ("Cutting a Release"). This copy of the formula is canonical but is NOT what
  # `brew install` uses — the tap repo P4UL-M/homebrew-tap is — so a placeholder here
  # installs nothing broken. The tap copy must NEVER carry one.
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "MIT"
  version "0.2.0-alpha.1"

  depends_on "rust" => :build
  depends_on "ripgrep"

  # The LiveSync sidecar bundle, needed only by the EXPERIMENTAL `couchdb` mount kind.
  #
  # It cannot be built during `brew install`: `sidecar/livesync-sidecar/dist/` is
  # gitignored, so the source tarball above has no copy, and `npm ci` needs network
  # access the install sandbox restricts. So the release attaches a prebuilt bundle
  # (the `sidecar-bundle` job in .github/workflows/release-deb.yml) and this resource
  # fetches it — no Node needed at install time, and none needed at runtime either
  # unless a couchdb mount is actually configured.
  #
  # `using: :nounzip` because the asset is a plain .mjs file, not an archive.
  resource "livesync-sidecar" do
    url "https://github.com/P4UL-M/deep-obsidian-mcp/releases/download/v0.2.0-alpha.1/livesync-sidecar-0.2.0-alpha.1.mjs",
        using: :nounzip
    # PLACEHOLDER — the release run prints this hash (see the workflow's "Name and
    # checksum the asset" step, and the .mjs.sha256 asset next to the bundle). Same
    # rule as above: filled in at release time, and never a placeholder in the tap.
    sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  end

  def install
    system "cargo", "install", *std_cargo_args(path: "rust/crates/deep-obsidian-cli")
    pkgshare.install "assets"
    pkgshare.install "skills"
    pkgshare.install "obsidian-snippets"
    # The ONE path the binary's own probe derives from its own location: from
    # <keg>/bin/deep-obsidian-mcp it walks up to <keg> and appends
    # share/deep-obsidian-mcp/sidecar/livesync-sidecar/dist/sidecar.mjs
    # (PACKAGED_BUNDLE_PREFIX in rust/crates/deep-obsidian-backend/src/sidecar.rs).
    # That is the same layout the .deb uses under /usr, so no channel needs
    # DEEP_OBSIDIAN_LIVESYNC_SIDECAR — it stays a user-facing override.
    resource("livesync-sidecar").stage do
      bundle = Pathname.glob("livesync-sidecar-*.mjs").fetch(0)
      (pkgshare/"sidecar/livesync-sidecar/dist").install bundle => "sidecar.mjs"
    end
    (var/"log/deep-obsidian-mcp").mkpath
  end

  def caveats
    <<~EOS
      Configure the service before starting it:
        deep-obsidian-mcp setup-service --vault ~/Vault --mcp --skills --vault-snippets

      On macOS, setup-service performs an interactive vault access preflight.
      If your vault is under Documents, Desktop, Downloads, or iCloud Drive, approve the
      permission prompt if macOS shows one. If no prompt appears, add the Homebrew
      service binary to Privacy & Security > Full Disk Access, then restart the service.

      Then start and validate:
        brew services start P4UL-M/tap/deep-obsidian-mcp
        deep-obsidian-mcp doctor
        curl -fsS http://127.0.0.1:4100/readyz

      Homebrew services run in packaged mode, so default indexes live outside the vault under:
        ~/Library/Application Support/deep-obsidian-mcp/indexes/<vault-hash>

      Agent skill templates are installed under:
        #{opt_pkgshare}/skills

      Obsidian CSS snippets are installed under:
        #{opt_pkgshare}/obsidian-snippets

      Project icons and logo assets are installed under:
        #{opt_pkgshare}/assets

      setup-service --skills copies them into Codex and Claude Code skill directories.
      setup-service --mcp configures Codex and Claude Code MCP client entries.
      setup-service --vault-snippets copies packaged Obsidian snippets into the vault and enables them.

      EXPERIMENTAL multi-backend mounts (couchdb, algolia) are opt-in per mount and are
      configured by editing config.json by hand — setup-service deliberately does not
      rewrite a mount table. `deep-obsidian-mcp print-config` shows what this build reads.

      A couchdb (Self-hosted LiveSync) mount additionally needs the LiveSync sidecar
      bundle, which this formula now INSTALLS from the release, at the path the binary
      probes for on its own — no configuration and no build step:
        #{opt_pkgshare}/sidecar/livesync-sidecar/dist/sidecar.mjs

      Running it still needs Node 20 or newer on the service's PATH, which this formula
      does not depend on (every other mount kind needs no Node at all):
        brew install node

      Building the bundle by hand is only needed for a source install that predates the
      release asset — a checkout, or a formula pinned to an older tag:
        git clone https://github.com/P4UL-M/deep-obsidian-mcp
        cd deep-obsidian-mcp/sidecar/livesync-sidecar && npm ci && npm run build
      A source checkout is then found automatically. To use such a build with this
      install, either copy dist/sidecar.mjs over the packaged path above, or point at it
      per mount with "sidecarPath" in config.json, or globally:
        export DEEP_OBSIDIAN_LIVESYNC_SIDECAR=/path/to/dist/sidecar.mjs

      `deep-obsidian-mcp doctor` reports, per mount, whether the bundle and a suitable
      Node were found. Every other mount kind — including the default filesystem vault —
      needs no Node at all.
    EOS
  end

  service do
    run [opt_bin/"deep-obsidian-mcp", "serve", "--packaged", "--transport", "http"]
    keep_alive true
    environment_variables DEEP_OBSIDIAN_PACKAGED: "1"
    log_path var/"log/deep-obsidian-mcp/output.log"
    error_log_path var/"log/deep-obsidian-mcp/error.log"
  end

  test do
    assert_match "Usage:", shell_output("#{bin}/deep-obsidian-mcp help")
    assert_match "deep-obsidian-mcp", shell_output("#{bin}/deep-obsidian-mcp version")
    # The sidecar bundle, at the exact path the binary's exe-relative probe derives. A
    # `couchdb` mount is the only thing that reads it, so a resource that silently
    # stopped landing here would otherwise surface as a broken mount, months later.
    assert_path_exists pkgshare/"sidecar/livesync-sidecar/dist/sidecar.mjs"
  end
end
