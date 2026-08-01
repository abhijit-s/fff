# Convenience recipes for installing and managing the fff daemon toolkit
# (fff-mcp + fff-engine + fffctl). Install logic lives in
# scripts/install-fff.sh — these recipes are thin, DRY wrappers.
#
#   just            # list recipes
#   just install    # install/upgrade (stable channel)
#   just status     # version + running daemons

# List available recipes
default:
    @just --list

# Install/upgrade — stable (macOS: brew tap · Debian/Ubuntu: apt · else: source)
install:
    bash scripts/install-fff.sh

# Install/upgrade by building from the current main branch
install-head:
    FFF_CHANNEL=head bash scripts/install-fff.sh

# Restart the background daemon onto the installed binary
restart:
    fffctl restart

# Show installed version and running daemons
status:
    @fff-mcp --version 2>/dev/null || echo "fff-mcp not on PATH"
    @fffctl list 2>/dev/null || true

# Register fff with Claude Code (user-scoped)
register:
    claude mcp add -s user fff -- "$(command -v fff-mcp)"

# Remove fff (macOS: Homebrew · Debian/Ubuntu: apt)
uninstall:
    #!/usr/bin/env bash
    set -euo pipefail
    case "$(uname -s)" in
      Darwin) brew uninstall fff ;;
      Linux)  sudo apt-get remove -y fff ;;
      *) echo "manual uninstall: remove fff-mcp/fff-engine/fffctl from your bin dir" ;;
    esac
