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
- `grep_search` — search note contents with ripgrep
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

1. **ripgrep** — `grep_search` exists if and only if a working `rg` was resolved. It is
   omitted rather than advertised-and-failing.
2. **multiple mounts** — a multi-mount vault adds a required `scope` argument to the recall
   tools that rank (`hybrid_search`, `load_knowledge`, `search_artifacts`). It adds no tool
   and removes none; a single-mount vault's list is unchanged.
3. **mount capabilities** — a mount declares what its storage can do
   (`vault_info.mounts[].capabilities`), and these four tools appear only when at least one
   mount has the capability they need:

| Tool | Needs | What it does |
|---|---|---|
| `note_history` | `version-history` | List a note's retained versions, newest first, with each version's author and timestamp. Retention keeps the most recent versions plus anything inside the mount's age window, so older versions may be absent. |
| `read_version` | `version-history` | Read one specific, possibly superseded version, reassembled from the mount's history. Take the `versionId` from `note_history`. |
| `resolve_divergence` | `version-history` | Return a diverged note's current head, the version it overtook, and their common ancestor, so **you** can three-way merge them. The server never merges: a wrong automatic merge produces plausible text and is nearly undetectable. Write the result with `upsert_note` and `resolveDivergence: true` to clear the mark. |
| `delete_note` | `soft-delete` | Soft-delete a note whose removal is observable to other participants and recoverable: it leaves listings and search, its previous version moves to history, and the content stays readable through `note_history` / `read_version`. |

`version-history` also adds the `resolveDivergence` argument to `upsert_note`.

The two capabilities are checked **separately**, because they come apart: a read-only shared
mount has a version history and no soft delete.

None of the four takes a `scope`. Each takes a `path`, so the mount is determined by
longest-prefix match exactly as it is for `read_file` — a `scope` would be a second,
redundant way to say the same thing, and a way for the two to disagree.

An advertised tool may still refuse a particular `path`: the mount that owns it may lack the
capability. That refusal names the mount, its backend kind, and the mounts that do support
the operation.

**`delete_note` is not deletion of local vault files, and this surface must never gain it.**
It exists only for a backend whose removal is observable and recoverable. A filesystem mount
refuses it, and a single-mount filesystem vault does not advertise it at all.

### Payload fields appear only when they are true

An incomplete answer must say so; a complete one must not carry the apparatus for saying so.
So these fields are absent unless they describe the actual case, and a backend that cannot
produce that case emits payloads byte-identical to the ones it always emitted:

- `list_children` gains `foldersTruncated` / `foldersTruncatedReason` only when a mount could
  not enumerate every subfolder. A mount whose folders are real directories never sets it.
- `grep_search` gains `exhaustive: false`, `candidateCount` and `exhaustiveNote` only when the
  search could not read every line in scope. A ripgrep-served search emits none of them, so
  an absent `exhaustive` still means exhaustive.

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
