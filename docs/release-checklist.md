# Release Checklist

Use this checklist when preparing a Homebrew-ready release artifact.

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
