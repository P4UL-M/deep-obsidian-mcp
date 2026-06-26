#!/usr/bin/env bash
# Build a signed, flat APT repository (suitable for static hosting on GitHub
# Pages) from a directory of .deb files.
#
# Usage:
#   scripts/build-apt-repo.sh <deb-dir> <out-dir> <gpg-key-id>
#
# Env:
#   SUITE              repository suite/codename (default: stable)
#   COMPONENT          repository component (default: main)
#   APT_GPG_PASSPHRASE passphrase for the signing key, if it has one
#
# Requires: dpkg-dev (dpkg-scanpackages), apt-utils (apt-ftparchive), gnupg.
# Produces <out-dir>/ with: pool/, dists/<suite>/..., and the armored public
# key as deep-obsidian-mcp.gpg at the root.
set -euo pipefail

DEB_DIR="${1:?usage: build-apt-repo.sh <deb-dir> <out-dir> <gpg-key-id>}"
OUT="${2:?missing <out-dir>}"
KEYID="${3:?missing <gpg-key-id>}"
SUITE="${SUITE:-stable}"
COMPONENT="${COMPONENT:-main}"
ARCHES=(amd64 arm64)

DEB_DIR="$(cd "$DEB_DIR" && pwd)"
rm -rf "$OUT"
mkdir -p "$OUT/pool/$COMPONENT"
cp "$DEB_DIR"/*.deb "$OUT/pool/$COMPONENT/"

cd "$OUT"

for arch in "${ARCHES[@]}"; do
  dir="dists/$SUITE/$COMPONENT/binary-$arch"
  mkdir -p "$dir"
  # Filename fields come out relative to the repo root (pool/<component>/...).
  dpkg-scanpackages -a "$arch" --multiversion "pool/$COMPONENT" > "$dir/Packages"
  gzip -9kf "$dir/Packages"
done

apt-ftparchive \
  -o "APT::FTPArchive::Release::Origin=deep-obsidian-mcp" \
  -o "APT::FTPArchive::Release::Label=deep-obsidian-mcp" \
  -o "APT::FTPArchive::Release::Suite=$SUITE" \
  -o "APT::FTPArchive::Release::Codename=$SUITE" \
  -o "APT::FTPArchive::Release::Components=$COMPONENT" \
  -o "APT::FTPArchive::Release::Architectures=${ARCHES[*]}" \
  release "dists/$SUITE" > "dists/$SUITE/Release"

GPG=(gpg --batch --yes --default-key "$KEYID")
if [[ -n "${APT_GPG_PASSPHRASE:-}" ]]; then
  GPG+=(--pinentry-mode loopback --passphrase "$APT_GPG_PASSPHRASE")
fi

# Detached + inline signatures over Release.
"${GPG[@]}" --armor --detach-sign -o "dists/$SUITE/Release.gpg" "dists/$SUITE/Release"
"${GPG[@]}" --clearsign -o "dists/$SUITE/InRelease" "dists/$SUITE/Release"

# Publish the public key (armored) for clients to fetch and dearmor.
gpg --armor --export "$KEYID" > "deep-obsidian-mcp.gpg"

cat > index.html <<'HTML'
<!doctype html>
<meta charset="utf-8">
<title>deep-obsidian-mcp APT repository</title>
<h1>deep-obsidian-mcp APT repository</h1>
<p>Install instructions:
<a href="https://github.com/P4UL-M/deep-obsidian-mcp/blob/main/docs/debian-package.md">docs/debian-package.md</a>.</p>
HTML

echo "APT repo built at: $OUT"
