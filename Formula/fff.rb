class Fff < Formula
  desc "Fast frecency-ranked file finder MCP server for AI code assistants"
  homepage "https://github.com/abhijit-s/fff"
  version "0.14.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/abhijit-s/fff/releases/download/v0.14.0/fff-aarch64-apple-darwin.tar.gz"
      sha256 "23361f387f02c8794b13feb5ea6831e17f9568e388bca5f037d24a8e7398f907"
    end
    on_intel do
      url "https://github.com/abhijit-s/fff/releases/download/v0.14.0/fff-x86_64-apple-darwin.tar.gz"
      sha256 "b6c17542e3d7c81bc477e96832453d40c7782c1e90a204e9640c9b267bdb50ee"
    end
  end

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

        # Search multiple projects from one server:
        [mcp]
        default = "app"
        [[mcp.roots]]
        name = "app"
        path = "/Users/you/work/app"
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
