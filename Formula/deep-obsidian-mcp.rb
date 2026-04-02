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
  end

  def caveats
    <<~EOS
      This formula is still scaffolding.
      The service commands exist, but Homebrew packaging is not complete until the project ships:
      - a real release artifact and checksum
      - validated brew service install/start/upgrade coverage
      - finalized config and log path expectations
    EOS
  end

  service do
    run [opt_bin/"deep-obsidian-mcp", "serve"]
    keep_alive true
    log_path var/"log/deep-obsidian-mcp/output.log"
    error_log_path var/"log/deep-obsidian-mcp/error.log"
  end

  test do
    # TODO: replace with a real packaged-binary smoke test once release artifacts exist.
    assert_match "Usage:", shell_output("#{bin}/deep-obsidian-mcp help")
  end
end
