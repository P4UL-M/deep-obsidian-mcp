# Algolia shared wiki — design proposal

**Status:** implemented on this branch (stages 0–6 of §13). Run the
end-to-end demo with `scripts/demo-shared-wiki.sh` — it uses an in-process
mock of the Algolia REST API, so no account or key is needed; point `baseUrl`
at nothing (default) with a real `appId`/key to run against actual Algolia.

**Model: mount-only authorship (the standing export was removed).** An earlier
implementation carried a persistent `export` rule plus a recurring `share
push`. It created an asymmetry between writer classes — mount writes were
fork-aware, while an exporter's push silently superseded a colleague's head
(their version went to history unflagged) — and it duplicated every exported
note across two addressable paths. Rather than adding sync state and pull
machinery to patch that, the export path was **deleted**: the shared wiki LIVES
in the index and every write goes through the mount (versioned, fork-aware,
symmetric for all participants).

`SharedMountConfig` therefore has no `export` field, and there is no `share
push`. The command surface is:

| Command | Role |
|---|---|
| `share seed --prefix <folder/> [--move]` | One-shot import of existing local notes. Only creates/updates — **never** removes anything from the index. `--move` deletes the local copies after a per-file check that the index holds identical content, so exactly one copy exists. |
| `share dump --to <dir>` | Materializes every head version into a directory with a manifest — backup, exit strategy, human-browsable snapshot for a corpus whose only live copy is the index. |
| `share status` | Note count, superseded-version count, cache stats, and any diverged notes per mount. |
| `delete_note` (MCP tool) | Ordinary removal: soft-deletes on the mount. The head becomes a `deleted: true` tombstone, its chunks leave the main index (so listings and search stop finding it), and the previous version moves to history — recoverable with `read_version`, undeleted by writing the note again. Refuses local paths: MCP has never exposed local file deletion. |
| `share retract --path <note>` | The permanent purge: removes a note, its chunks, the tombstone and the whole history. |
| `share set-key`, `share key` | Store the mount's API key (verified round trip); mint a scoped read-only key for a teammate. |

Consequence worth stating: because seed never reconciles deletions, deleting a
note locally does **not** remove it from the shared index — retraction is
explicit. That is the deliberate trade for never destroying a colleague's
contribution by accident, and it is covered by a regression test.

A large shared corpus lives in Algolia. It is **not** mirrored locally — the
premise is that it is bigger than any participant wants on disk. Locally there is
only a **bounded cache** of the notes actually touched. Writes are **append-only
versions**, so a note is never destroyed by an overwrite: the previous version is
marked superseded and stays retrievable.

Three earlier drafts failed, and each failure is load-bearing here:

1. **Algolia as a full vault backend (global switch)** — broke Obsidian, killed
   `grep_search` and artifacts, gated semantic on the Elevate plan.
2. **Publish + read-only subscription** — an agent grepping the wiki found
   nothing, and nobody could write to it.
3. **Full local working copy of the shared subset** — assumed the shared corpus
   fits on disk. It does not.

## Decisions taken

| Decision | Choice |
|---|---|
| Concurrency | **Append-only versions, no CAS.** Overwrites supersede, never destroy ([§3](#3-versioning-replaces-concurrency-control)) |
| History storage | **Separate index**, so current-content search is never polluted ([§3](#3-versioning-replaces-concurrency-control)) |
| History retention | **Floor + ceiling: always keep the 5 most recent versions; beyond those, keep anything younger than 90 days** ([§3.1](#31-retention-floor--ceiling)) |
| Semantic recall | **Adaptive** — NeuralSearch first-stage when the index has it, wide candidate window + facet prefilter otherwise; active mode reported ([§4.2](#42-semantic-becomes-reranking-not-retrieval)) |
| Local footprint | **Bounded LRU cache + pinned prefixes** ([§6](#6-configuration)) |
| Divergence | **`resolve_divergence` exposes the fork; the agent merges.** No server-side auto-merge ([§7](#7-tool-behaviour-on-shared-mounts)) |
| `grep_search` | **Candidate-bounded, explicitly non-exhaustive**, refuses unanchored patterns ([§4.3](#43-grep_search-becomes-candidate-bounded--reversing-an-earlier-position)) |

## 1. Constraints

| Constraint | Consequence |
|---|---|
| Shared corpus exceeds what is practical locally | No full materialization. Local presence is a bounded cache, hydrated on demand |
| Algolia has no compare-and-swap | Do not fight it — **version instead of guard** (§3) |
| Algolia record cap: 10 KB Build, 10–100 KB above | Chunk-per-record, not note-per-record (§5) |
| Regex is not an Algolia primitive | `grep_search` becomes candidate-bounded, not exhaustive (§4.3) |
| Local embeddings cannot cover a corpus you never download | Semantic becomes **reranking**, not first-stage retrieval (§4.2) |

## 2. Division of labour

| | Algolia | Local |
|---|---|---|
| Recall over the whole corpus | ✅ | ✗ (never sees it all) |
| Facet navigation, folders-as-facets | ✅ | ✗ |
| Sharing, zero-setup access | ✅ | ✗ |
| Version history storage | ✅ (append-only records) | ✗ |
| Semantic *reranking* of candidates | own tier only (NeuralSearch) | ✅ |
| Regex over candidates | ✗ | ✅ |
| Exact file reconstruction, line ranges, outlines | ✗ | ✅ (§4.1) |
| Obsidian visibility, offline | ✗ | cache only |

The shape that follows: **Algolia does first-stage retrieval over everything;
local does precision work on a small candidate set.** That is a standard
retrieve-then-rerank architecture, and it is the only shape that survives the
volume constraint.

## 3. Versioning replaces concurrency control

Your instinct is right and it is the key simplification: if an overwrite never
destroys anything, **no compare-and-swap is needed at all.**

Every write appends a new version record. The note's previous version is not
deleted — it is marked superseded:

```
note _Wiki/Decisions/Foo.md
  v1  participant=paul   supersededBy=v2      isHead=false
  v2  participant=alice  parent=v1            isHead=false  supersededBy=v3
  v3  participant=paul   parent=v1  ⚠fork     isHead=true
```

- **Push always succeeds.** No guard, no rejection, no lost race — because losing
  a race no longer loses content.
- **`parentVersionId`** records what the writer based their edit on. When it is not
  the current head, the new version is flagged `forkedFrom` — divergence is
  *recorded*, not prevented.
- **A note is `hasDivergence: true`** when a fork exists. An agent or human can
  reconcile later; nothing blocks in the meantime.
- **No overwrite destroys content.** Recovery from a bad edit is a query, not a
  restore. Note the precise scope: this is a guarantee about *overwrites*, not a
  promise of permanence — deliberate retraction does remove history (§8), and it
  has to, or a mistaken push could never be withdrawn.

This is the git-like behaviour you described, minus merges — and it fits Algolia's
grain exactly, because appending records is what Algolia is good at.

### 3.1 Retention: floor + ceiling

A version is purged when it is **both** outside the 5 most recent **and** older
than 90 days. Equivalently, the kept set is:

```
keep = (5 most recent versions)  ∪  (versions younger than 90 days)
```

Both halves matter, and each covers what the other misses:

- **The floor (5)** guarantees recovery depth on *stale* notes. A decision note
  edited five times over three years keeps all five — an age-only rule would leave
  it with no history at all, exactly when a wrong edit is hardest to notice.
- **The ceiling (90 days)** bounds growth on *hot* notes. A note edited fifty
  times in a week keeps all fifty while they are fresh, then decays to five.

Purge runs at write time on the note being written, so it costs nothing extra and
never needs a sweep job.

**Bounded, not constant.** Peak history size is "writes in the last 90 days", so a
very active corpus still grows within the window before decaying. If that becomes
a cost problem the natural third cap is a per-note maximum (e.g. 50), which does
not change the model — it just adds a clamp. Not needed for v1.

**Search must not see history.** History lives in a **separate index**
(`<name>_history`), not behind an `isHead` filter in the main one. The searchable
index then contains only current content: no wasted records, no ranking
pollution, no filter to forget. This directly answers "ça pollue un peu notre
recherche" — the pollution is avoided structurally rather than filtered away.

## 4. Reading and searching without a local corpus

### 4.1 Hydration is exact

`read_file` on a note not in cache fetches its head chunks and reassembles them.
This is **lossless**, verified by an existing invariant: the primary chunker
`section_chunks` tiles a note into **non-overlapping** segments, and
`section_chunks_round_trip_line_ranges_to_source` asserts

```rust
chunk.text == lines[chunk.start_line - 1..chunk.end_line].join("\n")
```

— each chunk's stored text is the exact source slice. Sorting head chunks by
`start_line` and concatenating reproduces the file byte-for-byte, so the full body
never needs to be stored twice.

One caveat: the fallback chunker `chunk_lines` (used for heading-less notes)
**does** overlap by 12 lines. Reassembly must de-duplicate by line range rather
than concatenating blindly — deterministic, since every chunk carries
`start_line`/`end_line`, but it must be handled or those notes gain duplicated
lines.

Hydrated notes land in the local cache, so line ranges, `note_outline`, and
repeated reads are served locally afterwards.

### 4.2 Semantic becomes reranking, not retrieval

Local embeddings cannot cover a corpus that is never downloaded. So the pipeline
inverts:

```
query ─► Algolia: top ~100 candidates over the WHOLE corpus   (recall)
       ─► hydrate candidates into cache
       ─► local embeddings rerank the 100                     (precision)
       ─► rrf_fuse(algolia_rank, semantic_rank) ─► results
```

Only candidates get embedded, and their vectors are cached — so cost is bounded by
query traffic, not corpus size. This preserves the semantic *quality* the local
implementation has today while never holding the corpus.

*Sizing caveat:* with `attributeForDistinct: "path"` and `distinct: 1`, a
`hitsPerPage` of 100 returns 100 **notes**, not 100 chunks — and distinct
interacts with pagination. Verify the actual hit/chunk arithmetic before fixing
the candidate window, rather than assuming 100 hits means 100 passages to rerank.

`rrf_fuse` already does the fusion and is rank-based; as its own comment states,
it "drops the dependence on incomparable cosine vs BM25 scales". Algolia's score
is likewise incomparable, which is exactly why it fuses without normalization.

**The honest limit: recall is bounded by whatever the first stage retrieves.** If a
semantically relevant note shares no lexical signal with the query, a lexical
first stage never makes it a candidate, so the reranker never sees it.

**Decision: adaptive, with the active mode reported.** The first stage is chosen
from what the index actually supports, detected at startup and re-checked on
settings reload:

| Index has | First stage | Recall |
|---|---|---|
| NeuralSearch enabled | Algolia semantic + keyword | semantic — the limit above goes away |
| keyword only | keyword + typo + prefix, **window 200–500**, facet prefilter (`type`, `project`, `layer`) | lexically bounded |

Local embedding rerank runs in both cases, so precision is unchanged either way;
only recall differs. `vault_info` reports the active mode, and `hybrid_search` /
`load_knowledge` carry a `recallStage` field — an agent that gets lexical recall
should know to issue narrower follow-up queries, which is exactly the behaviour
the existing wiki decision on architecture-agnostic retrieval already asks of
agents.

Adaptive rather than requiring NeuralSearch because the tool ships MIT via
Homebrew and apt: requiring the Elevate tier would make the whole feature
unusable for most installs. This is the one place the design is weaker than a
local vault, and it is reported rather than implied.

### 4.3 `grep_search` becomes candidate-bounded — reversing an earlier position

Earlier drafts rejected "Algolia prefilter → local regex" as silently
under-reporting. **That rejection assumed an exhaustive local alternative
existed.** With a corpus that is never fully local, it does not: the choice is
candidate-bounded regex or no regex at all. So the mechanism is right after all,
provided it is honest:

- Algolia narrows lexically over the whole corpus, candidates are hydrated, the
  regex runs locally over them.
- The result **must** carry `exhaustive: false`, the prefilter query used, and the
  candidate count — so an agent knows the answer is candidate-bounded.
- Patterns with no lexical anchor (`^\s*$`, pure-structural regexes) cannot be
  prefiltered. Those must be **refused with a clear reason**, not answered with a
  misleadingly small result set.

The precedent — `grep_search` is "ripgrep works or the tool is disabled", never a
silent fallback — is preserved in spirit: the honesty requirement stands, only the
capability boundary moved.

## 5. Records

Chunk-per-record, driven by the size cap: Algolia allows **10 KB on Build**,
10–100 KB above
([service limits](https://www.algolia.com/doc/guides/scaling/algolia-service-limits)),
and many notes here already exceed 10 KB. Chunks also give NeuralSearch a focused
passage rather than a note spanning many topics, and the existing chunker already
fits the budget.

**Main index — heads only.** One record per chunk of the current version, plus a
small `recordType: "note"` record per note:

```json
{
  "objectID": "wiki:_Wiki/Decisions/Foo.md@v3#2",
  "recordType": "chunk",
  "noteId": "wiki:_Wiki/Decisions/Foo.md",
  "versionId": "v3",
  "path": "_Wiki/Decisions/Foo.md",
  "dir": "_Wiki/Decisions",
  "folders": { "lvl0": "_Wiki", "lvl1": "_Wiki/Decisions" },
  "title": "Deep Obsidian keeps retrieval architecture-agnostic",
  "type": "wiki-decision", "project": "Deep Obsidian", "status": "active",
  "headings": ["Decision", "Rationale"],
  "chunkIndex": 2, "startLine": 21, "endLine": 34,
  "text": "...",
  "links": ["_Agent/Contracts/Deep Obsidian.md"],
  "linksRaw": ["_Agent/Contracts/Deep Obsidian|Agent Contract"],
  "updatedAtMs": 1753600000000,
  "participantId": "paul.mairesse@algolia.com"
}
```

The note record additionally carries `versionId`, `parentVersionId`,
`hasDivergence`, `contentHash`, and `chunkCount` — enough to hydrate and to detect
forks.

**History index** — same shape, plus `supersededBy` and `forkedFrom`; never
queried by normal search.

```js
// main index
searchableAttributes:  ["unordered(title)", "headings", "unordered(text)", "path"]
attributesForFaceting: [
  "searchable(folders.lvl0)", "searchable(folders.lvl1)", "searchable(folders.lvl2)",
  "filterOnly(links)", "filterOnly(path)", "filterOnly(noteId)", "filterOnly(versionId)",
  "filterOnly(dir)", "filterOnly(recordType)",
  "type", "project", "status", "layer", "tags"
]
attributeForDistinct: "path"
distinct: 1
customRanking: ["desc(updatedAtMs)"]
attributesToSnippet: ["text:40"]
```

**Version cutover ordering — and the filter that must not be negative.** A write:

1. reads the current head pointer, call it `vPrev`;
2. pushes the new version's chunks to the main index;
3. copies `vPrev`'s chunks into the history index;
4. deletes them from main by **`filters: 'noteId:X AND versionId:vPrev'`**;
5. updates the note record's head pointer.

Two details are load-bearing:

- **The delete filter must name `vPrev` explicitly, never `NOT versionId:vNew`.**
  With the negative form, two concurrent writers destroy each other: Alice pushing
  v3 runs `NOT versionId:v3`, which deletes Paul's concurrently-pushed v4 chunks.
  Both writes report success and one version's content is gone from the main index
  — precisely the destruction versioning exists to prevent. Naming `vPrev` means a
  concurrent writer's chunks are never in the delete set.
- **Copy to history before deleting from main** (3 before 4), so a crash between
  the two leaves a duplicate rather than losing the version.

The residual interleaving is benign: two versions' chunks may briefly coexist in
main and `distinct: 1` may return either. It self-heals on the next write, and
neither version is lost.

`contentHash` is the *same function over the same raw file bytes* as the hash
`read_file` already returns and `knownHash` compares (`fnv1a64:…`) — not computed
after chunking or frontmatter parsing. It is used for change detection and fork
display, no longer as a write guard.

## 6. Configuration

```json
{
  "vaultPath": "~/Documents/Algolia",
  "shared": [
    {
      "mountAt": "_Shared/DeepObsidian/",
      "appId": "XXXXXXXXXX",
      "indexName": "deep-obsidian-wiki",
      "keyRef": { "kind": "osKeyring", "service": "deep-obsidian-mcp", "account": "algolia-wiki" },
      "writable": true,
      "cache": {
        "maxBytes": "512MB",
        "evict": "lru",
        "pin": ["_Shared/DeepObsidian/_Wiki/Decisions/", "_Shared/DeepObsidian/_Wiki/Syntheses/"]
      },
      "retention": { "minVersions": 5, "maxAgeDays": 90 },
      "participantId": "paul.mairesse@algolia.com"
    }
  ]
}
```

**Cache: bounded LRU plus pinned prefixes.** Pinning is only an exemption from
eviction, so it is little extra code, and it addresses the real cold-start pain
(§10.3): reference notes that get reread constantly are otherwise evictable by a
single burst of one-off reads. Pinned prefixes are hydrated eagerly at startup, so
`load_knowledge` over the durable layer never pays a cold round trip.

Two properties to keep honest: pinned bytes count against `maxBytes` (so a pin
list larger than the budget is a config error, reported at startup, not a silent
overrun), and the cache is a cache — never a write buffer. Local edits to shared
notes are pushed, not held.

Also needed: **contributing local notes to the corpus** — an `export` rule naming
which local prefixes are pushed up (e.g. `_Wiki/` → the shared index). That is the
publishing direction, and it keeps the explicit-consent rules of §8.

Absent `shared`, behaviour is bit-identical to today. Keys use the existing
`SecretRef` model, never in the config file.

## 7. Tool behaviour on shared mounts

| Tool | Behaviour |
|---|---|
| `read_file` | hydrate from head chunks (exact, §4.1), cache, serve line ranges locally |
| `list_children` / `list_folders` | `filters: dir:"X"` + facet query on `folders.lvlN` |
| `find_files` | `restrictSearchableAttributes: ["path"]` |
| `hybrid_search`, `load_knowledge` | retrieve-then-rerank (§4.2); reports `recallStage` |
| `related_notes` | Algolia more-like-this → local embedding rerank |
| `graph_traverse` (incoming) | `filters: links:"<path>"` — one query, cheap |
| `note_outline` | `headings[]` on the note record, or the hydrated file |
| `grep_search` | candidate-bounded (§4.3); reports `exhaustive: false`, or refuses |
| `upsert_note`, `update_note_section` | write locally → append new version upstream |
| `request_vault_upload`, `search_artifacts` | local only, not shared in v1 |

New tools this design implies (small, and they are the payoff of versioning):

- **`note_history(path)`** — list versions with participant and timestamp.
- **`read_version(path, versionId)`** — read a superseded version.
- **`resolve_divergence(path)`** — return both diverged versions **and their common
  ancestor**, so an agent can perform a real three-way merge and push a
  reconciling version whose `parentVersionId` points at the head it resolved.

**No server-side auto-merge.** Agents merge markdown well, and a wrong automatic
merge in wiki content is very hard to notice afterwards — it produces plausible
text. Keeping merge logic out of the server means the server can never be silently
wrong; the cost is that reconciliation is an explicit act. Section-scoped writes
via `update_note_section` remain the way to avoid most forks in the first place,
since two participants editing different sections never diverge.

## 8. Publication consent

Pushing sends content off the machine to a service that replicates and indexes
it, and versioning means it is retained **by design** — an overwrite no longer
even removes the old copy. So retraction needs its own answer:

- **First push to a given index requires explicit confirmation with the full note
  list.** Confirmation is opt-out; a `--dry-run` flag is opt-in and only protects
  those who think to look.
- **Per-note opt-out** via `share: false` in frontmatter, visible to a human
  reading the note.
- **Retraction must be active, and it does purge history.** Three distinct paths
  remove a note from the shared set — deleted locally, gains `share: false` or is
  renamed out, or the export rule narrows — and only the first looks like a
  delete. Each must push a tombstone *and* purge the note's history records.
  Otherwise revoking a share leaves every prior version queryable, which is worse
  than before versioning existed.

  This is the deliberate exception to §3's non-destruction property, and the two
  must not be confused: **overwrites never destroy, retraction does.** Without
  that exception a mistaken push could never be withdrawn, which is untenable for
  a team wiki. So "recovery is a query" holds for editing accidents, not against
  an owner who explicitly unshares.
- `vault_info` reports shared mounts, export rules, cache size, and last sync.

## 9. Security

**Read scoping is native.**
[Secured API keys](https://www.algolia.com/doc/guides/security/api-keys/#secured-api-keys)
embed a `filters` restriction validated server-side, so a participant can be
scoped to a subset (`type:wiki-decision`, a folder facet, …).

**Write scoping is not.** Algolia write keys restrict by *index*, not by record.
So:

> A writable shared index is **write-trusted**: any participant with the write key
> can append a version to any note. Versioning makes this survivable — nothing is
> destroyed and `participantId` attributes every version — but it is not access
> control.

Versioning genuinely improves this over the earlier drafts: the failure mode of a
bad or careless write drops from "content lost" to "content superseded and
recoverable, with an audit trail". That is a meaningful mitigation, and it is why
Algolia-as-truth is now defensible where it was not before. Per-participant write
ACL still requires a proxy — but it is no longer the blocking concern it was.

**Read-only participants** get secured search keys and no write key. That is the
default for anyone who is not an intended contributor.

## 10. Remaining limits

1. **Semantic recall is lexically bounded without NeuralSearch** (§4.2). The single
   biggest weakness; reported via `recallStage` rather than implied.
2. **`grep_search` is candidate-bounded**, and refuses anchor-less patterns
   (§4.3).
3. **Cache misses cost a round trip.** First read of a cold note is network-bound;
   `load_knowledge` over many cold notes is the worst case. Mitigated by pinned
   prefixes (§6) and a bounded concurrent hydration path, not eliminated.
4. **Facet-derived structure**: empty folders do not exist; `list_folders` is
   capped at 1,000 facet values per query and needs pagination beyond that.
5. **Forks accumulate** if nobody reconciles. `hasDivergence` makes them visible
   and `resolve_divergence` makes them fixable; nothing forces resolution.
6. **No offline writes** to shared content — a push needs the network. Local-only
   notes are unaffected.
7. **History is bounded but not constant** (§3.1): peak size is "writes in the last
   90 days". A per-note clamp is the escape hatch if that ever bites.

## 11. Contract impact

Additive. `vaultPath` stays required, all 16 tools keep their names and schemas,
and a vault with no `shared` block behaves exactly as today. New clauses needed
for: shared mounts and the bounded cache; `grep_search` reporting
`exhaustive: false` and refusing unanchored patterns; `hybrid_search` reporting its
recall stage; append-only version semantics and the three new history tools;
explicit push consent and history-purging retraction.

### 11.1 Lazy index creation

An Algolia index does not exist until its first **write**; every read against a
never-written index answers `404 Index <name> does not exist`. Two consequences
the implementation has to honour:

- **All reads treat that 404 as "no records"** (`empty_if_missing_index`), so a
  first seed into a virgin app, a mount pointed at an empty index, and history
  reads before the first supersession all behave as empty rather than failing.
- **The history index is provisioned lazily.** Its settings cannot be applied
  before it exists, and it only exists once a note is first superseded — so
  `setSettings` runs immediately after that first history write (once per
  process, non-fatal on failure since the index is usable with defaults).

The mock mirrors this: reads against an unwritten index return the same 404.
The permissive auto-create it used to do hid the whole bug class from the tests.

### 11.1.1 Tombstones must be filtered out of every read

A soft-deleted note keeps its note record, so **every listing and search has to
exclude it** or the deleted note goes on appearing. `reads::LIVE_NOTES`
(`recordType:note AND NOT deleted:true`) is the single filter fragment used by
`list_children`, `list_folders`, `find_paths`, `backlinks`, `dump_all`, the seed
plan and consumer-side link resolution; `read_note` additionally treats a
tombstoned head as not-found.

Chunk queries deliberately do *not* carry the guard: chunk records have no
`deleted` attribute and a soft delete removes them outright, so a tombstoned
note has no chunks left to match.

### 11.2 Algolia request limits the mock must mirror

Two real-engine rejections that a permissive mock let through, both found by
running against a live app:

- **`maxFacetHits` caps at 100** on `searchForFacetValues` — over that Algolia
  400s rather than clamping (easy to confuse with `maxValuesPerFacet`, whose
  ceiling is 1,000). The client clamps, and structure enumeration reports
  `foldersTruncated` when the capped budget comes back full. Listing *paths*
  moved off facet search onto `browse` entirely, since 100 values would have
  silently truncated wiki-link resolution.
- **Empty filter values are rejected** (`filters: dir:""` → 400 "Not allowed
  empty string"). The mount root has `dir: ""` on its records, so root listing
  browses note records and matches the empty dir locally instead.

The mock now enforces both, so neither can regress unnoticed.

### 11.3 Algolia writes and settings are asynchronous

`batch` and `setSettings` return as soon as the task is QUEUED; the effect is
not observable until it is processed. Two consequences, both found by driving
the tools against a live app:

- **Every write awaits its task** (`save_objects_awaited`). Without it,
  "write, then read back to verify" — the last step of both the capture and
  maintenance skill flows — failed with "note not found" and only succeeded a
  few seconds later. Tasks on one index are processed in order, so awaiting the
  final head-pointer write also guarantees the chunk writes landed.
- **Settings edits await too** (`set_settings_awaited`). Declaring
  `attributesForFaceting` rebuilds the index, and a facet query issued before
  the task completed failed with "you need to add searchable(...) to
  attributesForFaceting".

Cost: roughly 3 s per mounted write against a real account. That is the price
of read-after-write consistency; batching several notes into one task would
amortise it and is the obvious optimisation if it starts to bite.

### 11.4 Index settings are provisioned lazily, by whoever writes first

Under mount-only authorship nothing runs a setup step, so **the first mount
write creates the index with Algolia's default settings** — no faceting (folder
listing fails outright), no `attributeForDistinct`, default searchable
attributes. `ensure_index_settings` therefore runs after the first write to
either index, once per process, and only when `attributesForFaceting` is absent
so a hand-tuned index (NeuralSearch, custom ranking) is never clobbered. A
failure is logged, not fatal: defaults still serve reads.

## 12. Implementation notes

**No official Algolia Rust client** — the
[API clients page](https://www.algolia.com/doc/libraries/sdk) lists none; crates.io
entries are unaffiliated. Raw REST over `reqwest`, already a dependency. Surface:
`saveObjects`, `getObjects`, `deleteBy`, `search`, `searchForFacetValues`,
`setSettings`, `browse`, plus secured-key generation (HMAC-SHA256, no API call).

**NeuralSearch** ([docs](https://www.algolia.com/doc/guides/ai-relevance/neuralsearch/get-started))
is now the difference between lexical-bounded and semantic recall (§4.2) — worth
confirming plan availability and whether it can be enabled by API, since it
materially changes retrieval quality rather than being a nice-to-have.

## 13. Staged plan

| Stage | Content | Ships value alone? |
|---|---|---|
| **0** | Consolidate the three divergent copies of `ensure_inside_vault` / `read_text_file` / `list_markdown_files` into `deep-obsidian-core`; add the symlink guard missing from the `index` copy | Yes — bug fix + dedup, independent of Algolia |
| **1** | Algolia REST client, dual-index provisioning (heads + history), secured-key generation | No — enabling work |
| **2** | **Export:** prefix selection, first-push confirmation, chunk push, note records | Yes — corpus queryable by the team |
| **3** | **Hydrating reads:** `read_file` (exact reassembly incl. overlap dedup), `list_children`, `list_folders`, `find_files`, `note_outline`, `graph_traverse`, bounded LRU cache | **Yes — a browsable shared corpus with no local mirror** |
| **4** | **Versioned writes:** append version, cutover ordering (explicit `vPrev` delete), history index, retention purge at write time, `note_history` / `read_version`, `hasDivergence` | **Yes — the shared wiki becomes writable and non-destructive. The milestone.** |
| **5** | **Retrieval:** adaptive first stage + NeuralSearch detection, retrieve-then-rerank, generalize `rrf_fuse` to N lists, candidate vector cache, `recallStage` reporting | Yes — semantic quality restored on shared content |
| **6** | `grep_search` candidate-bounded + refusal rules; `resolve_divergence` with common ancestor; retraction with history purge | Yes — closes the honesty and lifecycle gaps |

Stage 0 stands alone. **Stage 4 is the milestone.** Stage 3 is the one that proves
the premise — a shared corpus larger than the disk, browsable and readable, with
exact file reconstruction.

## Remaining unknowns

The four design forks are decided (see [Decisions taken](#decisions-taken)). What
is left is factual verification, not design:

1. **NeuralSearch availability** — is it enabled on the target app, and can it be
   turned on by API rather than the dashboard? The adaptive design (§4.2) works
   either way, so this is no longer blocking; it decides how much of onboarding
   `setup-service` can automate, and which recall tier is the common case. Answer
   before stage 5.
2. **`distinct` arithmetic** (§4.2) — with `attributeForDistinct: "path"` and
   `distinct: 1`, confirm how `hitsPerPage` maps to notes vs chunks before fixing
   the 200–500 candidate window.
3. **`distinct` under NeuralSearch** (§5) — confirm neural re-ranking and distinct
   de-duplication compose as expected.
4. **Export selection surface** (§6) — prefix + `share: false` frontmatter is
   assumed. Worth a second look if facet-based publishing ("every note with
   `type: wiki-decision`, wherever it lives") turns out to match how the wiki is
   actually organised.
