class Fff < Formula
  desc "Fast frecency-ranked file finder MCP server for AI code assistants"
  homepage "https://github.com/abhijit-s/fff"
  version "0.13.0"
  license "MIT"

  # Pre-built binaries — no Rust/LLVM dependency.
  on_macos do
    on_arm do
      url "https://github.com/abhijit-s/fff/releases/download/v0.13.0/fff-aarch64-apple-darwin.tar.gz"
      sha256 "PLACEHOLDER_ARM64"
    end
    on_intel do
      url "https://github.com/abhijit-s/fff/releases/download/v0.13.0/fff-x86_64-apple-darwin.tar.gz"
      sha256 "PLACEHOLDER_X86_64"
    end
  end

  # HEAD: build from source (requires Rust via rustup or brew)
  head do
    url "https://github.com/abhijit-s/fff.git", branch: "main"
    depends_on "rust" => :build
  end

  def install
    if build.head?
      ENV["CMAKE_ARGS"] = "-DUSE_SQLITE_CREDENTIAL_CACHING=OFF"
      system "cargo", "build", "--release", "--no-default-features",
             "-p", "fff-engine", "-p", "fff-mcp", "-p", "fff-ctl"
      bin.install "target/release/fff-mcp"
      bin.install "target/release/fff-engine"
      bin.install "target/release/fffctl"
    else
      # fff-mcp locates fff-engine via current_exe().parent() at runtime,
      # so all three must live in the same directory.
      bin.install "fff-mcp"
      bin.install "fff-engine"
      bin.install "fffctl"
    end
  end

  def caveats
    <<~EOS
      fff-mcp, fff-engine, and fffctl are all installed to #{HOMEBREW_PREFIX}/bin/.

      Register with Claude Code (user-scoped, survives updates):
        claude mcp add -s user fff -- #{bin}/fff-mcp

      Or add to your project .mcp.json:
        {
          "mcpServers": {
            "fff": { "type": "stdio", "command": "fff-mcp" }
          }
        }

      Manage running daemons with fffctl:
        fffctl list           # show all running daemons
        fffctl stop --all     # stop every daemon
        fffctl clean          # remove stale lockfiles / orphan sockets

      Configuration (optional): ~/.config/fff/config.toml
        [log]
        level = "fff_engine=info,fff_mcp=info,warn"
    EOS
  end

  test do
    assert_predicate bin/"fff-mcp", :executable?
    assert_predicate bin/"fff-engine", :executable?
    assert_predicate bin/"fffctl", :executable?
    assert_match "fff-engine", shell_output("#{bin}/fff-engine --help 2>&1", 2)
    assert_match "Manage fff-engine daemons", shell_output("#{bin}/fffctl --help 2>&1")
  end
end
