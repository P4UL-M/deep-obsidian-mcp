# typed: false

class DeepObsidianMcp < Formula
  desc "Filesystem-first MCP server for deep Obsidian vault access"
  homepage "https://github.com/<owner>/deep-obsidian-mcp"

  # TODO: replace with a real release tarball once the packaging flow is finalized.
  url "https://example.com/deep-obsidian-mcp-0.1.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "MIT"

  depends_on "rust" => :build
  depends_on "ripgrep"

  def install
    # TODO: switch to the released source or bottle artifact once packaging is finalized.
    system "cargo", "install", *std_cargo_args(path: "rust/crates/deep-obsidian-cli")
    pkgshare.install "skills"
    (var/"log/deep-obsidian-mcp").mkpath
  end

  def caveats
    <<~EOS
      Configure the service before starting it:
        deep-obsidian-mcp setup-service --vault ~/Vault

      Then start and validate:
        brew services start deep-obsidian-mcp
        deep-obsidian-mcp doctor
        curl -fsS http://127.0.0.1:4100/readyz

      Homebrew services run in packaged mode, so default indexes live outside the vault under:
        ~/Library/Application Support/deep-obsidian-mcp/indexes/<vault-hash>

      Agent skill templates are installed under:
        #{opt_pkgshare}/skills
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
  end
end
