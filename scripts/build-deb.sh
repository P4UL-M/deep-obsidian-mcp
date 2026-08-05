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
# Requirements: a Linux host (or container) with a Rust toolchain, plus Node >= 20 to
# build the LiveSync sidecar bundle the package ships. cargo-deb is installed
# automatically if missing. The produced .deb lands in target/debian/. This script does
# NOT run on macOS targets — cargo-deb builds a package for the host platform, so run
# it on Linux (or in CI).
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

# ---------------------------------------------------------------------------
# The LiveSync sidecar bundle
# ---------------------------------------------------------------------------
# Built BEFORE cargo-deb runs, because the bundle is a declared asset in
# rust/crates/deep-obsidian-cli/Cargo.toml and cargo-deb aborts on a missing asset.
#
# The failure below is deliberately loud and fatal rather than a skip. A .deb built
# without the bundle would install and serve every filesystem mount perfectly, and fail
# only when someone configures a couchdb mount — at which point the package is already
# published and the error looks like a user misconfiguration.
SIDECAR_DIR="$ROOT_DIR/sidecar/livesync-sidecar"
SIDECAR_BUNDLE="$SIDECAR_DIR/dist/sidecar.mjs"

node_major() {
  command -v node >/dev/null 2>&1 || return 1
  node --version 2>/dev/null | sed -E 's/^v([0-9]+).*/\1/'
}

if [[ -f "$SIDECAR_BUNDLE" ]]; then
  echo "sidecar bundle already built: $SIDECAR_BUNDLE" >&2
else
  major="$(node_major || true)"
  if [[ -z "$major" ]]; then
    echo "error: node was not found, and the .deb ships the LiveSync sidecar bundle." >&2
    echo "       Install Node >= 20 (the sidecar's engines floor), or pre-build the bundle with:" >&2
    echo "         cd sidecar/livesync-sidecar && npm ci && npm run build" >&2
    exit 1
  fi
  if (( major < 20 )); then
    echo "error: node $major is below the sidecar's floor of 20 (sidecar/livesync-sidecar/package.json)." >&2
    echo "       Install Node >= 20, or pre-build dist/sidecar.mjs with a newer Node." >&2
    exit 1
  fi
  echo "Building the LiveSync sidecar bundle with node $(node --version)..." >&2
  ( cd "$SIDECAR_DIR" && npm ci && npm run build )
fi

# Fail closed: a truncated or empty bundle must not be packaged.
if [[ ! -s "$SIDECAR_BUNDLE" ]]; then
  echo "error: $SIDECAR_BUNDLE is missing or empty after the build step." >&2
  exit 1
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
ls -1 "${CARGO_TARGET_DIR:-target}"/debian/*.deb
