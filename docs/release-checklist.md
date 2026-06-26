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

## Homebrew Smoke Test

- [ ] `brew install <formula>`
- [ ] `deep-obsidian-mcp setup-service --vault <vault>`
- [ ] `brew services start deep-obsidian-mcp`
- [ ] `deep-obsidian-mcp doctor`
- [ ] `deep-obsidian-mcp probe`

## Upgrade Checks

- [ ] Service restarts cleanly after formula upgrade.
- [ ] Config file is preserved across upgrades.
- [ ] Index directory survives upgrade and restart.
- [ ] Health endpoint and MCP endpoint remain stable.

## Notes

The checklist is intentionally stricter than the current implementation. Some items will remain manual until the Rust release packaging flow is finished and the formula stops depending on placeholder artifact metadata.
