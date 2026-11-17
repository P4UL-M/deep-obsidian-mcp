# CLI guide

Use `deep-obsidian-cli` to configure and manage a backend. Use
`deep-obsidian-mcp` to run the HTTP or stdio service.

## Find the right command

```sh
deep-obsidian-cli --help
deep-obsidian-cli doctor --help
deep-obsidian-cli mounts add couchdb --help
deep-obsidian-cli help users grant
```

The main help is an overview. Each command's help shows its own options and
examples. Help runs locally, even if the saved backend is offline. A misspelled
command or option returns an error, including when `--help` is present.

| Task | Command | Details |
|---|---|---|
| Set up a backend | `setup-service --wizard` | [First setup](../USAGE.md#1-set-up-your-vault) |
| Choose an existing backend | `link` | [Target selection](#choose-a-backend) |
| Clear the saved target | `unlink` | Keeps backend configuration and vault data |
| Add, list or remove storage mounts | `mounts` | [Mount configuration](../CONFIGURATION.md#building-the-table-the-mounts-cli) |
| Manage users and vault roles | `users` | [Multiuser](multiuser.md) |
| Check or replace stored credentials | `secrets` | [Authentication and credentials](authentication.md) |
| Diagnose configuration and dependencies | `doctor` | [Diagnostics](#diagnose-a-backend) |
| Check HTTP health and MCP access | `probe` | [Diagnostics](#diagnose-a-backend) |
| Inspect resolved settings | `print-config` | [Configuration](../CONFIGURATION.md#config-file--precedence) |
| Export or restore a LiveSync mount | `couchdb` | [Migration and rollback](migration-and-rollback.md) |
| Seed, snapshot or manage an Algolia corpus | `algolia` | [Algolia](algolia-mounts.md) |
| Inspect installed CLI version | `version` | Version of the local CLI executable |

## Choose a backend

Run `link` when you already have a backend:

```sh
deep-obsidian-cli link
deep-obsidian-cli link --status
deep-obsidian-cli mounts list
```

Discovery lists running Deep Obsidian containers in the current Docker context
and known local configuration files. It selects a sole candidate, asks when
there are several, and requires an explicit choice in noninteractive use.
Native entries are labelled **configured; service not verified**. A running
native process is not discovered by scanning the computer.

Docker must be reachable to list containers. With no explicit Docker selection,
an unavailable Docker context leaves only known local candidates. Use an
explicit context to get an error when Docker cannot be queried:

```sh
docker context ls
docker --context desktop-linux ps
deep-obsidian-cli link --context desktop-linux
deep-obsidian-cli link --context desktop-linux --container CONTAINER_NAME
```

For a remote host, first configure a Docker context using an SSH account that
can access Docker there. Replace `USER` and `HOST` with your SSH destination:

```sh
docker context create watson --docker "host=ssh://USER@HOST"
deep-obsidian-cli link --context watson
```

Discovery does not scan the network or every Docker context. The host CLI and
the target container need a release containing `deep-obsidian-cli` (alpha.9 or
newer). Forwarded commands run the container's own executable; backend paths
refer to that container, including import/export folders and enablement plans.

For native configurations:

```sh
deep-obsidian-cli link --local-config /absolute/path/config.json
```

The saved target lives in `~/.config/deep-obsidian-mcp/client.json`, or under
`$XDG_CONFIG_HOME` when set. It contains routing metadata and known local config
paths. Users and backend credentials live on the backend, not in this profile.
An unavailable saved target fails without redirecting changes elsewhere.

### Override or remove a link

```sh
deep-obsidian-cli --local mounts list
deep-obsidian-cli --config /path/config.json doctor
deep-obsidian-cli unlink
```

An explicit `--config` or `--local` takes precedence over the saved link. With no
link, normal local config resolution applies. Client installation actions
(`--mcp`, `--skills`, `--vault-snippets`) stay on the client machine.

## Set up and change configuration

Start with `setup-service --wizard`, or inspect `setup-service --help` for the
flag-driven flow. It summarizes changes before confirmation; `--dry-run`
previews without writing or creating credentials.

Use `mounts` to change an existing mount table. Mount addition asks for missing
fields on a terminal and validates/probes the result before saving. In scripts,
use explicit flags and `--yes`; credentials use masked prompts or stdin.
Removing a mount leaves its backing data and credentials intact.

For all persisted fields, defaults and precedence, use the
[configuration reference](../CONFIGURATION.md). Startup flags are documented by
`deep-obsidian-mcp --help`; service managers launch that executable directly.

## Diagnose a backend

```sh
deep-obsidian-cli doctor
deep-obsidian-cli doctor --probe-remote
deep-obsidian-cli probe
deep-obsidian-cli print-config
deep-obsidian-cli secrets check
```

`doctor` checks configuration, local vault access and dependencies. Add
`--probe-remote` for read-only checks of remote mounts. `probe` checks the
configured HTTP health and MCP endpoints. `print-config` shows settings and
secret references, not secret values. `secrets check` identifies each reference's
store and reports missing values without revealing them.

Use `--json` where the command supports machine-readable output. Use
`--dry-run` where the command supports a write preview; neither flag changes
which backend is selected.

## Recover a lost bearer token

Follow [token recovery](authentication.md#recover-a-lost-bearer-token) for Docker
secret files, macOS Keychain references and the encrypted-file fallback. Bearer
tokens, OAuth authorizations and multiuser passwords have different recovery
paths; see the [authentication guide](authentication.md).


## Apply mount changes without reconnecting

With an alpha.11-or-newer backend, `mounts add` and `mounts remove` automatically
reload the running backend after saving. The listener, OAuth state and existing
client sessions remain active; requests use the current grants and routing.
There can be a short index warm-up for affected mounts. A rejected reload retains
its previous live table and names the saved change as pending. Apply a corrected
or previously saved configuration with:

```sh
deep-obsidian-cli mounts reload
```

When the backend is stopped, changes are saved for its next startup. Changes to
the root mount, listener or authentication still require a service restart.
`--purge-index` requires a stopped backend; remove a live mount without that flag.
Upgrade the linked container as well as the host CLI before using live reload.
