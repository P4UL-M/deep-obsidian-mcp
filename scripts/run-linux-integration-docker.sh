#!/usr/bin/env bash
# Local Linux integration + installation test via Docker.
#
# Builds the .deb from the current HEAD inside a Debian bookworm Rust
# container (so the produced package has the lowest practical glibc floor),
# then installs it and runs scripts/linux-smoke-test.sh in clean containers
# for each target image.
#
# Usage:
#   scripts/run-linux-integration-docker.sh [--published] [--images "img1 img2 ..."]
#
#   --published   Also test the *published* APT repo end to end: run the
#                 GitHub Pages install.sh one-liner in each image instead of
#                 (in addition to) the locally built .deb.
#   --images      Space-separated docker images to test against.
#                 Default: "debian:12 ubuntu:24.04 ubuntu:22.04"
#
# Requirements: docker, a committed HEAD (the build uses `git archive HEAD`).
# Cargo caches persist in the named volumes dobs-cargo-registry / dobs-target,
# so re-runs only rebuild changed crates.
#
# Logs land in outputs/linux-integration/. Exit code 0 = everything passed.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

IMAGES="debian:12 ubuntu:24.04 ubuntu:22.04"
TEST_PUBLISHED=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --published) TEST_PUBLISHED=1; shift ;;
    --images) IMAGES="${2:?--images needs a value}"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

BUILD_IMAGE="rust:1-bookworm"
OUT_DIR="$ROOT_DIR/outputs/linux-integration"
DEB_DIR="$OUT_DIR/debs"
mkdir -p "$DEB_DIR"
rm -f "$DEB_DIR"/*.deb

FAILED=()
note() { echo; echo "==> $*"; }

note "Building the .deb from HEAD in $BUILD_IMAGE (cached volumes: dobs-cargo-registry, dobs-target)"
git archive --format=tar HEAD | docker run --rm -i \
  -v dobs-cargo-registry:/usr/local/cargo/registry \
  -v dobs-target:/build \
  -v "$DEB_DIR:/out" \
  "$BUILD_IMAGE" bash -c '
    set -euo pipefail
    mkdir /src && cd /src && tar xf -
    export CARGO_TARGET_DIR=/build
    command -v cargo-deb >/dev/null || cargo install cargo-deb --locked
    bash scripts/build-deb.sh "$(grep -m1 "^version" Cargo.toml | cut -d\" -f2)-localtest"
    cp "$CARGO_TARGET_DIR"/debian/*.deb /out/
  ' 2>&1 | tee "$OUT_DIR/build-deb.log" | tail -5

DEB_FILE=$(ls -1 "$DEB_DIR"/*.deb | head -1)
note "Built: $DEB_FILE"
docker run --rm -v "$DEB_DIR:/debs:ro" debian:12 dpkg-deb -f "/debs/$(basename "$DEB_FILE")" Package Version Architecture Depends

run_in_image() {
  local image="$1" mode="$2" log_slug
  log_slug="$(echo "$image-$mode" | tr ':/' '--')"
  local log="$OUT_DIR/test-$log_slug.log"
  note "[$image / $mode] installing and running the smoke test (log: $log)"

  local install_cmd
  if [[ "$mode" == published ]]; then
    install_cmd='curl -fsSL https://p4ul-m.github.io/deep-obsidian-mcp/install.sh | bash'
  else
    install_cmd='apt-get install -y /debs/*.deb'
  fi

  if docker run --rm \
      -v "$ROOT_DIR/scripts/linux-smoke-test.sh:/smoke-test.sh:ro" \
      -v "$DEB_DIR:/debs:ro" \
      "$image" bash -c "
        set -euxo pipefail
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        apt-get install -y -qq curl ca-certificates systemd >/dev/null
        $install_cmd
        bash /smoke-test.sh
      " > "$log" 2>&1; then
    echo "    PASS: $image ($mode)"
  else
    echo "    FAIL: $image ($mode) — see $log"
    tail -15 "$log" | sed 's/^/      /'
    FAILED+=("$image ($mode)")
  fi
}

for image in $IMAGES; do
  run_in_image "$image" local-deb
  if [[ "$TEST_PUBLISHED" == 1 ]]; then
    run_in_image "$image" published
  fi
done

echo
if [[ ${#FAILED[@]} -eq 0 ]]; then
  echo "ALL PASS ($IMAGES)"
else
  echo "FAILURES: ${FAILED[*]}"
  exit 1
fi
