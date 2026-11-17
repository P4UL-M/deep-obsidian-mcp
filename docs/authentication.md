# Authentication and credentials

Choose the recovery procedure for the credential you lost:

| Credential | Used for | Recovery |
|---|---|---|
| Classic bearer token | Direct HTTP MCP access; classic OAuth consent password | Recover from its configured source, or replace it and restart the service |
| OAuth access/refresh token | One client's authorization | Reconnect that client and complete consent again |
| Application user password | Multiuser OAuth login or stdio identity | Generate a replacement with `users rotate-password NAME` |
| CouchDB/Algolia credential | Backend storage connection | Use its provider's recovery procedure, then update the configured secret source |

Linking the CLI selects a backend using trusted Docker/host access. It does not
log an ordinary application user into MCP or create a CouchDB account.

## Find where a token comes from

```sh
deep-obsidian-cli secrets check
deep-obsidian-cli print-config
```

`secrets check` names the store, service/account or encrypted-file ID for each
configured reference. Neither command prints secret values. A config may use
custom reference names; use the names actually reported.

The service environment variable `DEEP_OBSIDIAN_AUTH_TOKEN` overrides the
configured bearer reference. Docker injects this from its mounted secret file.
A successful stored-reference check does not prove that the stored value is
the token the running service uses.

## Recover a lost bearer token

### Docker

In the supplied Compose deployment, the host file is `./secrets/auth_token` and
is mounted at `/run/secrets/auth_token`. Read the original host file, or run this
on the Docker host (replace the container name):

```sh
docker exec CONTAINER_NAME cat /run/secrets/auth_token
```

With a remote Docker context:

```sh
docker --context watson exec CONTAINER_NAME cat /run/secrets/auth_token
```

These commands display the credential. Read it in your terminal and save it in
your password manager. Custom `DO_SECRETS_DIR` deployments may use another path.

### macOS Keychain

If `secrets check` reports the default Keychain reference, retrieve it with:

```sh
security find-generic-password \
  -s deep-obsidian-mcp -a http-auth-token -w
```

macOS may ask you to unlock/approve access to Keychain. For a custom reference,
substitute the reported service and account. Keychain Access can also display
the matching entry after authentication.

### Encrypted-file fallback or service environment

There is currently no `secrets get`/`show` command. `print-config --no-redact`
still shows references; it does not resolve and reveal their values.

If the token was supplied through a service manager or secret manager, recover
it there. Otherwise replace the stored token using the procedure below. A
missing original token does not require recreating the vault or index.

## Replace a classic bearer token

For an existing stored `auth.tokenRef`:

```sh
deep-obsidian-cli secrets set --target auth-token
```

Enter a strong replacement from your password manager at the masked prompt.
For scripts, `--stdin` reads the replacement's first line. The command updates
the existing reference's store; it does not generate a new token automatically,
change the config, or move the value to a different store.

Restart the native backend after replacing its token, then update bearer clients
and reconnect OAuth clients as needed. `setup-service --auth` provisions an
initial token; rerunning it does not reveal or rotate an existing one.

For Docker, change the original mounted secret source and recreate the backend
container using your deployment's permissions. Do not rotate only the
container's temporary credential store: the entrypoint re-injects the mounted
source on boot. Service environment overrides must likewise be changed at their
source.

## OAuth and application users

A lost OAuth token is recovered by reconnecting the client. Access tokens and
refresh tokens are client credentials, not the server's classic password.
For setup and client behavior, see [OAuth](oauth.md).

With multiuser enabled, log in using the application username and generated
password. Password verifiers cannot reveal the original password. The operator
CLI generates a replacement:

```sh
deep-obsidian-cli users rotate-password NAME
```

It displays the new password once and invalidates that user's old password and
existing authorizations. Account creation, role assignment and destructive
opt-out are covered in [Multiuser](multiuser.md).

Detailed settings and secret-store behavior remain in
[Authentication](../CONFIGURATION.md#authentication) and
[Secrets are references](../CONFIGURATION.md#secrets-are-references-never-values).
