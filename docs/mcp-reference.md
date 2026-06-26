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

### Authoring tool notes

- **`upsert_note`** — generic create/update with explicit `content`, or
  `frontmatter` + `title` + `body`. No implicit title injection.
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

See also [agent-workflows.md](./agent-workflows.md) and the snippet-backed
writing-conventions pattern in
[writing-conventions-pattern.md](./writing-conventions-pattern.md).
