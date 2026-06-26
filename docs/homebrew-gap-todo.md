# Homebrew Packaging Gaps

Date: 2026-04-02

This document tracks what is still missing before `deep-obsidian-mcp` can be treated as a finished Homebrew package and service.

## Missing Packaging Work

- [ ] Publish a real release source tarball or bottle artifact for the Rust build.
- [ ] Replace the placeholder `url` and `sha256` in [Formula/deep-obsidian-mcp.rb](../Formula/deep-obsidian-mcp.rb).
- [ ] Validate `brew install deep-obsidian-mcp` from a clean machine without using the developer checkout.
- [ ] Decide whether the formula installs from source with Cargo or from a prebuilt release artifact.
- [ ] Document the supported macOS architectures and the bottle strategy.

## Service Validation

- [x] Formula service runs the packaged binary with `serve --packaged --transport http`.
- [x] Formula service sets `DEEP_OBSIDIAN_PACKAGED=1` so default indexes stay outside the vault.
- [x] Predictable Homebrew log paths are declared under `var/log/deep-obsidian-mcp/`.
- [ ] Validate `brew services start deep-obsidian-mcp` from an installed tap on a clean machine.
- [ ] Validate stop, restart, and upgrade behavior with a persisted config and an existing SQLite index.

## User Experience Decisions

- [x] Default config location remains `~/.config/deep-obsidian-mcp/config.json`.
- [x] Packaged default index location is `~/Library/Application Support/deep-obsidian-mcp/indexes/<vault-hash>`.
- [x] Packaged mode is explicit through `--packaged` or `DEEP_OBSIDIAN_PACKAGED=1`.
- [ ] Decide how embedding credentials should be supplied for `brew services` users.
- [ ] Document the upgrade path when the config schema changes.
- [ ] Document uninstall behavior, especially whether config, logs, and index data are preserved or removed.

## Formula Hardening

- [x] Formula smoke test exercises the installed binary with `help` and `version`.
- [ ] Decide whether `ripgrep` remains a runtime dependency or becomes an optional degraded-mode dependency.
- [ ] Confirm the formula versioning and release/tagging policy.
- [ ] Decide where any packaged examples or config templates should live, if any.

## Exit Condition

This gap list is complete when a user can install the formula from the tap, run `deep-obsidian-mcp setup-service --vault ~/Vault`, start it with `brew services`, and validate it with `doctor` and `probe` without touching the source checkout.
