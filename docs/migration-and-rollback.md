# Migration And Rollback

How to move a folder of your vault onto a remote backend — and, more importantly, how to
get it back off. Everything here is **experimental**; nothing described below is required
to run a filesystem vault, which remains the default and the only non-experimental
configuration.

The one rule that makes the rest safe: **do not move anything you have not first proven
you can bring back.** Both remote backends ship a verifiable export, and both verifications
are `diff -r`. Run the round trip on a copy before you run it on your notes.

## Before You Start

Build the mount table with **`deep-obsidian-mcp mounts`**, not an editor. It validates the
whole table through the same loader the server uses, converts a legacy `vaultPath` config to
an explicit root mount for you, prompts for the credential masked and stores only the
reference, probes the mount before writing anything, and leaves the previous file at
`config.json.bak`. See
[§ Building the table](../CONFIGURATION.md#building-the-table-the-mounts-cli).

Hand editing still works, and is still what you fall back to for the fields `mounts add`
does not expose (`options`, `cache`, `retention`, `recallWeight`). What does **not** work is
`setup-service`: it refuses to rewrite a config that declares a `mounts` table — with or
without `--overwrite` — and refuses an auth change on one, because it cannot write the
reference a token would need. Use `deep-obsidian-mcp print-config` to see exactly what this
build reads from your file, and `deep-obsidian-mcp mounts list` for what is mounted where.

Take a copy of the config before you change it:

```bash
cp ~/.config/deep-obsidian-mcp/config.json ~/.config/deep-obsidian-mcp/config.json.pre-mounts
```

Then check the local prerequisites — this contacts nothing and needs no credentials:

```bash
deep-obsidian-mcp doctor
```

For a `couchdb` mount it reports whether the LiveSync sidecar bundle was located and
whether a Node ≥ 20 is present. Add `--probe-remote` to also contact each remote-backed
mount read-only (one handshake, or one `getSettings`) once the credentials are in place.

## Moving A Folder To A CouchDB (LiveSync) Mount

A `couchdb` mount is a **view of a vault someone else already writes** — a Self-hosted
LiveSync database, normally driven by the Obsidian plugin on your other devices. So this
is not really a migration: you are mounting an existing vault, not converting a folder
into one. There is deliberately no "push my folder into CouchDB" command.

1. Set up Self-hosted LiveSync in Obsidian and let it populate the database.
2. Add the mount. `experimental.couchdbVaults` is required, and `experimental.multiVault`
   too whenever the table has more than one entry — a LiveSync database mounted *as the
   whole vault* is a one-mount table and needs only the couchdb flag. `mounts add` names
   each flag it needs and asks:

   ```bash
   deep-obsidian-mcp mounts add couchdb --id live --mount-at Live \
     --url https://couch.example --database obsidian --username obsidian
   ```

   The password is prompted **masked** and stored in the OS keyring (or the encrypted
   secrets file, reported); only the `passwordRef` lands in the config. The command then
   performs the sidecar handshake and **refuses to write** unless the compatibility verdict
   is `ok` — so step 4 below has already happened by the time it succeeds. Add `--e2ee` for
   an encrypted vault (it prompts for the passphrase too) and `--writable` only after
   step 5.

   See [CONFIGURATION.md](../CONFIGURATION.md) for every field, and
   [§ A remote mount at the vault root](../CONFIGURATION.md#a-remote-mount-at-the-vault-root)
   before you make it the root (`--mount-at ""`): there is then no `vaultPath` anywhere,
   `setup-service --vault-snippets` has nowhere to install, and an unreachable remote
   starts the service degraded rather than failing it.
3. `options` (chunking/hashing) is **not** a `mounts add` flag: it must match how the vault
   was written, or content is reassembled wrongly, and a value guessed from a command line
   would fail silently. Add it to the mount by hand if your vault needs it.
4. `deep-obsidian-mcp doctor --probe-remote` and confirm the mount's compatibility status
   is `ok`. Any other value (`locked`, `auth-failed`, `unknown-schema`, …) is the
   sidecar's own diagnosis of the remote, and the mount will not serve data until it reads
   `ok`. (`mounts add` already refused anything else, unless you passed `--keep-anyway`.)
5. **Take a snapshot before granting writes**, and verify the round trip:

```bash
deep-obsidian-mcp couchdb export --mount live --out ~/backups/live-$(date +%F)
```

Two exports of an unchanged vault are byte-identical, manifest included. That is what
makes the snapshot a verification rather than a hope:

```bash
deep-obsidian-mcp couchdb export --mount live --out /tmp/live-again
diff -r ~/backups/live-$(date +%F) /tmp/live-again && echo "round trip verified"
```

`writable` defaults to `false`. Leave it that way until the export above is verified.

## Getting Out Of A CouchDB Mount

The export directory **is** the way out: it is a plain tree of files.

```bash
# 1. Snapshot, and check what you got.
deep-obsidian-mcp couchdb export --mount live --out ~/live-final
ls -R ~/live-final          # ordinary files; manifest.json records revision + hash per entry

# 2. Move the tree into your filesystem vault.
mkdir -p ~/Vault/Live
rsync -a --exclude manifest.json ~/live-final/ ~/Vault/Live/

# 3. Unmount, and let the root vault serve ~/Vault/Live as an ordinary folder. Nothing is
#    deleted from CouchDB; the previous config is kept at config.json.bak, and the stored
#    password is kept and named so you can clean your keyring yourself.
deep-obsidian-mcp mounts remove --id live

# 4. Confirm.
deep-obsidian-mcp mounts list
deep-obsidian-mcp print-config
deep-obsidian-mcp doctor
```

To go back the other way — push a snapshot into the database — `couchdb restore` writes a
tree through the same revision-guarded path the MCP tools use. It creates missing entries,
skips identical ones, and **refuses** entries whose remote content differs unless
`--force`, so the default cannot discard an edit made after the export. `--dry-run` reports
exactly what a real run would do and works on a read-only mount:

```bash
deep-obsidian-mcp couchdb restore --mount live --from ~/live-final --dry-run
```

Note what restore does **not** do: it never deletes. A note someone added after your
export survives a restore. "Make the vault exactly match this tree" would mean deleting a
colleague's work, and no flag does it.

## Moving A Folder To An Algolia Mount

An `algolia` mount is a **shared, Markdown-only corpus** stored as records in an Algolia
index that several participants mount at once. Note bodies go to a hosted third-party
service. Read [algolia-mounts.md](./algolia-mounts.md) — especially the security section —
before you put anything real in one.

1. Add the mount; `experimental.algoliaVaults` is required, and `experimental.multiVault`
   too whenever the table has more than one entry. `mounts add` names each flag and asks:

   ```bash
   deep-obsidian-mcp mounts add algolia --id wiki --mount-at Wiki \
     --app-id ABC1234XYZ --index-name team-wiki
   ```

   The API key is prompted **masked** and stored in the OS keyring (or the encrypted
   secrets file, reported); only the `apiKeyRef` lands in the config. The command then
   reads the index once and **refuses to write** if it cannot be reached, so steps 2 and 3
   below are already done when it succeeds. Add `--writable` only after you have verified
   the dump round trip.

   An algolia mount *may* be the root (`--mount-at ""`), but think twice: an Algolia corpus
   has no local search index, so a vault rooted on one loses `vault_info`, `build_index`
   and `recommend_folder` while keeping reads, writes, listings and `grep_search`. Mounting
   it under a filesystem or couchdb root keeps the whole surface — see
   [§ A remote mount at the vault root](../CONFIGURATION.md#a-remote-mount-at-the-vault-root).
2. `$DEEP_OBSIDIAN_ALGOLIA_API_KEY` still shadows the stored key, with a `warn`, if you
   would rather supply it per-process than store it.
3. Check it: `deep-obsidian-mcp algolia status --mount wiki`, or
   `deep-obsidian-mcp doctor --probe-remote`.
4. Import the folder the mount shadows — **once**:

```bash
# See what would happen first. Always.
deep-obsidian-mcp algolia seed --mount wiki --dry-run

# Import (creates and updates only; never deletes from the index to match the source).
deep-obsidian-mcp algolia seed --mount wiki
```

`--move` deletes each local original, but only after re-reading the index and confirming it
holds those exact bytes. Do not use it until you have verified a dump round trip below.
Non-`.md` files are refused: the corpus stores Markdown only, and no flag lifts that.

## Getting Out Of An Algolia Mount

```bash
# 1. Dump. Two dumps of an unchanged corpus are byte-identical, so this verifies itself.
deep-obsidian-mcp algolia dump --mount wiki --out ~/wiki-final
deep-obsidian-mcp algolia dump --mount wiki --out /tmp/wiki-again
diff -r ~/wiki-final /tmp/wiki-again && echo "round trip verified"

# 2. Move the notes into the vault folder the mount was shadowing.
mkdir -p ~/Vault/Wiki
rsync -a --exclude manifest.json ~/wiki-final/ ~/Vault/Wiki/

# 3. Unmount, and confirm. Nothing is deleted from the index (see below); the previous
#    config is kept at config.json.bak, and the stored API key is kept and named.
deep-obsidian-mcp mounts remove --id wiki
deep-obsidian-mcp mounts list && deep-obsidian-mcp doctor
```

`algolia restore` writes a dump back, with the same refuse-on-divergence rule as couchdb
restore — and even with `--force` nothing is destroyed: the current version moves to
history.

Leaving the index behind is not the same as deleting it. The records stay in Algolia, and
every other participant still sees them. `algolia retract --mount wiki --path <note>`
permanently deletes one note **and its entire history**; it is the one destructive
operation here, it prompts unless `--yes`, and it is deliberately not an MCP tool. To
dispose of the whole corpus, delete the index in the Algolia dashboard.

## Config Rollback

Every write this binary makes to your config — `setup-service`, `mounts add`,
`mounts remove` — backs the previous file up first **when the content changes** (an
unchanged rewrite does not, so no spurious `.bak` appears):

```
~/.config/deep-obsidian-mcp/config.json.bak
```

Rolling back is a copy:

```bash
cp ~/.config/deep-obsidian-mcp/config.json.bak ~/.config/deep-obsidian-mcp/config.json
deep-obsidian-mcp print-config     # confirm before restarting
```

Only **one** generation is kept, and the next content-changing write replaces it. For a
change you want to be able to undo later, take your own copy — as at the top of this page.

A config you edited **by hand** has no `.bak`, because nothing wrote it: version control
or your own copy is the rollback there. `setup-service` still refuses to rewrite a mounts
config at all, so its `.bak` never appears for one; the `mounts` family's does.

### Downgrading The Binary

A config written by a newer build and then rewritten by an older one keeps its unknown
top-level keys and its unknown per-mount keys: both are retained verbatim across a
load→save round trip, so an older `setup-service` cannot silently delete a setting it does
not understand.

**One gap, worth knowing before you downgrade.** Unknown keys *inside* a mount's `backend`
object are still dropped — `MountBackendConfig` is internally tagged by `kind`, and serde
supports neither field retention nor a hard rejection on such a variant. So if a newer
build adds a `couchdb` or `algolia` option and an older build rewrites that file, the
option is lost. Two consequences:

- Keep your own copy of the config before downgrading.
- An older build reading a config with an unknown **mount kind** fails cleanly with
  "unknown variant" rather than mis-parsing it, so a whole new backend cannot be silently
  discarded — only an option within an existing one.
