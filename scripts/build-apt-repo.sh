#!/usr/bin/env bash
# Assemble a signed flat APT repository from a directory of .deb files.
# Engine only: all repo identity is env-overridable (config-not-fork).
#
#   build-apt-repo.sh <deb_dir> <out_dir>
#
# Requires: dpkg-dev (dpkg-scanpackages), apt-utils (apt-ftparchive), gpg.
# Signing key is imported from $APT_GPG_PRIVATE_KEY (armored) when set;
# otherwise the repo is built unsigned (local dry-run).
set -euo pipefail

DEB_DIR="${1:?usage: build-apt-repo.sh <deb_dir> <out_dir>}"
OUT_DIR="${2:?usage: build-apt-repo.sh <deb_dir> <out_dir>}"

SUITE="${APT_SUITE:-stable}"
COMPONENT="${APT_COMPONENT:-main}"
ORIGIN="${APT_ORIGIN:-fff}"
LABEL="${APT_LABEL:-fff}"
ARCHES="${APT_ARCHES:-amd64 arm64}"

DISTS="$OUT_DIR/dists/$SUITE"
POOL="$OUT_DIR/pool/$COMPONENT/f/fff"

rm -rf "$OUT_DIR"
mkdir -p "$POOL"
cp "$DEB_DIR"/*.deb "$POOL/"

for arch in $ARCHES; do
  bindir="$DISTS/$COMPONENT/binary-$arch"
  mkdir -p "$bindir"
  # Paths in Packages must be relative to OUT_DIR (the repo root apt fetches).
  ( cd "$OUT_DIR" && dpkg-scanpackages --arch "$arch" "pool/$COMPONENT" ) \
    > "$bindir/Packages"
  gzip -9 -k -f "$bindir/Packages"
done

apt_ftparchive_conf="$(mktemp)"
cat > "$apt_ftparchive_conf" <<EOF
APT::FTPArchive::Release::Origin "$ORIGIN";
APT::FTPArchive::Release::Label "$LABEL";
APT::FTPArchive::Release::Suite "$SUITE";
APT::FTPArchive::Release::Codename "$SUITE";
APT::FTPArchive::Release::Components "$COMPONENT";
APT::FTPArchive::Release::Architectures "$ARCHES";
EOF

apt-ftparchive -c "$apt_ftparchive_conf" release "$DISTS" > "$DISTS/Release"
rm -f "$apt_ftparchive_conf"

if [ -n "${APT_GPG_PRIVATE_KEY:-}" ]; then
  gnupg_home="$(mktemp -d)"
  export GNUPGHOME="$gnupg_home"
  chmod 700 "$gnupg_home"
  echo "$APT_GPG_PRIVATE_KEY" | gpg --batch --import
  keyid="$(gpg --list-secret-keys --with-colons | awk -F: '/^sec:/ {print $5; exit}')"

  gpg_opts=(--batch --yes --local-user "$keyid" --pinentry-mode loopback)
  [ -n "${APT_GPG_PASSPHRASE:-}" ] && gpg_opts+=(--passphrase "$APT_GPG_PASSPHRASE")

  gpg "${gpg_opts[@]}" --clearsign -o "$DISTS/InRelease" "$DISTS/Release"
  gpg "${gpg_opts[@]}" -abs -o "$DISTS/Release.gpg" "$DISTS/Release"

  # Public key users trust: served at repo root as fff.gpg.
  gpg --armor --export "$keyid" > "$OUT_DIR/fff.gpg"
  rm -rf "$gnupg_home"
else
  echo "WARN: APT_GPG_PRIVATE_KEY unset — repo is UNSIGNED (dry-run only)." >&2
fi

# GitHub Pages skips files/dirs under paths starting with an underscore
# unless a .nojekyll marker is present; none here, but keep it defensive.
touch "$OUT_DIR/.nojekyll"

echo "APT repo assembled at $OUT_DIR (suite=$SUITE arches=$ARCHES)"
