#!/usr/bin/env bash
# Install the fff daemon toolkit (fff-mcp + fff-engine + fffctl) from the
# abhijit-s/fff fork, picking the right channel for the current platform:
#
#   macOS          -> Homebrew tap (abhijit-s/tap)
#   Debian/Ubuntu  -> signed APT repo on GitHub Pages
#   other Linux    -> build from source with cargo
#
# Usage:
#   bash scripts/install-fff.sh                 # stable channel
#   FFF_CHANNEL=head bash scripts/install-fff.sh # build from current main
#
# Curl-able:
#   curl -fsSL https://raw.githubusercontent.com/abhijit-s/fff/main/scripts/install-fff.sh | bash
set -euo pipefail

TAP_SHORT="abhijit-s/tap"
TAP_REPO="abhijit-s/homebrew-tap"
APT_BASE="https://abhijit-s.github.io/fff"
GIT_URL="https://github.com/abhijit-s/fff"
CHANNEL="${FFF_CHANNEL:-stable}" # stable | head

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

install_macos() {
  command -v brew >/dev/null || die "Homebrew is required — see https://brew.sh"
  log "Tapping + trusting $TAP_SHORT"
  brew tap "$TAP_REPO" >/dev/null 2>&1 || true
  brew trust "$TAP_SHORT" >/dev/null 2>&1 || true # recent Homebrew gates untrusted taps

  local arch; arch="$(uname -m)"
  if [ "$CHANNEL" = head ]; then
    log "Installing fff (--HEAD, builds from main)"
    brew install --HEAD "$TAP_SHORT/fff" || brew upgrade --fetch-HEAD "$TAP_SHORT/fff"
  elif [ "$arch" = "arm64" ]; then
    log "Installing fff (stable arm64 bottle)"
    brew install "$TAP_SHORT/fff" || brew upgrade "$TAP_SHORT/fff"
  else
    log "Stable bottle is arm64-only; building from source (--HEAD) on $arch"
    brew install --HEAD "$TAP_SHORT/fff" || brew upgrade --fetch-HEAD "$TAP_SHORT/fff"
  fi
}

install_debian() {
  command -v curl >/dev/null || die "curl is required"
  log "Adding signed APT repo $APT_BASE"
  sudo install -d -m 0755 /etc/apt/keyrings
  curl -fsSL "$APT_BASE/fff.gpg" | sudo tee /etc/apt/keyrings/fff.asc >/dev/null
  echo "deb [signed-by=/etc/apt/keyrings/fff.asc] $APT_BASE stable main" \
    | sudo tee /etc/apt/sources.list.d/fff.list >/dev/null
  log "apt update && install fff"
  sudo apt-get update && sudo apt-get install -y fff
}

install_source() {
  command -v cargo >/dev/null || die "Rust/cargo is required — see https://rustup.rs"
  local dest="${PREFIX:-$HOME/.local}/bin"
  local work; work="$(mktemp -d)"
  log "Building from source ($GIT_URL) — ripgrep backend, no Zig"
  git clone --depth 1 "$GIT_URL" "$work/fff"
  ( cd "$work/fff" && cargo build --release -p fff-mcp -p fff-engine -p fff-ctl )
  install -d "$dest"
  install -m 0755 "$work/fff/target/release/fff-mcp" "$work/fff/target/release/fff-engine" \
    "$work/fff/target/release/fffctl" "$dest/"
  rm -rf "$work"
  log "Installed to $dest (ensure it is on your PATH)"
}

main() {
  case "$(uname -s)" in
    Darwin) install_macos ;;
    Linux)
      if command -v apt-get >/dev/null && [ -f /etc/debian_version ]; then
        install_debian
      else
        install_source
      fi
      ;;
    *) die "unsupported OS: $(uname -s)" ;;
  esac

  echo
  local bin; bin="$(command -v fff-mcp || echo fff-mcp)"
  log "Installed: $("$bin" --version 2>/dev/null || echo 'fff-mcp (restart shell if not on PATH)')"
  cat <<EOF

Next: register with your AI client, e.g. Claude Code (user-scoped):
    claude mcp add -s user fff -- "$bin"

Manage the background daemon with:  fffctl list | restart | stop --all
EOF
}

main "$@"
