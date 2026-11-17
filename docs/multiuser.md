# Multiuser and linked management

`deep-obsidian-mcp` is the service process. `deep-obsidian-cli` manages it. Docker images, Homebrew and Debian packages install both executables. Existing storage connections and CouchDB/LiveSync credentials remain configured through the existing mounts and secrets commands.

Multiuser is opt-in. Without it, the service retains classic access to all configured mounts, with its existing bearer/OAuth authentication and no username.

## Link the CLI to a backend

```sh
deep-obsidian-cli link
deep-obsidian-cli link --container deep-obsidian --context default
deep-obsidian-cli link --local-config /absolute/path/config.json
deep-obsidian-cli link --status
deep-obsidian-cli unlink
```

Discovery uses the selected Docker context and known local configurations. One candidate is selected automatically; multiple candidates require an interactive selection or an explicit target. Remote Docker contexts work through Docker's existing connection configuration.

The client profile lives in `$XDG_CONFIG_HOME/deep-obsidian-mcp/client.json` (normally `~/.config/deep-obsidian-mcp/client.json`). It contains routing metadata, not users or backend secrets. Explicit `--config PATH` or `--local` overrides a saved link. An unavailable saved target fails without falling back to a different backend. Client installation flags stay local.

Linked Docker commands execute the CLI inside the container, with stdin, arguments and exit status preserved. Paths in backend commands refer to the container. For example, an enablement plan must be readable inside that container. Direct execution is equivalent:

```sh
docker exec -it deep-obsidian deep-obsidian-cli \
  --config /var/lib/deep-obsidian-mcp/config.json users enable --wizard
```

Docker/host access is trusted operator access; linking does not authenticate an ordinary application user.

## Enable and manage users

Use `deep-obsidian-cli users enable --wizard` for an existing configuration. The setup wizard also offers multiuser activation after configuring mounts and authentication. It asks for at least one user, a role or no access on each mount, and a default destination. Each mount exposed to users needs an owner. The final confirmation commits the entire identity/grant plan; cancellation or dry-run creates no users or passwords.

For HTTP, configure OAuth in the existing setup wizard first. Multiuser login uses the application username and generated password in the OAuth consent page. The classic all-vault bearer is refused while multiuser is enabled. For stdio, supply `DEEP_OBSIDIAN_USERNAME` and `DEEP_OBSIDIAN_PASSWORD` to the service process; it authenticates once and checks current permissions on each request. Classic stdio requires neither variable.

```sh
deep-obsidian-cli users status
deep-obsidian-cli users list
deep-obsidian-cli users add laura
deep-obsidian-cli users grant laura --mount shared --role reader
deep-obsidian-cli users grant laura --mount laura-private --role owner
deep-obsidian-cli users default laura --mount laura-private
deep-obsidian-cli users show laura
deep-obsidian-cli users revoke laura --mount shared
deep-obsidian-cli users rotate-password laura
deep-obsidian-cli users remove laura
```

Creation and rotation generate a strong password and display it once in the explicit CLI response. Store it when displayed. Passwords are not accepted as CLI arguments or recoverable from storage. Rotation through the CLI invalidates the old password and all of that user's existing authorizations. New users have no access until granted a role.

| Role | Access |
|---|---|
| reader | Browse, read, search and supported artifact/history reads |
| editor | Reader plus create, edit, upload and supported recoverable deletion |
| owner | Editor plus manage that vault's memberships and rebuild indexes |

Permissions apply to entire mounts and intersect with backend capabilities; an owner cannot make a read-only backend writable. Membership changes take effect on subsequent authorization decisions, including existing OAuth and stdio sessions. The MCP tool `set_vault_membership` lets an authenticated owner assign an existing user a role on their own mount, or remove it with `role: "none"`. Account creation and password rotation remain operator CLI actions. Grant the successor ownership before removing the previous owner; the last owner cannot be removed or demoted.

Search and resource enumeration use only authorized mounts. Existing logical paths remain valid. Write tools also accept `mountId` to interpret `path` or `folder` relative to a permitted mount; `mountId: "default"` uses the user's default. Session capture without an explicit path/folder uses the default mount, or the sole permitted mount; an ambiguous destination is refused.

OAuth clients can POST their access or refresh token as a form field named `token` to `/revoke`, advertised in authorization-server metadata. This revokes that authorization family independently of the user's other clients. Existing refresh rotation/replay protection remains in place. Tokens never exceed current user grants.

For noninteractive enablement, `users enable --plan /path/users.json --yes` accepts an array without passwords:

```json
[
  {
    "name": "paul",
    "default_mount": "private",
    "grants": [
      {"mount_id": "private", "role": "owner"},
      {"mount_id": "shared", "role": "owner"}
    ]
  },
  {
    "name": "laura",
    "default_mount": "shared",
    "grants": [{"mount_id": "shared", "role": "reader"}]
  }
]
```

Use `--dry-run` to inspect the request without creating accounts. Do not put generated passwords in the plan.

## Server state and backups

Mount definitions in `config.json` remain authoritative for storage connections. Users, Argon2 password verifiers, grants, credential generations, hashed local sessions and audit metadata live in `control.sqlite`. Back up this database as authoritative state, using a SQLite-consistent backup or stopping the service before copying it. Indexes remain derived data shared by authorized users of each mount.

The state directory is `DEEP_OBSIDIAN_STATE_DIR`, then `DO_STATE_DIR`, then the configuration file's directory (or the index directory when no config file is used). Docker defaults to the persistent `/var/lib/deep-obsidian-mcp` volume. Keep config, control state and indexes outside filesystem vaults. Multiuser rejects overlapping physical filesystem roots; private vaults need separate storage roots or databases.

The CLI uses a private Unix-domain control socket for a running Linux/macOS service. When the service is stopped, it locks the same state and manages it offline. It never exposes an HTTP operator API. An unreachable live control socket causes an error rather than a second writer bypassing the service. Windows supports offline management; live management requires a Unix backend, including a linked Linux Docker container.

Activation writes a private `multiuser-mode.json` override beside the database and mirrors `multiuser.enabled` into writable configuration files. The override supports read-only bind-mounted configs and is authoritative when present. Back it up with `control.sqlite`. A config/database mode mismatch fails closed. Use the CLI for transitions rather than editing mode flags manually. Container recreation must retain the state volume.

## Disable multiuser

```sh
deep-obsidian-cli users disable
# Or, for an explicitly confirmed noninteractive operation:
deep-obsidian-cli users disable --yes
```

Disabling deletes all application users, grants and sessions, invalidates user OAuth tokens, and immediately restores classic access to all configured mounts with no username. It preserves vault contents, databases, mount configuration and indexes. Re-enabling starts with new application identities and assignments.

CouchDB accounts, database policies and direct LiveSync connections are unchanged. Two people syncing the same LiveSync database share the same vault; MCP grants do not create private folders inside that database or enforce read-only access on direct sync.
