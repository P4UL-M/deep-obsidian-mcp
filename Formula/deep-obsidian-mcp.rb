# typed: false

class DeepObsidianMcp < Formula
  desc "Filesystem-first MCP server for deep Obsidian vault access"
  homepage "https://github.com/<owner>/deep-obsidian-mcp"

  # TODO: replace with a real release tarball once the packaging flow is finalized.
  url "https://example.com/deep-obsidian-mcp-0.1.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "MIT"

  depends_on "node@22"
  depends_on "ripgrep"

  def install
    # TODO: install the release artifact layout rather than the developer tree.
    # The intended final shape is:
    # - a compiled executable under bin/
    # - support files under libexec/
    # - a config template under etc/
    bin.install_symlink "deep-obsidian-mcp"
  end

  def caveats
    <<~EOS
      This formula is a scaffold for the planned Homebrew service workflow.
      The Node refactor still needs to land `setup-service`, `doctor`, `print-config`, and `probe`
      before this formula becomes a real end-user package.
    EOS
  end

  service do
    run [opt_bin/"deep-obsidian-mcp", "serve"]
    keep_alive true
    log_path var/"log/deep-obsidian-mcp/output.log"
    error_log_path var/"log/deep-obsidian-mcp/error.log"
  end

  test do
    # TODO: replace with a real smoke test against the packaged binary.
    assert_match "Usage:", shell_output("#{bin}/deep-obsidian-mcp help")
  end
end
