# Algolia mounts — a shared, Markdown-only corpus

> **EXPERIMENTAL.** Gated behind `experimental.multiVault` and
> `experimental.algoliaVaults`, read-only unless you set `writable`, and subject to change.
> Do not put anything here that you do not also have a `algolia dump` of.

An Algolia mount grafts a folder of your vault onto an [Algolia](https://www.algolia.com)
index that several people can mount at once. Unlike every other mount kind, **the index is
the vault**: there is no local mirror of the content, which is exactly what lets two
participants author the same corpus without a sync protocol between them.

- [What this is for](#what-this-is-for)
- [The binary exception](#the-binary-exception)
- [Configuration](#configuration)
- [Versioning replaces concurrency control](#versioning-replaces-concurrency-control)
- [Retention](#retention)
- [Reading and searching](#reading-and-searching)
- [The CLI](#the-cli)
- [Security](#security)
- [Known limits](#known-limits)
- [Lineage](#lineage)

## What this is for

One case, and it is worth being narrow about it: a **team wiki** that agents on several
machines read and write, where the interesting operations are search and retrieval rather
than file management. `_Wiki/` shared across a team is the shape this was built for.

It is a bad fit for anything else. A personal vault wants a filesystem mount. A vault synced
between your own devices wants a [CouchDB mount](./homebrew-service.md) or Obsidian Sync.
Attachments want a filesystem mount, always — see below.

## The binary exception

**An Algolia mount stores Markdown only. There is nothing you can configure that changes
this.**

A note is one small `note` record (metadata plus a pointer to the head version) and one
`chunk` record per chunk of that version's text. There is no record shape for binary
content, so every binary operation is refused:

| Operation | Result |
|---|---|
| `read_file` on a non-`.md` path | refused, "MARKDOWN ONLY" |
| raw byte read of any path, `.md` included | refused |
| `request_vault_upload` targeting the mount | refused **at the mint**, before a token is issued |
| `upsert_note` with a non-`.md` path | refused |
| `algolia restore` of a tree containing a non-`.md` file | that file refused and named; `--force` does **not** lift it |

The refusals say so explicitly and deliberately do **not** mention `writable`, because
turning writes on cannot help. Keep attachments on a filesystem mount and link to them from
the shared note; the link resolves for every participant who also has that mount.

The upload refusal happens at the mint rather than at the `PUT` on purpose: a token issued
for an impossible destination would fail only after you had already streamed the body.

## Configuration

The mount goes in the `mounts` table. See
[CONFIGURATION.md § Multiple vaults](../CONFIGURATION.md#multiple-vaults-mounts) for the
table as a whole; this is the Algolia-specific part.

Note there is no top-level `vaultPath`: a config sets that **or** `mounts`, never both, so
the vault root moves into the root mount.

```json
{
  "experimental": { "multiVault": true, "algoliaVaults": true },
  "mounts": [
    { "id": "vault", "mountAt": "", "backend": { "kind": "filesystem", "vaultPath": "~/Vault" } },
    {
      "id": "team-wiki",
      "mountAt": "_Wiki",
      "backend": {
        "kind": "algolia",
        "appId": "ABC1234XYZ",
        "indexName": "team-wiki",
        "apiKeyRef": { "kind": "osKeyring", "service": "deep-obsidian-mcp", "account": "algolia-team-wiki" },
        "writable": false,
        "participantId": "paul@laptop",
        "cache": { "maxBytes": 536870912, "pinnedPrefixes": ["Decisions/"] },
        "retention": { "minVersions": 5, "maxAgeDays": 90 }
      }
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `appId` | Algolia application id. An identifier, not a credential. |
| `indexName` | The main index. Its `_history` sibling is **derived** from this name, never configured, so the two cannot drift apart. |
| `apiKeyRef` | A [secret reference](../CONFIGURATION.md#secrets-are-references-never-values). Never a literal key. |
| `baseUrl` | Override the REST endpoint. For tests, demos and proxies. Must carry no userinfo. |
| `writable` | **Defaults to `false`.** Gates every write, including `delete_note`. |
| `participantId` | Who you are in the corpus's audit trail. Lands on every record you write and is read by others. Defaults to `<user>@unknown`, which says out loud that it was never set. |
| `cache.maxBytes` | Byte budget for the local hydrated-note cache. Default 512 MiB. |
| `cache.pinnedPrefixes` | Prefixes never evicted from that cache. |
| `retention.minVersions` / `maxAgeDays` | See [Retention](#retention). Defaults 5 / 90. |
| `indexDir` | Holds the **cache only** — there is no local search index for this mount. Defaults to `<root indexDir>/mounts/<id>`. |

Three gates compose, and each answers a different question:

- `experimental.algoliaVaults` — *may this build talk to Algolia at all?* A property of the
  backend's maturity, the same answer for every mount.
- `experimental.multiVault` — *may this config have more than one mount?* An Algolia mount
  cannot be the root mount, so it always needs this too.
- the mount's own `writable` — *may the agent edit **this** corpus?* A per-mount question, so
  one writable wiki and one read-only mirror can sit in the same table.

The API key can also come from `$DEEP_OBSIDIAN_ALGOLIA_API_KEY`, which **shadows** the
configured `apiKeyRef`. The override wins (a deployment that sets it means it) but it is
logged at `warn` when it does, because a stray environment variable silently repointing a
writable shared corpus at a different credential is a genuine footgun.

## Versioning replaces concurrency control

The key simplification, and it is what makes the whole design work: **if an overwrite never
destroys anything, no compare-and-swap is needed.**

Every write appends a new version and moves the note's head pointer. The version it replaced
is not deleted — it moves to the history index:

```
_Wiki/Decisions/Foo.md
  v1  paul   supersededBy=v2
  v2  alice  parent=v1  supersededBy=v3
  v3  paul   parent=v1  ⚠ forkedFrom=v2   ← head
```

- **A write always succeeds.** No rejection, no lost race — because losing a race no longer
  loses content. This is where an Algolia mount differs from a CouchDB one, which answers
  `VersionConflict` for a stale base.
- **A stale base FORKS.** The write records `forkedFrom: <the head it overtook>` and sets
  `hasDivergence` on the note. Divergence is *recorded*, not prevented.
- **`vault_info.mounts[].conflictedPaths`** lists diverged notes; `resolve_divergence`
  returns the head, the overtaken version and their common ancestor so **you** can merge
  them. The server never merges: a wrong automatic merge produces plausible text and is
  nearly undetectable.
- **Recovery is a query, not a restore.** `note_history` lists what is still readable and
  `read_version` reads it back.
- **Deleting is soft.** `delete_note` replaces the head with a tombstone: the note leaves
  every listing and every search, other participants can tell it was *removed* rather than
  merely find it missing, and `recoverableFrom` names a version that can still be read.

The precise scope of "nothing is destroyed": it is a guarantee about **overwrites**, not a
promise of permanence. `algolia retract` deliberately destroys a note and its whole history,
and it has to — otherwise a mistaken push into a shared corpus could never be withdrawn.

## Retention

A note's history is bounded by a **union**, never an intersection:

```
keep = (the N most recent versions)  ∪  (everything younger than D days)
```

Defaults N = 5, D = 90. Both halves matter and each covers what the other misses:

- **The floor (N)** protects *stale* notes. A decision note edited five times over three
  years keeps all five; an age-only rule would leave it with no history at all — exactly
  when a wrong edit is hardest to notice.
- **The ceiling (D)** bounds growth on *hot* notes. A note edited fifty times this week
  keeps all fifty while they are fresh, then decays to five.

Purging runs at write time, on the note being written, so it costs nothing extra and never
needs a sweep job. It is best-effort by construction: it runs *after* the head has already
moved, so a failure leaves old versions lingering rather than losing the write.

History lives in a **separate index** (`<indexName>_history`) rather than behind a flag in
the main one, so the searchable index holds current content only: no ranking pollution, and
no filter anyone can forget.

## Reading and searching

- **There is no local index for this mount.** A scoped `hybrid_search` is served by the
  Algolia index itself (the `native-recall` capability). Hits carry ordinal scores and name
  their recall stage rather than claiming parity with the local hybrid ranker, and the
  payload sets `nativeRecall`, `recallMode`, `mountId` and `exhausted` while omitting the
  local ranker's `semanticBackend` / `semanticScore` / `bm25Score`. See
  [mcp-reference.md](./mcp-reference.md#recall-on-a-native-recall-mount).
- **`grep_search` is candidate-bounded, not exhaustive.** It runs a lexical prefilter over
  the index and evaluates your pattern locally over the candidates that came back, so a
  match in a chunk the index ranked below the cap is not reported. The response says
  `exhaustive: false` with a `candidateCount`, and a pattern with no literal anchor is
  refused rather than answered misleadingly.

  This is the one mount kind where that is true. A **CouchDB** mount serves `grep_search`
  with an exhaustive virtual scan — it reads every candidate note and reports no
  `exhaustive` key, like ripgrep — because a document store can hand over the whole corpus
  and a ranked search API cannot. It pays for that in a full corpus read per query. See
  [behavior-contract.md](./behavior-contract.md#line-search-per-backend) for the three-way
  comparison.
- **There are no directories.** Folders are synthesized from hierarchical `folders.lvlN`
  facets, so an empty folder does not exist. Algolia caps facet enumeration at 100 values
  and answers `400` rather than clamping, so a folder with more than 100 direct subfolders
  cannot be listed exhaustively; `list_children` then sets `foldersTruncated` (the files it
  lists are still complete).
- **An index does not exist until its first write.** Every read against a never-written
  corpus answers `404 Index <name> does not exist`, which means "no records" — not a
  failure. `algolia status` reports it as unreachable-and-unprovisioned rather than as an
  error.
- **Writes are asynchronous, but the head push is awaited.** Read-after-write is part of the
  behaviour contract every capture flow depends on, so the head-pointer write waits for the
  index task rather than returning as soon as it is queued.
- **There is no change feed.** Algolia has no "what changed since" primitive, so this mount
  advertises no `watch` capability and nothing waits on one. A colleague's edit is picked up
  by your next read — the head lookup *is* the freshness check.

## The CLI

Every command takes `--mount <id>`, and every one is secret-free in its output.

```bash
# Import a local folder into the corpus, once. Defaults to the folder the mount
# shadows (<vaultPath>/<mountAt>) — which is what a migration wants.
deep-obsidian-mcp algolia seed --mount team-wiki --dry-run
deep-obsidian-mcp algolia seed --mount team-wiki
deep-obsidian-mcp algolia seed --mount team-wiki --from ~/Notes/wiki --move

# The backup / exit strategy. Deterministic: two dumps of an unchanged corpus are
# byte-identical, so `diff -r` verifies a round trip.
deep-obsidian-mcp algolia dump --mount team-wiki --out ~/Backups/team-wiki
deep-obsidian-mcp algolia restore --mount team-wiki --from ~/Backups/team-wiki --dry-run
deep-obsidian-mcp algolia restore --mount team-wiki --from ~/Backups/team-wiki --force

# Reachability, provisioning, note and version counts, divergence, retention.
deep-obsidian-mcp algolia status --mount team-wiki

# The ONE destructive operation. Prompts unless --yes. Never an MCP tool.
deep-obsidian-mcp algolia retract --mount team-wiki --path _Wiki/Decisions/Mistake.md

# A scoped read-only key for a teammate.
deep-obsidian-mcp algolia key --mount team-wiki \
  --parent-key-ref env:SEARCH_ONLY_KEY --prefix _Wiki
```

`--json` on any of them prints the machine-readable report instead of the rendered one, and
the global `--dry-run` applies to `seed`, `restore` and `retract`.

### `seed`

Creates and updates; **never** deletes. A note in the corpus that is not in your folder is
left alone, because on a shared corpus it is most likely a colleague's — removal is
`retract`'s job precisely so that it cannot happen as a side effect of an import.

Two things are skipped and named rather than silently dropped: a non-`.md` file (the corpus
cannot hold it) and a note whose own frontmatter says `share: false` (a per-note opt-out that
travels *with* the note, so it is visible to whoever reads it and does not depend on
remembering a CLI flag).

The first import into a virgin index is reported as such, because that is the one case where
the whole corpus is coming into being from your machine.

`--move` deletes each local original **after** re-reading the index and confirming it holds
exactly those bytes. Per file: anything that drifted in between is kept and named. Empty
parent directories are pruned, but never the folder you pointed at.

### `dump` / `restore`

A dump directory is a plain tree of notes plus a `manifest.json` recording each note's path,
head `versionId`, content hash, size, and whether it carried a divergence or a hash
mismatch. It carries the mount id and **nothing else about the connection** — no app id, no
index name, no key — and no timestamp, because two dumps of an unchanged corpus must be
byte-identical or "dump, mutate, restore, dump again, compare" is not a verification.

A `hashMismatch` row means the body reassembled from chunk records did not match the hash
the note record declares, i.e. a chunk is missing or duplicated. A dump is the one moment
every note is read end to end, which makes it the right place to notice; the command exits
non-zero when any row has it, so a script does not mistake a corrupt snapshot for a backup.

`restore` writes such a tree back through the same guarded, fork-aware path the MCP tools
use:

| Situation | Default | With `--force` |
|---|---|---|
| no live note at the path | **created** | created |
| index holds identical bytes | **skipped** (so a re-run is idempotent) | skipped |
| index holds different bytes | **refused**, and named | **superseded** — the current version moves to history and stays readable |
| non-`.md` path | **refused** | **still refused** |

`--dry-run` performs every read and comparison and no write, and works on a read-only mount,
so you can find out what a restore would do before turning writes on.

### `retract`

The exception to non-destruction: the note record, its chunks, and every history record for
it are deleted. It prompts with the note's head version and the number of versions about to
go unless `--yes`.

It is a CLI command and **deliberately not an MCP tool**. An agent cannot judge whether a
human wanted a shared corpus's history destroyed, and no amount of tool-description wording
fixes that.

### `key`

Derives an Algolia
[secured API key](https://www.algolia.com/doc/guides/security/api-keys/#secured-api-keys)
scoped to one folder, for a teammate who should be able to read the corpus and nothing else.
The derived key is printed (sharing it is the point); the parent never is.

`--parent-key-ref` takes one of:

| Form | Where the parent comes from |
|---|---|
| `mount` (default) | the mount's own `apiKeyRef`, or `$DEEP_OBSIDIAN_ALGOLIA_API_KEY` if set |
| `keyring:<service>/<account>` | the OS keyring |
| `file:<id>` | the encrypted secrets file |
| `env:<VAR>` | an environment variable |

`--prefix` maps onto the folder facet its depth implies (`_Wiki` → `folders.lvl0`,
`_Wiki/Decisions` → `folders.lvl1`). Only three levels of facet exist, so a folder four
levels down is **refused** rather than silently scoped to its grandparent, which would hand
out more access than you asked for.

There is no `algolia set-key`. Storing a mount's key is not an Algolia-specific operation —
it is an ordinary secret reference. Use `setup-service --wizard`, place the value in the
encrypted secrets file, or set `$DEEP_OBSIDIAN_ALGOLIA_API_KEY`. A generic
`secrets set --ref <ref>` command would be the right home for it and does not exist yet.

## Security

**Read scoping is native, and it is enforced by Algolia rather than by this server.** A
secured key embeds a `filters` restriction validated server-side, so a scope your own client
enforced — which a modified client could lift — is not what you get.

**A secured key inherits its parent's ACLs.** Verified the hard way against a live account in
PR #40: a key derived from the mount's own write key read only `_Wiki/` and **successfully
wrote a record into `_Agent/`**. The `filters` restriction constrains **search only**; it
does not constrain writes at all.

So `algolia key` inspects the parent's ACLs and **refuses** any parent holding `addObject`,
`deleteObject`, `deleteIndex` or `editSettings`, pointing you at a search-only parent
instead. It also warns when the parent lacks `browse`, which is a distinct ACL from `search`:
without it the teammate gets a bare `403` from the reads that enumerate exhaustively
(listing the mount root, `note_history`, `algolia dump`) while every scoped read still works.

What Algolia does enforce correctly, also verified live: `search` and `browse` honour the
filter, and `getObjects` on an out-of-scope object is refused outright with
`403 objectID not allowed`. That 403 is mapped to the **same not-found** a genuinely missing
note produces, so a scoped participant cannot tell "exists but hidden" from "does not exist"
and use the difference to enumerate paths outside their scope.

**Write scoping is not native.** Algolia write keys restrict by *index*, not by record:

> A writable Algolia mount is **write-trusted**. Any participant with the write key can
> append a version to any note in the index.

Versioning makes this survivable rather than safe — nothing is destroyed, and
`participantId` attributes every version — but it is **not access control**. The failure mode
of a careless write drops from "content lost" to "content superseded and recoverable, with an
audit trail", which is a meaningful mitigation and the reason this design is defensible at
all. Per-participant write ACLs need a proxy this project does not have.

**Read-only participants get secured search keys and no write key.** That is the right
default for anyone who is not an intended contributor.

## Known limits

1. **Markdown only.** [The binary exception](#the-binary-exception). Nothing configures it
   away.
2. **Semantic recall is lexically bounded** without Algolia NeuralSearch. The single biggest
   retrieval weakness; reported through `recallMode` rather than implied.
3. **`grep_search` is candidate-bounded** and refuses anchor-less patterns.
4. **A cache miss costs a round trip.** The first read of a cold note is network-bound, and
   `load_knowledge` over many cold notes is the worst case. Mitigated by
   `cache.pinnedPrefixes`, not eliminated.
5. **Structure is facet-derived.** Empty folders do not exist, and a folder with more than
   100 direct subfolders cannot be listed exhaustively (`foldersTruncated` says so).
6. **Forks accumulate if nobody reconciles.** `hasDivergence` makes them visible and
   `resolve_divergence` makes them fixable; nothing forces resolution.
7. **No offline writes.** A write needs the network. Local mounts are unaffected.
8. **History is bounded, not constant.** Peak size is "writes in the last 90 days". A
   per-note version clamp is the escape hatch if that ever bites.
9. **A writable mount is write-trusted.** See [Security](#security).
10. **No change feed**, so no `watch` capability and no auto-reindex for this mount.

## Lineage

The design, the record shapes, the versioning model, the retention rule and every live
finding in [Security](#security) come from **PR #40 ("Algolia shared wiki")**, whose
`docs/algolia-shared-wiki.md` is the original design document. What changed in the port is
the architecture around it, not the model:

- a `shared[]` config array became an entry in the general `mounts` table, so an Algolia
  corpus is configured like every other vault and routed by the same longest-prefix router;
- the runtime became a `VaultBackend` behind the same boundary the filesystem and CouchDB
  backends sit behind, so the MCP surface reaches it through `execute` and nothing else;
- the `share` CLI family became `algolia`, addressed by `--mount <id>` instead of
  `--index <name>`;
- `share set-key` is gone (see [`key`](#key)), and `seed`'s refusal to import the mount's own
  prefix is gone too — under a mount table, that prefix is the migration path rather than a
  mistake.
