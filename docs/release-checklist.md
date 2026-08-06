# Release Checklist

Use this checklist when preparing a Homebrew-ready release artifact.

## Cutting a Release (`vX.Y.Z`)

Do **all** of these — the two Homebrew formula copies and the apt tap are easy to miss.

1. [ ] **CHANGELOG.md** — add the new version section on `main`.
2. [ ] **Push the tag** `vX.Y.Z` on `main`. The `release-deb` workflow then builds the `.deb` for **amd64 + arm64**, signs and publishes the APT repo to GitHub Pages, and attaches both `.deb`s to the GitHub Release.
   - Requires repo secret `APT_GPG_PRIVATE_KEY`, and the `github-pages` environment must allow `v*` tag deploys.
3. [ ] **This repo's `Formula/deep-obsidian-mcp.rb`** — bump `url` + `sha256` + `version` (sha256 of the tag source tarball). Canonical copy, but **not** what `brew install` uses.
4. [ ] **Separate tap repo `P4UL-M/homebrew-tap` → `Formula/deep-obsidian-mcp.rb`** — bump the same `url`/`sha256`/`version`. **`brew tap P4UL-M/tap` installs from here, not from this project's `Formula/` dir.** Skipping it leaves `brew upgrade` on the old version. Direct commit to the tap's default branch is the normal process.
5. [ ] **Verify live:** the GitHub Release has both `.deb`s; `https://p4ul-m.github.io/deep-obsidian-mcp/install.sh` returns 200; the tap formula shows the new version.

## Build And Verify

- [ ] Build the Rust workspace successfully with `cargo build --release -p deep-obsidian-cli --bin deep-obsidian-mcp`.
- [ ] Confirm the service CLI and config resolution match the maintained behavior contract in [behavior-contract.md](./behavior-contract.md).
- [ ] Verify `setup-service` can persist a config file without editing a plist.
- [ ] Verify `doctor` reports the resolved config, vault path, and writable index directory.
- [ ] Verify `probe` succeeds against a running HTTP service.

## Package

- [ ] Produce a release artifact that does not require a developer checkout.
- [ ] Confirm the artifact layout matches the Homebrew formula expectations.
- [ ] Confirm the formula knows where to find the executable, support files, and service wrapper.
- [ ] Confirm `rg` and any native dependencies are either bundled or declared explicitly.

## The LiveSync Sidecar Bundle

Needed only by the **experimental `couchdb` mount kind**. Every other mount — including
the default filesystem vault — runs with no Node and no bundle.

**The one contract, for every channel.** The binary finds the bundle relative to its own
location: from `<prefix>/bin/deep-obsidian-mcp` it walks up to `<prefix>` and looks for
`share/deep-obsidian-mcp/sidecar/livesync-sidecar/dist/sidecar.mjs`
(`PACKAGED_BUNDLE_PREFIX` in `rust/crates/deep-obsidian-backend/src/sidecar.rs`). That
path is Homebrew's `pkgshare` and the `.deb`'s `/usr/share/deep-obsidian-mcp` at the same
time, so **no channel sets `DEEP_OBSIDIAN_LIVESYNC_SIDECAR`** — not the systemd unit, not
the `brew services` plist. The env var stays a user-facing override.

- [ ] **`.deb` — automatic.** `scripts/build-deb.sh` runs `npm ci && npm run build` in
      `sidecar/livesync-sidecar` before invoking cargo-deb, which declares the bundle as
      an asset. The script **fails loudly** if Node is absent or older than 20 rather
      than producing a package whose couchdb mounts cannot start. In CI,
      `actions/setup-node@v4` supplies Node 20 — `rust:1-bookworm` has none, and
      bookworm's apt `nodejs` is 18, below the floor.
- [ ] **`.deb` — verify** the `release-deb` run shows `Recommends: nodejs (>= 20)` and
      **not** a `Depends` on nodejs, and that `scripts/linux-smoke-test.sh` passes its
      "Sidecar bundle is discoverable by the binary's own probe" step. That step runs
      `doctor` from `/` against a hand-written couchdb mount config, so it proves the
      probe finds the *packaged* copy rather than a source-checkout one.
- [ ] **Homebrew — NOT shipped, deliberately.** `sidecar/livesync-sidecar/dist/` is
      gitignored, so the release tarball has no bundle, and building it needs `npm ci`
      network access that Homebrew's install sandbox restricts. The formula's caveats tell
      a couchdb user how to build it and where to drop it. Confirm the caveats survive any
      formula edit.
  - **Follow-up (not done):** have the release workflow build the bundle, attach it to the
    GitHub Release, and add a `resource` block to both formula copies. This cannot be done
    in the same commit that introduces it — a formula needs the asset's `sha256`, which
    does not exist until the release is cut.

## The Container Image (not published yet)

The `docker` job in `.github/workflows/ci.yml` builds the image natively on
`ubuntu-24.04` + `ubuntu-24.04-arm` and runs `scripts/docker-smoke-test.sh` against
it on every PR that touches `Dockerfile`, `docker/**`, `rust/**` or `sidecar/**`. It
publishes **nothing**: the GHCR steps are present but commented out, pending the
decision to start releasing images. Full deployment docs: [docker.md](./docker.md).

- [ ] **Decide whether this release publishes an image.** If yes, uncomment the two
      GHCR steps at the bottom of the `docker` job and add `packages: write` to the
      workflow's `permissions`.
- [ ] **Tag policy, same shape as the `.deb` channel:** `ghcr.io/p4ul-m/deep-obsidian-mcp:vX.Y.Z`
      plus `:latest`, on `v*` tags only — never from a branch or a PR.
- [ ] **Multi-arch is a manifest list, not a `platforms:` build.** The CI job uses
      `load: true` and is single-arch per runner on purpose (QEMU would make the Rust
      release build take tens of minutes, and a cross-built image cannot be
      smoke-tested on the builder). Publishing therefore means pushing one digest per
      runner and joining them with `docker/metadata-action` + a `merge` job — not
      switching the existing job to `platforms: linux/amd64,linux/arm64`.
- [ ] **The bundle, in this channel: automatic.** A dedicated `node:20-slim` stage
      runs `npm ci && npm run build` and the runtime stage copies `dist/sidecar.mjs`
      to `/opt/deep-obsidian-mcp/share/deep-obsidian-mcp/...`, i.e. to the same
      exe-relative path the `.deb` uses. `scripts/docker-smoke-test.sh` asserts
      `doctor` (run from `/`) finds it there, which is the parity check that keeps the
      second build path honest.
- [ ] **If the image is published, say in the release notes** that it requires a
      bearer-token secret (it refuses to start otherwise), terminates no TLS, and
      starts *degraded* against a CouchDB database no Obsidian client has synced yet.

## Homebrew Smoke Test

- [ ] `brew install <formula>`
- [ ] `deep-obsidian-mcp setup-service --vault <vault>`
- [ ] `brew services start deep-obsidian-mcp`
- [ ] `deep-obsidian-mcp doctor`
- [ ] `deep-obsidian-mcp probe`

## Multi-Backend Status: EXPERIMENTAL

The `couchdb` and `algolia` mount kinds, and multi-mount configs generally, are gated
behind per-risk `experimental` flags (`multiVault`, `couchdbVaults`, `algoliaVaults`) and
must be described as experimental in release notes.

- [ ] Release notes state that multi-backend mounts are experimental and configured by
      hand: `setup-service` **refuses to rewrite a declared mount table** (and refuses an
      auth change on one, because it cannot write the reference the token would need).
- [ ] Release notes point at [migration-and-rollback.md](./migration-and-rollback.md) for
      how to move a folder onto a remote backend **and how to get back off it**.
- [ ] An `algolia` mount sends note bodies to a hosted third-party service; a `writable`
      one is a corpus several people write concurrently. Both are worth naming in notes.

## Upgrade Checks

- [ ] Service restarts cleanly after formula upgrade.
- [ ] Config file is preserved across upgrades.
- [ ] Index directory survives upgrade and restart.
- [ ] Health endpoint and MCP endpoint remain stable.
- [ ] **Downgrade safety.** A config written by a newer build, rewritten by this one,
      keeps its unknown top-level and per-mount keys (`UnknownFields` in
      `deep-obsidian-types`). Known gap: keys inside a `backend` object are still dropped —
      see the policy doc on `UnknownFields`. Adding a backend option is therefore a
      forward-incompatible change to guard in release notes.
- [ ] **Config rollback.** A content-changing `setup-service` write leaves the previous
      file at `config.json.bak`. Restoring it is the documented rollback; see
      [migration-and-rollback.md](./migration-and-rollback.md).

## Notes

The checklist is intentionally stricter than the current implementation. Some items will remain manual until the Rust release packaging flow is finished and the formula stops depending on placeholder artifact metadata.
