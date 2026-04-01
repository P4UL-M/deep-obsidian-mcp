# Release Checklist

Use this checklist when preparing a Homebrew-ready release artifact.

## Build And Verify

- [ ] Build the Node package successfully.
- [ ] Confirm the service CLI and config resolution match the plan in `docs/brew-service-refactor-plan.md`.
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

The checklist is intentionally stricter than the current implementation. Some items will remain manual until the Node service refactor lands the new command surface and the formula stops depending on the developer checkout layout.
