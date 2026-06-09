class Fff < Formula
  desc "Fast frecency-ranked file finder MCP server for AI code assistants"
  homepage "https://github.com/abhijit-s/fff"
  url "https://github.com/abhijit-s/fff/archive/refs/tags/v0.13.1.tar.gz"
  sha256 "d9791790ff65888faa432c9715c9e81c6b24ada5bb159baf7157c6e287474edf"
  license "MIT"

  head do
    url "https://github.com/abhijit-s/fff.git", branch: "main"
  end

  depends_on "rust" => :build

  def install
    ENV["CMAKE_ARGS"] = "-DUSE_SQLITE_CREDENTIAL_CACHING=OFF"
    system "cargo", "build", "--release", "--no-default-features",
           "-p", "fff-engine", "-p", "fff-mcp", "-p", "fff-ctl"

    bin.install "target/release/fff-mcp"
    bin.install "target/release/fff-engine"
    bin.install "target/release/fffctl"
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
