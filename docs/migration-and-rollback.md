# Migration And Rollback

How to move a folder of your vault onto a remote backend — and, more importantly, how to
get it back off. Everything here is **experimental**; nothing described below is required
to run a filesystem vault, which remains the default and the only non-experimental
configuration.

The one rule that makes the rest safe: **do not move anything you have not first proven
you can bring back.** Both remote backends ship a verifiable export, and both verifications
are `diff -r`. Run the round trip on a copy before you run it on your notes.

## Before You Start

Multi-mount configs are edited **by hand**. `setup-service` refuses to rewrite a config
that declares a `mounts` table — with or without `--overwrite` — and refuses an auth
change on one, because it cannot write the reference a token would need. Use
`deep-obsidian-mcp print-config` to see exactly what this build reads from your file.

Take a copy of the config before you edit it:

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
2. Add the mount to `config.json` by hand. `experimental.couchdbVaults` is required, and
   `experimental.multiVault` too whenever the table has more than one entry — a LiveSync
   database mounted *as the whole vault* is a one-mount table and needs only the couchdb
   flag. See [CONFIGURATION.md](../CONFIGURATION.md) for every field, and
   [§ A remote mount at the vault root](../CONFIGURATION.md#a-remote-mount-at-the-vault-root)
   before you make it the root: there is then no `vaultPath` anywhere,
   `setup-service --vault-snippets` has nowhere to install, and an unreachable remote
   starts the service degraded rather than failing it.
3. Store the password under the id your `passwordRef` names — in the OS keyring, or in the
   encrypted secrets file. `options` (chunking/hashing) **must match how the vault was
   written**, or content is reassembled wrongly.
4. `deep-obsidian-mcp doctor --probe-remote` and confirm the mount's compatibility status
   is `ok`. Any other value (`locked`, `auth-failed`, `unknown-schema`, …) is the
   sidecar's own diagnosis of the remote, and the mount will not serve data until it reads
   `ok`.
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

# 3. Change the mount kind to filesystem, or delete the mount entirely and let the root
#    vault serve ~/Vault/Live as an ordinary folder.
$EDITOR ~/.config/deep-obsidian-mcp/config.json

# 4. Confirm.
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

1. Add the mount by hand; `experimental.algoliaVaults` is required, and
   `experimental.multiVault` too whenever the table has more than one entry. An algolia
   mount *may* be the root, but think twice: an Algolia corpus has no local search index,
   so a vault rooted on one loses `vault_info`, `build_index` and `recommend_folder` while
   keeping reads, writes, listings and `grep_search`. Mounting it under a filesystem or
   couchdb root keeps the whole surface — see
   [§ A remote mount at the vault root](../CONFIGURATION.md#a-remote-mount-at-the-vault-root).
2. Store the API key under the id `apiKeyRef` names (or set
   `$DEEP_OBSIDIAN_ALGOLIA_API_KEY`, which shadows it with a `warn`).
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

# 3. Remove the mount from config.json and confirm.
$EDITOR ~/.config/deep-obsidian-mcp/config.json
deep-obsidian-mcp print-config && deep-obsidian-mcp doctor
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

A `setup-service` write that **changes** content backs the previous file up first (an
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

There is no `.bak` for a mounts config, because `setup-service` never writes one. Version
control or your own copy is the rollback for a hand-edited file.

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
