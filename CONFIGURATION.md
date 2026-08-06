# Configuring deep-obsidian-mcp

- [The setup wizard](#the-setup-wizard)
- [Config file & precedence](#config-file--precedence)
- [Rotating a stored secret](#rotating-a-stored-secret)
- [Semantic search (embeddings)](#semantic-search-embeddings)
- [Authentication](#authentication)
- [Automatic reindexing](#automatic-reindexing)
- [Transport & stdio modes](#transport--stdio-modes)
- [Multiple vaults (mounts)](#multiple-vaults-mounts) — **experimental**

## The setup wizard

```bash
deep-obsidian-mcp setup-service --wizard
```

The first-init flow. [USAGE.md](./USAGE.md#1-set-up-your-vault) lists the six screens;
this section is about the decisions behind them.

**It can produce a mount table, and it asks about remote roots up front.** Screen 1
offers a local folder (the default), a remote LiveSync/CouchDB vault, or a shared
Algolia index — because [any backend may be the root
mount](#a-remote-mount-at-the-vault-root), and a wizard that only ever offered a folder
would make the one path a LiveSync-only user needs look unsupported. A local root with
no further mounts is still written as a plain top-level `vaultPath`, so the common case
produces exactly the file it always did; anything else becomes a `mounts` table.

**Each mount goes through the same sequence `mounts add` does**, in the same order and
with the same wording, because it is [the same
code](#building-the-table-the-mounts-cli): the legacy `vaultPath` is migrated to an
explicit root mount on the first addition, the experimental flag is named and confirmed
rather than assumed, the whole table is re-validated through the server's own loader on
every addition, the credential goes to the secret store as a reference, and the mount is
**probed** before anything is written. A failed probe stops the wizard and asks whether to
keep the mount anyway.

**Nothing is written until you have seen it.** The last screen prints the resolved config
through the same renderer `print-config` uses — credentials appear only as
[references](#secrets-are-references-never-values) — and asks once. The previous file, if
there was one, goes to `config.json.bak`.

**An interrupted run leaves nothing behind.** A probe has to happen *after* the
credential reaches the store (that is what the probe authenticates with), so every later
exit — a failed probe, a declined recap, `Ctrl-D` at any question — deletes the
credentials that run stored and writes no config.

**What it does not do.** It will not edit a config that already declares a mount table:
re-deriving one from a fresh set of answers would silently drop every per-mount setting
the wizard never asks about (`options`, `cache`, `retention`, `recallWeight`, an explicit
`indexDir`). Use `mounts add` / `mounts list` / `mounts remove`, which change a table
without touching the rest of the file. It also does not tune retention, `recallWeight` or
the Algolia cache — those are file edits, documented below.

`--dry-run` walks every screen, validates the config, and stores nothing, probes nothing
and writes nothing.

## Config file & precedence

`setup-service` writes normalized JSON to
`~/.config/deep-obsidian-mcp/config.json` (override with `--config <path>`).
Inspect the resolved config any time with `deep-obsidian-mcp print-config`
(secrets are redacted).

Settings resolve in this order:

1. CLI flags
2. config file
3. environment variables
4. built-in defaults

### Secrets are references, never values

Secrets (embedding API keys, the auth token, a mount's CouchDB password or Algolia
API key) are **never** stored in the config file. The config only holds a
reference; the value lives in the OS keyring when available, or an encrypted local
file as a fallback.

A reference is one of two shapes:

```json
{ "kind": "osKeyring", "service": "deep-obsidian-mcp", "account": "algolia-team-wiki" }
{ "kind": "encryptedFile", "id": "algolia-team-wiki" }
```

This is what makes `print-config` safe by construction rather than by careful
redaction: there is nothing secret in the persisted config to redact. Everything
else in a mount definition — a CouchDB URL and user name, an Algolia app id and
index name — is an identifier, not a credential, and is printed verbatim.

#### What the encrypted-file fallback actually protects against

Be clear-eyed about this one. `~/.config/deep-obsidian-mcp/secrets.json` is encrypted with
XChaCha20-Poly1305 under a **static key compiled into the binary**, and the binary is
public. So the file protects against *accidental* disclosure — a `grep` through your dotfiles,
a backup or a sync client that swallows the whole config directory, a secret scanner in CI, a
config folder committed to a repository by mistake — and it means the credential is not
sitting in plaintext next to a config file people do read. It protects against **nothing**
where someone has local file access: anyone who can read the file can also fetch the key from
the binary and decrypt it. Treat it as obfuscation-plus-integrity, not as a vault.

The **OS keyring is the only genuinely protected store**, and every command prefers it: it
is guarded by the login session, and on macOS the Keychain also gates access per
application. The encrypted file is what you get when there is no keyring to reach — a
container, a headless Linux box with no D-Bus session, a CI runner — and every command that
falls back to it says so rather than doing it quietly. On such a host, if the credential
warrants better than obfuscation, hand it to the service manager instead: `systemd-creds`
(`LoadCredentialEncrypted=`) binds a secret to the machine's TPM or host key and hands it to
the unit as a file, and the token can also be supplied out of band through
`DEEP_OBSIDIAN_AUTH_TOKEN` (see [Authentication](#authentication)).

`deep-obsidian-mcp secrets check` tells you which store each reference actually resolves in,
without printing any value — see [Rotating a stored secret](#rotating-a-stored-secret).

## Rotating a stored secret

```bash
deep-obsidian-mcp secrets check                                   # where does every reference resolve?
deep-obsidian-mcp secrets set --mount team                        # rotate that mount's credential
deep-obsidian-mcp secrets set --mount team --field e2ee-passphrase
deep-obsidian-mcp secrets set --target auth-token
deep-obsidian-mcp secrets set --target embedding-api-key
```

`secrets check` prints one line per reference the config holds — every mount credential, the
bearer token, both embedding keys — saying whether the store answers for it, and naming the
command that rotates it when it does not. **It never prints a value**, and it exits non-zero
when any reference is `MISSING`, so it works as a gate in a script.

```
[ok     ] mounts.team.passwordRef          osKeyring service=deep-obsidian-mcp account=mount-team-password
[MISSING] mounts.team.e2ee.passphraseRef   osKeyring service=deep-obsidian-mcp account=mount-team-e2ee-passphrase
  rotate with: deep-obsidian-mcp secrets set --mount team --field e2ee-passphrase
[ok     ] mounts.wiki.apiKeyRef            encryptedFile id=mount-wiki-api-key
[ok     ] auth.tokenRef                    encryptedFile id=http-auth-token
4 reference(s) checked, 3 resolved.
```

One caveat the output repeats, because it decides whether a `MISSING` line matters: `check`
reports the **store**, not the effective value. `DEEP_OBSIDIAN_AUTH_TOKEN`,
`DEEP_OBSIDIAN_EMBEDDING_API_KEY` and `DEEP_OBSIDIAN_ALGOLIA_API_KEY` shadow a reference at
runtime and are not consulted, so a `MISSING` reference and a perfectly healthy server are
compatible.

`secrets set` **rotates**: it replaces the value one reference points at, and it **never
modifies the config file**.

- **It writes to the reference the config actually contains.** Read out of the file, custom
  hand-written ids included — not to a freshly derived `mount-<id>-<purpose>`. A rotation that
  guessed would leave the config pointing at a value nobody updated, the mount would keep
  authenticating with the old secret, and nothing would say which of the two entries was live.
- **The reference's own store is preserved: rotation is not migration.** An `osKeyring`
  reference stays in the keyring, an `encryptedFile` reference stays in the file. This is
  *different* from `mounts add`, which prefers the keyring and falls back to the encrypted
  file — and the difference is not an inconsistency. `mounts add` writes the reference it
  chose into the config in the same run, so any choice it makes is self-consistent.
  `secrets set` writes no config, so the same fallback would put the value somewhere the
  config does not point: the old secret would stay live and the operator would believe the
  rotation had happened. So **a store that cannot be written to is reported, with the
  remedy, and nothing is changed.** Moving a secret between stores means changing the
  reference, which is a config edit (or `mounts remove` then `mounts add`, which chooses
  fresh).
- **The new value is never a flag value**, for the reason a mount's password is not: it is
  prompted **masked**, or read from the first line of stdin with `--stdin`.
- **`--field` defaults by backend kind** — a couchdb mount's `password`, an algolia mount's
  `api-key`. `e2ee-passphrase` is never the default: a mount that has one also has a
  password, so a wrong guess would rotate the other secret and surface later as "cannot
  decrypt".
- **Adding a secret the config has no reference for is refused**, and the message names the
  command that does it. Turning on E2EE, or enabling auth for the first time, changes the
  mount or the `auth` section — `secrets set` only ever changes a value. The one reference no
  command rotates is a couchdb mount's `e2ee.obfuscatePassphraseRef`: nothing in this binary
  creates it, so it only exists hand-written. `check` still reports it, and says to store the
  value under the id or account it names.
- `--dry-run` reports which reference *would* be written to and reads no value at all.

After rotating, restart the server: it resolves each reference once, at startup. Rotating
`auth-token` invalidates the token every client holds, and `doctor --probe-remote` is what
confirms a mount can still authenticate with its new credential — `secrets set` writes to the
store and does not contact anything.

## Semantic search (embeddings)

The server has two semantic modes:

- **Sparse fallback** (default) — local term vectors, no external dependency.
- **Embedding-backed** — an OpenAI-compatible `/embeddings` endpoint, with
  similarity ranking executed through `sqlite-vec`.

Enable embeddings through [the wizard](#the-setup-wizard), which offers an Ollama preset
(a local endpoint and model, no account and no key) and stores any API key you do give as
a reference rather than in the file:

```bash
deep-obsidian-mcp setup-service --wizard
```

Without an embedding endpoint, retrieval stays lexical and says so: results report
`recallMode: "lexical"`. An Algolia mount is the exception — its recall is native to the
index and unaffected either way.

Or configure them with flags / environment variables:

```bash
deep-obsidian-mcp serve --vault ~/Vault \
  --embedding-provider openai-compatible \
  --embedding-model nomic-embed-text \
  --embedding-base-url http://localhost:11434/v1
```

Environment variables (useful for the service wrapper and containers):

| Purpose | Variables (first match wins) |
|---|---|
| Provider | `DEEP_OBSIDIAN_EMBEDDING_PROVIDER`, `EMBEDDING_PROVIDER` |
| Model | `DEEP_OBSIDIAN_EMBEDDING_MODEL`, `EMBEDDING_MODEL`, `OPENAI_EMBEDDING_MODEL` |
| Base URL | `DEEP_OBSIDIAN_EMBEDDING_BASE_URL`, `EMBEDDING_BASE_URL`, `OPENAI_BASE_URL` |
| API key | `DEEP_OBSIDIAN_EMBEDDING_API_KEY`, `EMBEDDING_API_KEY`, `OPENAI_API_KEY` |

A blank API key is allowed for local OpenAI-compatible endpoints such as Ollama.

## Authentication

HTTP bearer authentication is **optional and disabled by default**, so loopback
(`127.0.0.1`) setups keep working unchanged. Enable it when you expose the
service beyond the local machine (binding `0.0.0.0` or fronting it with a
tunnel).

Enable it (generates a 256-bit token, stores it securely, prints it once):

```bash
deep-obsidian-mcp setup-service --wizard     # answer yes to authentication
deep-obsidian-mcp setup-service --vault ~/Vault --auth   # non-interactive
```

Disable it again (also deletes the stored token):

```bash
deep-obsidian-mcp setup-service --no-auth
```

Send the token from your client:

```
Authorization: Bearer <token>
```

When enabled, `POST /mcp` and `PUT /upload/{token}` require the token;
`/healthz` and `/readyz` stay open for liveness probes. A missing or invalid
token gets `401` with a `WWW-Authenticate: Bearer` challenge.

Two guards reduce the chance of accidental exposure:

- **Fail-closed bind** — the server refuses to start on a non-loopback host with
  auth disabled. Override deliberately with `--insecure-no-auth` or
  `DEEP_OBSIDIAN_ALLOW_INSECURE=1`.
- **Origin validation** — requests carrying a browser `Origin` header are
  rejected unless that origin is in `auth.allowedOrigins` (DNS-rebinding
  defence). Non-browser clients (Claude Code, curl) omit `Origin` and are
  unaffected.

For containers, tunnels, or headless hosts where the OS keyring is unavailable,
set `DEEP_OBSIDIAN_AUTH_TOKEN` to a literal token; it enables auth and overrides
any configured token reference.

## Automatic reindexing

The local index updates itself in the background: an initial build/check at
startup, debounced rebuilds after vault changes, and a periodic catch-up sync.
It's on by default. Tune or disable it:

```bash
deep-obsidian-mcp serve --vault ~/Vault \
  --auto-reindex true \
  --reindex-debounce-ms 1500 \
  --reindex-interval-ms 30000

deep-obsidian-mcp serve --vault ~/Vault --auto-reindex false
```

The `build_index` tool is still available for an explicit forced rebuild.

## Transport & stdio modes

The server speaks both the HTTP (Streamable) transport and stdio. For stdio
clients you can pin the framing:

```bash
deep-obsidian-mcp --vault ~/Vault --stdio-mode auto      # default
deep-obsidian-mcp --vault ~/Vault --stdio-mode framed
deep-obsidian-mcp --vault ~/Vault --stdio-mode newline
```

For the HTTP service, see [USAGE.md](./USAGE.md#3-run-it-as-a-service).

## Multiple vaults (mounts)

> **EXPERIMENTAL.** Everything in this section is behind `experimental` flags,
> defaults to off, and may change. A config with no `mounts` table behaves exactly
> as it always has.

A single vault needs nothing here: `vaultPath` alone still works and is still the
recommended shape. A `mounts` table lets you graft **other** storage onto folders of
that vault, so an agent sees one namespace while the content behind different
prefixes lives in different places.

A config sets **either** `vaultPath` **or** `mounts`, never both: both spell "where the
vault root is", and silently preferring one would let someone who added `mounts` to an
existing config keep serving the old vault with no signal at all. Migrating means moving
`vaultPath` into the root mount — which is what `mounts add` does for you (see
[Building the table](#building-the-table-the-mounts-cli) below).

```json
{
  "indexDir": "~/.local/share/deep-obsidian-mcp",
  "experimental": {
    "multiVault": true,
    "couchdbVaults": true,
    "algoliaVaults": true
  },
  "mounts": [
    {
      "id": "vault",
      "mountAt": "",
      "backend": { "kind": "filesystem", "vaultPath": "~/Vault" }
    },
    {
      "id": "phone",
      "mountAt": "LiveSync",
      "backend": {
        "kind": "couchdb",
        "url": "https://couch.example",
        "database": "obsidian",
        "username": "obsidian",
        "passwordRef": { "kind": "osKeyring", "service": "deep-obsidian-mcp", "account": "livesync-phone" },
        "writable": false
      }
    },
    {
      "id": "team-wiki",
      "mountAt": "_Wiki",
      "backend": {
        "kind": "algolia",
        "appId": "ABC1234XYZ",
        "indexName": "team-wiki",
        "apiKeyRef": { "kind": "osKeyring", "service": "deep-obsidian-mcp", "account": "algolia-team-wiki" },
        "writable": false
      }
    }
  ]
}
```

### Building the table: the `mounts` CLI

**Use `deep-obsidian-mcp mounts` rather than an editor.** The JSON above is the reference
for what the fields *mean*; it is not the recommended way to produce one. A mount table has
invariants a text editor cannot check — unique ids and prefixes, exactly one root mount, an
`experimental` flag per remote backend, a credential that belongs in the secret store rather
than in the file — and `setup-service` deliberately refuses to rewrite a config that has one
(a mount table is the one thing it cannot reproduce faithfully). The `mounts` family is that
gap closed:

```bash
# What is mounted where, and which experimental flags are on. Works on a legacy
# vaultPath-only config too, where it shows the implicit root as such.
deep-obsidian-mcp mounts list

# Graft a second local vault onto /Team.
deep-obsidian-mcp mounts add filesystem --id team --mount-at Team --vault-path ~/TeamVault

# A LiveSync vault. The password is PROMPTED masked; --password-stdin for scripts.
deep-obsidian-mcp mounts add couchdb --id phone --mount-at LiveSync \
  --url https://couch.example --database obsidian --username obsidian

# A shared Algolia corpus, writable. The API key is prompted masked.
deep-obsidian-mcp mounts add algolia --id team-wiki --mount-at _Wiki \
  --app-id ABC1234XYZ --index-name team-wiki --writable

# Unmount. Nothing is deleted from the remote.
deep-obsidian-mcp mounts remove --id phone
```

Add `--json` to any of them for a structured report, and `--dry-run` to validate without
writing. `deep-obsidian-mcp mounts add --help` lists every kind's flags.

**Leave a flag out and `mounts add` asks for it.** On a terminal, the flags you did give
are treated as answers already supplied and only the gaps are prompted for — the same
questions, in the same order, that [the wizard](#the-setup-wizard) asks, because it is one
implementation:

```bash
# Prompts for --id, --mount-at and --database; --url is already answered.
deep-obsidian-mcp mounts add couchdb --url https://couch.example
```

The id is suggested as a slug of the `mountAt` you type (`Team/Alpha` → `team-alpha`) and
is editable. Optional flags with documented defaults — `--username`, `--writable`,
`--e2ee`, `--participant-id` — are *not* asked: they have working defaults, and filling in
two forgotten flags should not turn into a five-question interrogation. The wizard does ask
them, because there were no flags for it to default from.

With **no terminal** — a script, a pipe, `--yes` (which means "ask me nothing"), or
`--password-stdin` / `--api-key-stdin` (which have already claimed stdin for the
credential) — a missing required flag is an error naming every one of them at once. Never a
prompt that would hang a script forever.

What `mounts add` does, in order:

1. **Migrates a legacy config.** A `vaultPath`-only config is first converted to an explicit
   root mount (`id: "vault"`, `mountAt: ""`, the same path), which resolves to exactly the
   same vault path and index directory the `vaultPath` did — then the new mount is appended.
   The conversion is reported, not silent.
2. **Validates the whole table** through the same loader the server uses at startup, before
   anything is written. So this command cannot produce a config the server would then refuse
   to load, and a duplicate id costs you nothing but the error message.
3. **Asks before enabling an experimental flag.** A couchdb or algolia mount, or a second
   mount of any kind, needs the relevant `experimental.*` flag. The command names the flag,
   says what it turns on, says that it is experimental and may change, and asks. It never
   enables one silently. `--yes` answers every prompt, for scripting.
4. **Stores the credential in the secret store**, OS keyring first and the encrypted secrets
   file as a reported fallback, and puts only the [reference](#secrets-are-references-never-values)
   in the config. **There is no `--password` or `--api-key` flag**: a credential passed as a
   flag value lands in `ps` output and in your shell history, so the only two ways in are a
   masked prompt and `--password-stdin` / `--api-key-stdin` (one line per secret; with
   `--e2ee`, the password first and the E2EE passphrase second).
5. **Probes the mount before writing** — a directory read, one sidecar handshake, or one
   index read; nothing that can mutate anything. On failure it writes **nothing** and removes
   the credential it had just stored, so a typo'd URL leaves no orphan in your keyring.
   `--keep-anyway` writes the mount regardless and tells you `doctor --probe-remote` will
   report it degraded.
6. **Writes with a backup.** A content-changing write leaves the previous file at
   `config.json.bak`, and any key this build does not understand is carried across untouched.

`mounts remove` **unmounts**; it is not a delete. Nothing is ever removed from a couchdb or
algolia backing store (`couchdb export` / `algolia dump` remain the ways to take a copy). The
mount's local index stays unless you pass `--purge-index`, and the stored credential is
**always** kept and named in the output — a reference can be shared by more than one mount,
so deleting one silently could break the other. Two removals are refused: the **root mount**
while other mounts exist (they resolve by longest prefix beneath it), and the **last** mount
(a config needs a root mount, so there would be no valid table to write — edit the file
directly to start over).

`mounts list` and `mounts remove` also work on a config the loader **refuses** — a duplicate
prefix, a missing root, an `experimental` flag someone deleted. `list` prints the table with a
warning and the loader's reason rather than going dark on the file that most needs reading
(index directories are omitted, because they derive from the resolved root), and `remove` is
one of the ways to fix such a table: it validates the table it is about to *write*, not the
one it started from.

Editing the file by hand still works, and `deep-obsidian-mcp doctor` still checks the result.

### The table

| Field | Meaning |
|---|---|
| `id` | Stable, user-chosen slug. Appears in error messages, in `vault_info.mounts[]`, and as `--mount <id>` on the CLI. |
| `mountAt` | The logical vault-relative folder this mount appears at. `""` is the vault root. Stored without leading or trailing slashes. |
| `backend` | One of the three kinds below, discriminated by `kind`. |
| `recallWeight` | Optional. This mount's weight in **federated** recall's fusion stage (see below). Defaults to `1.0`. Must be a finite number greater than `0`. |

Three rules the config validator enforces:

- **`vaultPath` and `mounts` are mutually exclusive** (an empty `mounts` array counts as
  absent). See above.
- **Exactly one mount has `mountAt: ""`.** It may be **any** kind, so a vault can be
  fully remote with no local directory in it at all. What is rejected is a table with *no*
  root mount: routing is longest-prefix and `""` is the only prefix that matches
  everything, so without it a path outside every declared prefix would resolve to nothing
  and a typo in a prefix would become "no such path" instead of landing in the root vault.
  A rootless table is not a vault with a hole in it, it is a vault with no floor.

  A one-mount table needs only that backend's own experimental flag; `multiVault` does not
  apply, because one mount is the legacy shape spelled out longhand.
- **Ids and prefixes are unique.** Routing is longest-prefix, so `_Wiki` and
  `_Wiki/Decisions` can both be mounts and the more specific one wins; two mounts at
  the same prefix are rejected.
- **`recallWeight` is finite and positive.** Zero would remove the mount from every
  federated ranking while `vault_info` still reported it healthy, and a negative weight
  would order that mount's hits worst-first. Both are silent wrong answers, so the config
  is rejected instead of clamped.

### Backend kinds

**`filesystem`** — a vault rooted at a local directory. The only one that stores binary
attachments, and the only one with no experimental gate.

| Field | Meaning |
|---|---|
| `vaultPath` | The directory. Accepts `~`. |
| `indexDir` | Where this mount's search index lives. |

**`couchdb`** — a Self-hosted LiveSync vault in CouchDB, reached through the
supervised Node sidecar. Needs `experimental.couchdbVaults`. See
[docs/homebrew-service.md](./docs/homebrew-service.md) and
`deep-obsidian-mcp couchdb export --help`.

| Field | Meaning |
|---|---|
| `url` | Server origin **without** the database path. Validated to carry no userinfo, because it is printed verbatim by `doctor` and `print-config`. |
| `database` | The LiveSync database name. |
| `username` | A CouchDB user name — an identifier, not a credential, so it is plaintext. |
| `passwordRef` | Secret reference to the password. |
| `e2ee` | `{ "passphraseRef": …, "obfuscatePassphraseRef": … }` when the vault is encrypted or path-obfuscated. |
| `sidecarPath` | Explicit path to the built sidecar bundle. |
| `options` | Chunking / hashing knobs forwarded to the sidecar. **Must match how the vault was written.** |
| `writable` | **Defaults to `false`.** Setting it is what makes the sidecar initialize read-write; nothing else unlocks a write. It gates `delete_note` too — a delete is a write, and a read-only mount advertises no `soft-delete` capability. |
| `indexDir` | Defaults to `<root indexDir>/mounts/<id>`. |

**`algolia`** — a shared, **Markdown-only** corpus stored as records in an Algolia
index, which several participants can mount at once. Needs
`experimental.algoliaVaults`. Full documentation, including the binary exception and
the security model, is in
[docs/algolia-mounts.md](./docs/algolia-mounts.md).

| Field | Meaning |
|---|---|
| `appId` | Algolia application id. Not a credential. |
| `indexName` | The main index. Its `_history` sibling is derived from this name. |
| `apiKeyRef` | Secret reference to the API key. `$DEEP_OBSIDIAN_ALGOLIA_API_KEY` shadows it, with a `warn` when it does. |
| `baseUrl` | Override the REST endpoint. Must carry no userinfo. |
| `writable` | **Defaults to `false`.** |
| `participantId` | Who you are in the corpus's audit trail; lands on every record you write. Defaults to `<user>@unknown`. |
| `cache` | `{ "maxBytes": …, "pinnedPrefixes": [ … ] }` for the local hydrated-note cache. |
| `retention` | `{ "minVersions": 5, "maxAgeDays": 90 }` — how much version history to keep. |
| `indexDir` | Holds the **cache only**; there is no local search index for this mount. |

### `writable` is per mount, on purpose

The experimental flag answers *may this build talk to this storage at all* — a
question about the backend's maturity, with the same answer for every mount.
`writable` answers *may the agent edit **this** vault* — a question about one vault's
role. A read-only archive and a writable scratch vault in one table is a reasonable
thing to want, and a single global flag could not express it.

Both default to off, so every pre-existing config keeps exactly the behaviour it has
today.

### A remote mount at the vault root

The root mount is where the vault "is", so putting a remote backend there changes three
things worth knowing before you do it.

**There is no `vaultPath` anywhere.** Everywhere the vault's location is *reported* —
`doctor`'s `vault:` line, the `vaultPath` field in `healthz`, `readyz` and `vault_info` —
a remote root renders `url/database` (couchdb) or `appId/indexName` (algolia) instead of a
directory. None of that carries a credential. `print-config` writes `"vaultPath": null`,
exactly as it already did for a filesystem-rooted `mounts` table.

Two commands lose something they cannot do without a local directory, and both say so
rather than pretending:

- `setup-service --vault-snippets` reports `skip`. The snippets are files Obsidian reads
  out of `<vault>/.obsidian/snippets`, and a LiveSync vault's `.obsidian` folder lives on
  each syncing *device*. Install them into the local vault of each device instead. The
  `--mcp` and `--skills` installs are unaffected. [The wizard](#the-setup-wizard) does not
  even offer the snippets for a remote root, and says why.
- `doctor`'s `vault` check reports `ok` with the root's location and points at
  `--probe-remote`, which is what actually contacts the remote. It is deliberately not
  `fail` — a fully-remote vault is a supported configuration, and `doctor`'s exit code
  gates on `fail`.

**An unreachable remote root starts the service DEGRADED rather than killing it.** A
missing local directory is a permanent mistake and still aborts startup; a remote that is
down is an outage, and a service that refused to start would be bricked by a network blip.
So `/readyz` answers 503 naming the mount, every path refuses with the backend's own
reason, and a CouchDB mount re-hand-shakes in the background until the remote answers — no
restart needed. A verdict only a config change could fix (rejected credentials, a wrong
E2EE passphrase) is *not* retried, because a running process cannot re-read its config.

**An `algolia` root has no local search index at all**, so the index-derived tools
(`vault_info`, `build_index`, `recommend_folder`) refuse on such a vault while reads,
writes, listings, outlines and `grep_search` all work. A `couchdb` root does have one, so
a fully-remote LiveSync vault keeps every tool. If you want the whole surface over a
shared Algolia corpus, mount it under a filesystem or couchdb root instead of at `""`.

### `indexDir` per mount

Each mount gets its own index directory, defaulting to `<root indexDir>/mounts/<id>`
— keyed by mount id so two mounts cannot collide. The **root** mount uses the
top-level `indexDir`, which is what keeps a single-mount config's index exactly where
it has always been.

When the root mount is **remote**, there is no vault directory to put an index inside, so
its default moves out of the vault and under the same application data directory packaged
mode uses — macOS `Application Support`, otherwise `$XDG_DATA_HOME` — at
`indexes/mounts/<id>`, keyed by the root mount's id. The `mounts/` segment is not
decoration: a filesystem vault's packaged index lands at `indexes/<16 hex characters>`, and
every one of those strings is also a legal mount id, so without the reserved segment a
mount called `abcdef0123456789` could quietly share one `index.sqlite` with an unrelated
vault. Set an explicit `indexDir` if you run two services over two different remotes whose
root mounts share an id.

An Algolia mount is the exception: it has no local search index (the remote index *is*
the corpus), so its directory holds only the hydrated-note cache. The derivation is
the same regardless, so there is one rule to remember rather than one per backend.

### Federated recall: `recallWeight` and `federatedRerank`

| Key | Where | Meaning |
|---|---|---|
| `recallWeight` | per mount | That mount's weight in the fusion stage. Default `1.0`. |
| `federatedRerank` | top level | Whether the final rerank runs. Default `true`. Config-file only — there is no CLI flag or env var, because a per-invocation override of a retrieval-quality setting would make two runs of the same server rank differently with nothing in the config to explain it. |


On a multi-mount vault the ranking tools (`hybrid_search`, `load_knowledge`,
`search_artifacts`) answer an **unscoped** call by searching every mount and fusing the
results into one ranking. Pass `scope` naming a mount root to search that mount alone
instead.

It runs in two stages.

**1. Fusion** picks which candidates are worth looking at, by weighted Reciprocal Rank Fusion
over RANKS rather than scores: a candidate the mount ranked `r`-th (0-based) contributes
`recallWeight / (60 + r)`. Ranks are used because two independently built indexes produce
scores on incomparable scales, and averaging them would invent a comparison that does not
exist.

`recallWeight` is the only cross-mount preference available, and it applies to this stage. It
does **not** change a scoped search — a single backend's own ordering is unaffected by a
constant — it only decides which mount wins when two mounts each offer an equally-ranked hit.
Raise it above `1.0` for a mount you consider more authoritative; leave it unset for the
default.

**2. Rerank** decides the order, by rescoring the fused candidates against the query with a
scorer that knows nothing about mounts: cosine of one query vector against each candidate's
own stored chunk vector, fused with BM25 computed over the candidate set as its own corpus.
Both halves are computed by the server, over content it already holds to render the response;
no mount is ever asked to score another mount's content.

The rerank is not a refinement, it is what makes the answer ranked. Fusion alone cannot order
across mounts: every mount's best hit contributes the identical `weight / (60 + 0)`, and since
logical paths are namespaced no candidate ever appears in two mounts' lists, so nothing is ever
summed and the order collapses into a rank-for-rank interleave broken by mount id. An answer
then sits at the position its own mount happens to hold. Measured on a fixed 12-query eval over
one corpus split two and three ways, MRR against a single-vault baseline of 0.958:

| | recall@20 | recall@50 | MRR | nDCG@20 |
|---|---|---|---|---|
| one vault (baseline) | 1.000 | 1.000 | 0.958 | 0.969 |
| federated, fusion only | 1.000 | 1.000 | 0.600 / 0.469 | 0.703 / 0.601 |
| federated, with rerank | 1.000 | 1.000 | **0.958** | **0.969** |

Recall is identical in every row: fusion always retrieved the right notes, only their order was
wrong. Set `federatedRerank: false` to get the fusion-only ordering — useful to reproduce a
pure-fusion ranking, or to keep a federated query from issuing any query-time embedding — at
the quality shown above.

The response says what happened, so a partial answer is never presented as a complete one:

| Field | Meaning |
|---|---|
| `federated` | `true` on a fused answer. Absent on a scoped or single-mount one. |
| `mounts[]` | One entry per mount: `id`, `mountAt`, `source` (`local-index` or `native-recall`), `recallWeight`, `candidateCount`, `exhausted`, and `recallMode` for a mount that ranked for itself. |
| `mounts[].skipped` | The mount could never have contributed — an Algolia mount holds no binary files and therefore no artifacts. Not a shortfall, and it does not set `degraded`. |
| `mounts[].error` | The mount could have contributed and could not be reached. |
| `degraded` | `true` when any mount errored, or any mount's own retrieval fell back to lexical-only. Always present. |
| `missingBackends` | The mount ids behind a `degraded: true`. Emitted only when non-empty. |
| `candidateBudgetReached` | The search stopped on its work budget. Together with `degraded` it means a better hit may exist on a mount that was not read further. |
| `matches[].mountId` | Which mount produced this hit. |
| `rerank` | `"semantic+lexical"` or `"none"`. Which stage produced the order. |
| `rerankedCandidates` | How many candidates carried a semantic score, when reranked. |
| `matches[].score` | The number that produced THIS order — the rerank score when reranked, the fused score otherwise. |
| `matches[].rrfScore` | The fusion-stage score, kept so the ordering stays explainable. |

`rerank: "none"` with `degraded: false` means this deployment has no embedding backend
configured, so there was nothing to rerank with and nothing was lost. `rerank: "none"` with
`degraded: true` means the query could not be embedded and the answer fell back to fusion order.

Two tools stay per mount rather than federating, and both are deliberate:
`related_notes` and `graph_traverse` answer from the mount owning the `path` they are
given, because a wiki link from a note on one mount to a note on another is not an edge in
either index. `recommend_folder` refuses a multi-mount vault outright: it scores folders by
how much of the query's evidence sits under each one, and those counts are comparable only
within a single index.

### What multi-mount changes for a client

- `vault_info.mounts[]` lists each mount's id, prefix, backend kind, capabilities, and
  conflicted paths.
- The recall tools that rank (`hybrid_search`, `load_knowledge`, `search_artifacts`)
  gain an **optional** `scope` argument: omit it for the federated whole-vault answer,
  pass it to search one mount natively. A single-mount vault's tool surface is unchanged.
- `grep_search` and `find_files` search every mount too. Both produce matches or an
  enumeration rather than a ranking, so merging them needs no fusion. How a mount serves
  line search differs by backend — a filesystem mount spawns `rg`, a CouchDB mount runs an
  exhaustive virtual scan that costs a full corpus read per query, and an Algolia mount is
  candidate-bounded and says so with `exhaustive: false`. See
  [docs/behavior-contract.md](./docs/behavior-contract.md#line-search-per-backend).
- Tools whose whole purpose depends on a capability appear only when some mount has
  it. See
  [docs/mcp-reference.md](./docs/mcp-reference.md#conditionally-advertised-tools).
