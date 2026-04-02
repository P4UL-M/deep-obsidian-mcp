# Homebrew Packaging Gaps

Date: 2026-04-02

This document tracks what is still missing before `deep-obsidian-mcp` can be treated as a finished Homebrew package and service.

## Missing Packaging Work

- [ ] Publish a real release source tarball or bottle artifact for the Rust build.
- [ ] Replace the placeholder `url` and `sha256` in [Formula/deep-obsidian-mcp.rb](/Users/paul.mairesse/Documents/Playground/deep-obsidian-mcp/Formula/deep-obsidian-mcp.rb).
- [ ] Validate `brew install deep-obsidian-mcp` from a clean machine without using the developer checkout.
- [ ] Decide whether the formula installs from source with Cargo or from a prebuilt release artifact.
- [ ] Document the supported macOS architectures and the bottle strategy.

## Missing Service Validation

- [ ] Validate `brew services start deep-obsidian-mcp` against the packaged binary, not the checkout script flow.
- [ ] Confirm the service starts with a config file only and does not rely on shell-only environment setup.
- [ ] Validate stop, restart, and upgrade behavior with a persisted config and an existing SQLite index.
- [ ] Confirm predictable log paths and retention behavior under Homebrew service management.

## Missing User Experience Decisions

- [ ] Finalize the default config location for a Homebrew install and document it as stable.
- [ ] Finalize the default index location for packaged installs and document ownership and writability expectations.
- [ ] Decide how embedding credentials should be supplied for `brew services` users.
- [ ] Document the upgrade path when the config schema changes.
- [ ] Document uninstall behavior, especially whether config, logs, and index data are preserved or removed.

## Missing Formula Hardening

- [ ] Replace the placeholder formula test with a real smoke test against the packaged binary.
- [ ] Decide whether `ripgrep` remains a runtime dependency or becomes an optional degraded-mode dependency.
- [ ] Confirm the formula versioning and release/tagging policy.
- [ ] Decide where any packaged examples or config templates should live, if any.

## Exit Condition

This gap list is complete when a user can install the formula from the tap, run `deep-obsidian-mcp setup-service --vault ~/Vault`, start it with `brew services`, and validate it with `doctor` and `probe` without touching the source checkout.
