# MCP reference

The tools, resources, and prompts the server exposes to MCP clients. For setup
and usage, see the top-level [USAGE.md](../USAGE.md).

## Tools

- `vault_info` — vault metadata and index status
- `load_knowledge` — load durable project/agent knowledge
- `recommend_folder` — suggest a destination folder for a note
- `list_children` — list a folder's contents (`foldersOnly:true` for subfolders only)
- `read_file` — read a whole note or a line range (`startLine`/`endLine`)
- `find_files` — find notes by substring or regex path match
- `grep_search` — search note contents with ripgrep (or, on a CouchDB mount, an exhaustive
  imitation of it — see "Line search per mount kind")
- `build_index` — force an explicit index rebuild
- `hybrid_search` — BM25 + semantic ranking (`bm25Weight:0` = semantic-only, `semanticWeight:0` = BM25-only)
- `related_notes` — notes related by subject similarity
- `graph_traverse` — traverse wiki-link graph (`direction:"incoming"`, `depth:1` for backlinks)
- `upsert_note` — create/update a markdown note
- `update_note_section` — replace the preamble or a named heading section
- `request_vault_upload` — mint an out-of-band upload URL for binary/large files
- `upsert_session_note` — create/update a session note

### Conditionally advertised tools

`tools/list` is computed once per process, and exactly three things may change what it
contains. Everything else about it is frozen. The rule throughout: **a tool that could only
ever refuse is not advertised.**

1. **ripgrep** — `grep_search` exists if and only if the ROOT mount declares
   `grep-search`, which on a filesystem root means a working `rg` was resolved. It is
   omitted rather than advertised-and-failing. A non-root CouchDB mount declares
   `grep-search` without needing `rg` (it imitates it in-process), but `tools/list` cannot
   say "available for some paths", so registration stays keyed on the root; per-mount truth
   is in `vault_info.mounts[].capabilities`.
2. **multiple mounts** — a multi-mount vault adds a required `scope` argument to the recall
   tools that rank (`hybrid_search`, `load_knowledge`, `search_artifacts`). It adds no tool
   and removes none; a single-mount vault's list is unchanged.
3. **mount capabilities** — a mount declares what its storage can do
   (`vault_info.mounts[].capabilities`), and these four tools appear only when at least one
   mount has the capability they need:

| Tool | Needs | What it does |
|---|---|---|
| `note_history` | `version-history` | List a note's retained versions, newest first, with each version's author and timestamp. Retention keeps the most recent versions plus anything inside the mount's age window, so older versions may be absent. `limit` (default 50, max 500) bounds the answer; because the order is newest-first it keeps the most recent versions, and when it cut the list short the payload also carries `truncated: true`, `totalCount` and `truncationNote`. An untruncated answer carries none of those keys. |
| `read_version` | `version-history` | Read one specific, possibly superseded version, reassembled from the mount's history. Take the `versionId` from `note_history`. |
| `resolve_divergence` | `version-history` | Return a diverged note's current head, the version it overtook, and their common ancestor, so **you** can three-way merge them. The server never merges: a wrong automatic merge produces plausible text and is nearly undetectable. Write the result with `upsert_note` and `resolveDivergence: true` to clear the mark. |
| `delete_note` | `soft-delete` | Soft-delete a note whose removal is observable to other participants and recoverable: it leaves listings and search, and the payload's `howToRecover` says how to undo it **on that mount**. Recovery differs per backend — see the table below. |

`version-history` also adds the `resolveDivergence` argument to `upsert_note`.

The two capabilities are checked **separately**, because they come apart **in both
directions**: a read-only Algolia mount has a version history and no soft delete; a writable
CouchDB mount has a soft delete and no version history.

#### What a delete leaves behind, per backend

`delete_note` means the same thing to a caller everywhere — the note leaves every listing,
every enumeration and every search, and the removal replicates to the other participants —
but what is left to recover from differs, so the payload states it per call rather than
letting a client assume.

| Mount | `recoverableFrom` | How to undo it |
|---|---|---|
| Algolia | The version the tombstone superseded | `read_version` that version, then `upsert_note` it back. The whole retained history survives the delete. |
| CouchDB (LiveSync) | **Absent** — this mount has no `version-history` capability, so there is no `versionId` any tool could read | Reading the path still returns its **last** content — `read_file` for a note, `read_artifact` for an attachment — because the tombstone keeps the stored content it was made from. `upsert_note` it back and the note is live again, on every device that syncs the vault. Nothing older than that last content survives, and `note_history` / `read_version` do not exist for such a vault. |
| Filesystem | n/a | Refused. This surface exposes no deletion of local vault files. |

An absent `recoverableFrom` therefore means "there is no version a versioned read could
serve", **not** "the content is gone". `howToRecover` is present on every successful delete
and is the field to follow.

Deleting an already-deleted note is a successful no-op with `alreadyDeleted: true`, and on a
CouchDB mount it reports the revision the tombstone already had — a repeated delete
replicates nothing.

None of the four takes a `scope`. Each takes a `path`, so the mount is determined by
longest-prefix match exactly as it is for `read_file` — a `scope` would be a second,
redundant way to say the same thing, and a way for the two to disagree.

An advertised tool may still refuse a particular `path`: the mount that owns it may lack the
capability. That refusal names the mount, its backend kind, and the mounts that do support
the operation.

**`delete_note` is not deletion of local vault files, and this surface must never gain it.**
It exists only for a backend whose removal is observable and recoverable. A filesystem mount
refuses it, and a single-mount filesystem vault does not advertise it at all.

A remote mount that is **read-only** refuses it too, and says so differently: the backend
can soft-delete, but the mount did not set `"writable": true`. The refusal names that setting
rather than claiming the removal would be a local unlink.

### Payload fields appear only when they are true

An incomplete answer must say so; a complete one must not carry the apparatus for saying so.
So these fields are absent unless they describe the actual case, and a backend that cannot
produce that case emits payloads byte-identical to the ones it always emitted:

- `list_children` gains `foldersTruncated` / `foldersTruncatedReason` only when a mount could
  not enumerate every subfolder. A mount whose folders are real directories never sets it.
- `grep_search` gains `exhaustive: false`, `candidateCount` and `exhaustiveNote` only when the
  search could not read every line in scope. A ripgrep-served search emits none of them, so
  an absent `exhaustive` still means exhaustive.

#### Line search per mount kind

The match shape is identical everywhere. What differs is completeness and cost:

| Mount | How | Exhaustive? | Cost |
|---|---|---|---|
| Filesystem | spawns `rg` | yes | one tree walk |
| CouchDB (LiveSync) | exhaustive virtual scan: manifest → `glob` pre-filter → read every candidate note through the sidecar and match in-process | yes | **a full corpus read per query**; no cache. Narrow the `glob` or lower the `limit` |
| Algolia | candidate-bounded: top-200 lexical prefilter, then the pattern | **no** | one search request |

So a CouchDB grep is complete (no `exhaustive` key, like ripgrep's) but slow on a large
vault, and an Algolia grep is fast but reports `exhaustive: false` with a `candidateCount`.
An unscoped grep on a multi-mount vault concatenates every mount's matches in config order.

#### Recall on a native-recall mount

`hybrid_search` and `load_knowledge` gain `nativeRecall`, `recallMode`, `mountId` and
`exhausted` only when the answer came from a **mount's own index** rather than from the local
one — and then omit `semanticBackend`, `degraded`, `semanticScore` and `bm25Score`, which
describe the local ranker and would be fabricated values here.

`exhaustive` and `exhausted` are different facts and are not unified:

- `exhaustive: false` (grep) — the search did not look everywhere, so an empty result is not
  proof of absence;
- `exhausted: false` (recall) — the search looked everywhere, and there are simply more
  results than `limit` asked for.

No payload advertises a continuation a caller cannot take: a tool that reports more results
must either declare the argument that fetches them or offer no cursor at all.

### Authoring tool notes

- **`upsert_note`** — generic create/update with explicit `content`, or
  `frontmatter` + `title` + `body` — mutually exclusive modes (the input
  schema encodes this as a `oneOf`). Sending both `content` and `body` is
  accepted only when their text is identical (the call succeeds with a
  `warning` in the result); diverging text is rejected. No implicit title
  injection.
- **`update_note_section`** — patch the preamble or one heading section without
  rewriting the whole note.
- **`request_vault_upload`** — for binary or large non-markdown files, returns a
  short-lived capability URL to `PUT` the bytes to.
- **`list_children`** — inspect real vault structure instead of inferring it from
  search (`foldersOnly:true` lists only subfolders).
- **`upsert_session_note`** accepts either:
  - `topic` + `folder` to derive the canonical `Session - <slug>.md` path, or
  - an explicit vault-relative `path` to update a known note deterministically
    (takes precedence over `topic`/`folder`).
  It writes the markdown body as-is and does **not** auto-insert a title —
  include one only if you want it saved.

## Resources

- `obsidian://vault/info`
- `obsidian://note?path=...`
- `obsidian://heading?path=...&slug=...`
- `obsidian://block?path=...&id=...`

## Prompts

Read/synthesis workflows exposed as MCP prompts:

- `obsidian-load-context`
- `obsidian-project-briefing`
- `obsidian-daily-review`

## Packaged skills

Agent skill templates (installed with `setup-service --skills`) for operational
workflows:

- `obsidian-wiki-init`
- `obsidian-capture-session`
- `obsidian-knowledge-maintenance`

See also [agent-workflows.md](./agent-workflows.md).

The frozen contract these tools must not drift from is
[behavior-contract.md](./behavior-contract.md). For the experimental multi-mount surface —
`scope`, per-mount capabilities, and the storages behind them — see
[CONFIGURATION.md § Multiple vaults](../CONFIGURATION.md#multiple-vaults-mounts) and
[algolia-mounts.md](./algolia-mounts.md).
