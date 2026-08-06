# Release Checklist

Use this checklist when preparing a Homebrew-ready release artifact.

Merging a multi-PR stack onto `main` before the tag is a separate procedure:
[stack-merge-procedure.md](./stack-merge-procedure.md). Do that first; everything below
assumes `main` already carries the content being released.

## Where The Version Lives

Bumped **before** the tag, on `main`, and they must all agree with the tag:

| Place | What it is | Who reads it |
|---|---|---|
| `Cargo.toml` → `[workspace.package] version` | the source of truth | `CARGO_PKG_VERSION` → `deep-obsidian-mcp version`; cargo-deb's default package version; `Cargo.lock` (regenerate with any `cargo` command, do not hand-edit) |
| `Formula/deep-obsidian-mcp.rb` → `url`, `sha256`, `version`, and the `livesync-sidecar` resource's `url` + `sha256` | this repo's canonical copy | nothing installs from it (see step 4 below) |
| `P4UL-M/homebrew-tap` → `Formula/deep-obsidian-mcp.rb` | the copy `brew install` actually uses | every Homebrew user |
| `Dockerfile` → `ARG VERSION` | default for the OCI `org.opencontainers.image.version` label | image consumers; CI passes the tag version explicitly when publishing |
| `CHANGELOG.md` → the top section heading | release notes | humans |

Deliberately **not** version-stamped, and not to be "fixed" during a bump:

- **The MCP `serverInfo.version`** returned by `initialize` is a frozen literal
  (`"0.1.0"` in `rust/crates/deep-obsidian-server/src/mcp.rs`), asserted byte-for-byte by
  `rust/crates/deep-obsidian-server/tests/golden/initialize.json`. It is part of the
  frozen black-box MCP contract, not a build stamp; wiring it to `CARGO_PKG_VERSION`
  would make that golden regenerate on every release. Changing it is a contract change
  with its own justification, never a side effect of a version bump.
- **The sidecar's own version** (`sidecar/livesync-sidecar/package.json`, and
  `SIDECAR_VERSION` in `src/protocol.ts`) is the handshake version the Rust supervisor
  pins against, asserted in Rust tests. Nothing in the release flow versions it. The
  release **asset** filename carries the *tag* version instead.

## Cutting a Release (`vX.Y.Z`)

Do **all** of these — the two Homebrew formula copies and the apt tap are easy to miss.

1. [ ] **On `main`, before tagging:**
   - [ ] `Cargo.toml` `[workspace.package] version` = `X.Y.Z` (no leading `v`), and
         `Cargo.lock` regenerated (`cargo metadata >/dev/null` is enough).
   - [ ] `Dockerfile` `ARG VERSION` = the same value.
   - [ ] **CHANGELOG.md** — the top section reads `## vX.Y.Z — <date>`; if it still says
         `PENDING`, replace it with the tag's date.
   - [ ] `cargo test --workspace` green and `cargo clippy --workspace --all-targets`
         no worse than before.
2. [ ] **Push the tag** `vX.Y.Z` on `main`. The `release-deb` workflow then builds the `.deb` for **amd64 + arm64**, signs and publishes the APT repo to GitHub Pages, and attaches both `.deb`s **plus the LiveSync sidecar bundle** (`livesync-sidecar-X.Y.Z.mjs` and its `.sha256`) to the GitHub Release.
   - Requires repo secret `APT_GPG_PRIVATE_KEY`, and the `github-pages` environment must allow `v*` tag deploys.
   - The Pages deploy is historically flaky; if it fails, delete the stale
     `github-pages` artifact and re-run the job rather than re-tagging.
3. [ ] **This repo's `Formula/deep-obsidian-mcp.rb`** — set `version`, the tarball `url` +
   `sha256`, and the `livesync-sidecar` resource's `url` + `sha256`. Canonical copy, but
   **not** what `brew install` uses, so it is the only copy allowed to sit with
   placeholder hashes between a stack merge and a tag.
   - Tarball hash: `curl -sL <url> | shasum -a 256`.
   - Bundle hash: the **first field** of the release's `livesync-sidecar-X.Y.Z.mjs.sha256`
     asset (the workflow's "Name and checksum the asset" step prints the same line).
4. [ ] **Separate tap repo `P4UL-M/homebrew-tap` → `Formula/deep-obsidian-mcp.rb`** — mirror
   the same `url`/`sha256`/`version` **and the whole `resource "livesync-sidecar"` block,
   its `install` staging lines, and the `assert_path_exists` in `test do`** (that assertion
   is what makes `brew test` catch a tap copy whose resource stopped landing).
   **`brew tap P4UL-M/tap` installs from here, not from
   this project's `Formula/` dir.** Skipping it leaves `brew upgrade` on the old version;
   copying it with a placeholder `sha256` breaks `brew install` for **everyone**, not just
   couchdb users — the resource is fetched unconditionally. Never commit a placeholder to
   the tap. Direct commit to the tap's default branch is the normal process.
5. [ ] **Verify live:** the GitHub Release has both `.deb`s **and both sidecar assets**;
   `https://p4ul-m.github.io/deep-obsidian-mcp/install.sh` returns 200; the tap formula
   shows the new version; `brew install P4UL-M/tap/deep-obsidian-mcp` succeeds and
   `<brew --prefix>/share/deep-obsidian-mcp/sidecar/livesync-sidecar/dist/sidecar.mjs`
   exists.

## Build And Verify

- [ ] Build the Rust workspace successfully with `cargo build --release -p deep-obsidian-cli --bin deep-obsidian-mcp`.
- [ ] Confirm the service CLI and config resolution match the maintained behavior contract in [behavior-contract.md](./behavior-contract.md).
- [ ] Verify `setup-service` can persist a config file without editing a plist.
- [ ] Verify `doctor` reports the resolved config, vault path, and writable index directory.
- [ ] Verify `probe` succeeds against a running HTTP service.

## Package

- [ ] Produce a release artifact that does not require a developer checkout.
- [ ] Confirm the artifact layout matches the Homebrew formula expectations, including the
      `livesync-sidecar` resource's staged destination (`pkgshare/sidecar/livesync-sidecar/dist/sidecar.mjs`).
- [ ] Confirm the formula knows where to find the executable, support files, and service wrapper.
- [ ] Confirm `rg` and any native dependencies are either bundled or declared explicitly.
- [ ] **Debian version ordering, for the release *after* a prerelease.** A known defect with
      a chosen remedy — nothing to do for v0.2.0-alpha.1 itself, but it must be settled
      before the first non-prerelease of the line.

      The workflow stamps the .deb with the tag minus its `v` via `--deb-version`, so
      `v0.2.0-alpha.1` becomes the Debian version `0.2.0-alpha.1`, whose `-` dpkg reads as
      the *debian revision* separator. Verified with `dpkg --compare-versions` on
      `debian:12`:

      | comparison | holds? |
      |---|---|
      | `0.1.0-alpha.12 < 0.2.0-alpha.1` | yes — apt upgrades between prereleases |
      | `0.2.0-alpha.1 < 0.2.0` | **no** — a plain `0.2.0` looks like a *downgrade*, so `apt upgrade` would leave prerelease users behind |
      | `0.2.0~alpha.1 < 0.2.0` | yes — `~` sorts *before* the release |
      | `0.1.0-alpha.12 < 0.2.0~alpha.1` | yes — so switching to `~` is safe for existing apt users |

      The mechanism: **cargo-deb already converts prereleases correctly on its own.**
      `scripts/build-deb.sh` with no argument (cargo-deb 3.7.0, verified locally) produces
      `deep-obsidian-mcp_0.2.0~alpha.1-1_<arch>.deb`. The workflow's explicit
      `--deb-version` *overrides* that conversion, which is why published prerelease
      packages carry the trap form — the alpha.12 release's assets are
      `deep-obsidian-mcp_0.1.0-alpha.12_amd64.deb`, with no `-1` revision suffix.

      Remedies, any one of: drop `--deb-version` on prerelease tags and let cargo-deb
      convert; stamp the `~` form explicitly; or give the stable an epoch (`1:0.2.0`).
      Note that all of them change the **asset filename shape** (`~`, plus a revision
      suffix), so make the change on a release where that can be verified without a stack
      merge in flight.

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
- [ ] **Release asset — automatic since v0.2.0-alpha.1.** The `sidecar-bundle` job in
      `release-deb.yml` builds the bundle with Node 20 and uploads
      `livesync-sidecar-<version>.mjs` + `.sha256`; the `publish` job attaches both to the
      GitHub Release next to the `.deb`s. The job runs on **every** trigger (PRs included)
      so the build and the naming are exercised between releases; only the attach step is
      tag-gated. Verify the two assets are on the release, and that the `.mjs` is ~1.5 MB
      rather than empty.
- [ ] **Homebrew — shipped, via that asset.** `sidecar/livesync-sidecar/dist/` is still
      gitignored (the source tarball has no bundle) and `npm ci` still cannot run inside
      Homebrew's install sandbox, so the formula does not build it: a
      `resource "livesync-sidecar"` (`using: :nounzip`, the asset is a plain `.mjs`) is
      staged into `pkgshare/"sidecar/livesync-sidecar/dist/sidecar.mjs"` — the same
      exe-relative path `PACKAGED_BUNDLE_PREFIX` derives for a Cellar keg, asserted in
      `sidecar.rs`'s `bundle_candidates` test. Confirm after any formula edit that:
  - the resource's `sha256` is the real asset hash in the **tap** copy (a wrong or
    placeholder hash fails `brew install` for every user, since resources are fetched
    unconditionally — not just for couchdb users);
  - the caveats still describe the bundle as installed, with a hand-build only for source
    installs;
  - `brew install` then `deep-obsidian-mcp doctor` reports the bundle located at the
    `pkgshare` path (with a couchdb mount declared).

## The Container Image (not published yet)

The `docker` job in `.github/workflows/ci.yml` builds the image natively on
`ubuntu-24.04` + `ubuntu-24.04-arm` and runs `scripts/docker-smoke-test.sh` against
it on every PR that touches `Dockerfile`, `docker/**`, `rust/**` or `sidecar/**`. It
publishes **nothing**: the GHCR steps are present but commented out, pending the
decision to start releasing images. Full deployment docs: [docker.md](./docker.md).

- [ ] **Decide whether this release publishes an image.** For **v0.2.0-alpha.1 the answer
      is no** — the image stays unpublished and the GHCR steps stay commented out. If a
      later release publishes one, uncommenting the two steps is **not sufficient**; all
      four of these are required:
  1. add `packages: write` to the workflow's `permissions`;
  2. add a tag trigger to `ci.yml` — it currently fires on `push: branches: [main]`,
     `pull_request` and `workflow_dispatch` **only**, so the commented steps' own
     `if: startsWith(github.ref, 'refs/tags/v')` guard can never be true as things stand;
  3. pass `build-args: VERSION=${GITHUB_REF_NAME#v}` and `VCS_REF=${{ github.sha }}` to
     the push step — the commented block passes neither, so the published image would
     carry the `Dockerfile`'s default `ARG VERSION` and `VCS_REF=unknown` in its OCI
     labels;
  4. join the two per-runner digests into one manifest list (next item).
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
- [ ] `deep-obsidian-mcp version` prints the tag version without the leading `v`.
- [ ] `deep-obsidian-mcp setup-service --vault <vault>`
- [ ] `brew services start deep-obsidian-mcp`
- [ ] `deep-obsidian-mcp doctor`
- [ ] `deep-obsidian-mcp probe`
- [ ] `test -s "$(brew --prefix)/share/deep-obsidian-mcp/sidecar/livesync-sidecar/dist/sidecar.mjs"`
      — the `resource` landed at the probed path. Nothing else in the install needs it, so
      a silently-missing bundle would only surface for a couchdb user.

## v0.2.0-alpha.1 Specifics

- [ ] **The experimental gates stay ON.** `multiVault`, `couchdbVaults` and
      `algoliaVaults` remain opt-in `experimental` flags for this release; stabilizing any
      of them is post-release work and must not be folded into the tag. Release notes must
      say "experimental", not "beta".
- [ ] **It is a minor bump (`0.1.x` → `0.2.0`) because the config model grew**, not because
      anything filesystem-only changed: a config with no `mounts` table resolves to exactly
      one filesystem mount and every payload stays byte-identical. Say that plainly — an
      operator on a plain filesystem vault should read the notes and conclude nothing is
      required of them.
- [ ] **Forward-incompatibility to name in the notes:** keys inside a `backend` object are
      dropped when an older build rewrites a newer config (see "Downgrade safety" below),
      so a config written by 0.2.0 and rewritten by 0.1.x loses backend options.
- [ ] **Point at the migration doc in the notes**, both directions:
      [migration-and-rollback.md](./migration-and-rollback.md) has "Moving A Folder To A
      CouchDB (LiveSync) Mount" / "Getting Out Of A CouchDB Mount", the same pair for
      Algolia, plus "Config Rollback" and "Downgrading The Binary".

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

The checklist is intentionally stricter than the current implementation. What is genuinely
automated on a `v*` tag: the `.deb` for both architectures, its install + smoke test, the
signed APT repo and its Pages deploy, and the sidecar bundle asset. What stays manual, by
nature rather than by neglect: the two formula copies (a formula cannot know a hash that
does not exist until the tag is cut), the `brew`/`brew services` smoke test on a real
machine, and the upgrade checks. No bottle is published, so every Homebrew install still
builds from source.
