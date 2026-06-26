#!/usr/bin/env bash
# Build the Debian/Ubuntu .deb for deep-obsidian-mcp using cargo-deb.
#
# Usage:
#   scripts/build-deb.sh [deb-version]
#
# - With no argument the package version comes from Cargo.toml (e.g. 0.1.0).
# - Pass a version (e.g. the release tag without the leading "v", such as
#   0.1.0-alpha.11) to stamp the .deb with that version.
#
# Requirements: a Linux host (or container) with a Rust toolchain. cargo-deb is
# installed automatically if missing. The produced .deb lands in
# target/debian/. This script does NOT run on macOS targets — cargo-deb builds a
# package for the host platform, so run it on Linux (or in CI).
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

DEB_VERSION="${1:-}"

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "warning: cargo-deb builds for the host platform; run this on Linux to produce an installable .deb." >&2
fi

if ! command -v cargo-deb >/dev/null 2>&1; then
  echo "cargo-deb not found; installing..." >&2
  cargo install cargo-deb --locked
fi

# Build the release binary first so cargo-deb packages an optimized build.
cargo build --release -p deep-obsidian-cli --bin deep-obsidian-mcp

ARGS=(deb -p deep-obsidian-cli --no-build)
if [[ -n "$DEB_VERSION" ]]; then
  ARGS+=(--deb-version "$DEB_VERSION")
fi

echo "Running: cargo ${ARGS[*]}" >&2
cargo "${ARGS[@]}"

echo
echo "Built packages:"
ls -1 target/debian/*.deb
