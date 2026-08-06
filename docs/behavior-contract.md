# Behavior Contract

This document defines the maintained Rust service surface.

## Scope

The contract covers:

- configuration resolution
- service startup and health probing
- MCP tool and resource availability
- vault-relative note reads and graph traversal
- fixture-based verification assets used by Rust tests

It does not define internal module layout.

## Configuration

Canonical config shape:

- `vaultPath`
- `indexDir`
- `transport`
- `stdioMode`
- `http.host`
- `http.port`
- `http.mcpPath`
- `http.healthPath`
- `autoReindex.enabled`
- `autoReindex.debounceMs`
- `autoReindex.intervalMs`
- `embedding.provider`
- `embedding.model`
- `embedding.baseUrl`
- `embedding.apiKeyRef`

Resolution precedence:

1. CLI flags
2. config file
3. environment variables
4. defaults

Rules:

- A vault ROOT is required before service startup, and it may be any backend kind. A legacy
  config expresses it as `vaultPath`; a mount table expresses it as the mount with
  `mountAt: ""`. Exactly one of the two, never both, and never neither: a table with no root
  mount is refused, because `""` is the only prefix that matches every path and without it a
  path outside every declared prefix would resolve to nothing.
- `vaultPath` is therefore absent from a config whose root mount is a CouchDB or Algolia
  backend — such a vault has no local directory at all. Everywhere a vault location is
  REPORTED (`doctor`'s `vault:` line, `healthz`/`readyz`/`vault_info`'s `vaultPath`) a remote
  root renders `url/database` or `appId/indexName`, which carries no credential. A filesystem
  root renders the same path it always did, byte for byte.
- `transport` must default to `stdio` for subprocess use and `http` for service wrappers.
- `http.mcpPath` and `http.healthPath` must normalize to leading-slash paths.
- `embedding.apiKeyRef` stores a reference to a secret, not the secret itself.
- `doctor`, `print-config`, `probe`, and readiness output must never print resolved secret values.
- Encrypted local secret storage prevents accidental plaintext exposure in config files. For stronger local protection, use the OS keyring provider. The encrypted-file fallback is not equivalent to OS keyring storage because the application carries the decryption key.
- `setup-service` and packaged mode choose an index directory outside the vault when no index directory is explicitly resolved; explicit CLI, config file, or environment values must be preserved.
- Packaged mode is opt-in through `--packaged` or `DEEP_OBSIDIAN_PACKAGED=1`; ad-hoc dev commands without that opt-in keep the vault-local default index directory for compatibility.

## Service Contract

The service must expose:

- an MCP endpoint over HTTP
- a health endpoint
- stdio MCP support for subprocess use

Health responses should include enough metadata to diagnose startup and indexing issues:

- `status`
- `vaultPath`
- `generatedAt` or equivalent index timestamp
- `semanticBackend`
- `autoReindex`

The health endpoint must be lightweight and read-only. It must not trigger index refresh, rebuild, embedding calls, or filesystem mutation.

Readiness is distinct from health. The service should expose a readiness endpoint, currently `/readyz`, that reports whether the index is usable, still loading, or degraded. Packaging and service managers should use readiness, not health alone, as the MCP usability gate.

### Startup failure: fail fast, or start degraded

Which of the two a failing mount causes depends on the KIND of failure, not on where the
mount sits:

- **A mount backed by a LOCAL DIRECTORY at the vault root fails startup.** A directory that
  is missing, misspelled or unreadable is a permanent configuration error; nothing about
  waiting improves it, and a green process serving errors for the whole vault would hide the
  mistake. This is unchanged, and it covers every legacy `vaultPath` config. It applies to
  stdio too, where a client can show a startup failure but has nowhere to show a readiness
  probe.
- **Everything else starts DEGRADED.** Any non-root mount, and a REMOTE mount at the vault
  root. An unreachable remote is an outage: the url and credentials are right and the server
  is down, slow, or mid-rebuild. Making that fatal would let a network blip brick a service a
  supervisor then restart-loops, and would take the healthy mounts down with it.

A degraded start is honest, not silent. Readiness answers 503, the failing mount is named,
health says so, and every path on that mount refuses with the backend's own reason — never
with an empty result, which a caller could not distinguish from an empty vault. Other mounts
serve normally.

A degraded remote mount must also be able to RECOVER without a process restart. A CouchDB
mount re-hand-shakes on a bounded backoff until its remote answers, so a service that came up
while CouchDB was down comes back by itself; an Algolia mount probes its index on every call,
so it never caches a stale verdict. The retry stops when the remedy is in the mount's own
configuration — rejected credentials, a wrong passphrase — because a running process cannot
re-read its config, and retrying would be a stream of failed logins that can never succeed.

## Agent Workflows

The MCP API may expose additive prompts for common Obsidian workflows. These prompts must not replace or rename existing tools. They should guide clients toward safe tool use: narrow retrieval, outline-first inspection, graph-aware context, dry-run for broad writes, and hash guards for existing-note updates.

Packaged skill templates are documentation-like agent instructions, not runtime configuration. Installing or omitting them must not change the server's tool behavior.

`doctor` should also report non-secret local diagnostics, including config source attribution, auto-reindex settings, MCP/health/readiness endpoint URLs, index SQLite path and size, index schema metadata when available, and the latest health/readiness payload when service endpoints respond.

The service should fail fast when required config is missing or a LOCAL root vault cannot be read. A root vault that is remote and unreachable is an outage rather than a missing config, and starts degraded instead — see "Startup failure" above.

## MCP Contract

The black-box surface must preserve:

- `vault_info`
- `load_knowledge`
- `recommend_folder`
- `list_children` (with `foldersOnly` flag for subfolder-only listing)
- `read_file`
- `find_files`
- `grep_search`
- `build_index`
- `hybrid_search` (with `bm25Weight`/`semanticWeight` flags for BM25-only or semantic-only ranking)
- `related_notes`
- `graph_traverse` (with `direction:"incoming"` for backlinks)
- `upsert_note`
- `update_note_section`
- `request_vault_upload`
- `upsert_session_note`

Conditionally advertised (see "Tool availability" below): `grep_search`, `delete_note`,
`note_history`, `read_version`, `resolve_divergence`.

### Tool availability is environment- and capability-dependent

`tools/list` is computed once per process. Three inputs, and only these three, may change
what it contains. Everything else about it is frozen.

1. **Environment and mounts — line search.** `grep_search` is advertised **if and only if
   at least one mount declares `grep-search`**. A filesystem mount declares it only when a
   working `rg` was resolved; a CouchDB mount and an Algolia mount always declare it, since
   they imitate ripgrep above their own storage rather than spawning one (see "Line search
   per backend" below). "Some mount can serve line search, or `grep_search` does not exist":
   it is omitted rather than advertised-and-failing.

   This used to be keyed on the ROOT mount, which was defensible only while the root was
   guaranteed to be a local directory. It no longer is — a CouchDB or Algolia backend may be
   the vault root — and asking the root would then answer for the wrong thing entirely,
   reporting no line search for a fully-remote vault that greps perfectly well.

   The widened rule changes exactly one configuration that could already exist: a filesystem
   root on a host with no `rg`, plus a remote mount. That used to hide `grep_search` and now
   advertises it. Advertising is the honest answer, because an unscoped grep FEDERATES and
   tolerates a mount that cannot serve it — the capable mounts return their matches and the
   incapable ones are named in the payload's `missingMounts` with `exhausted: false`. A
   caller gets real matches plus an explicit statement of what was not searched, instead of a
   tool that does not exist for a vault where most of it works. The converse — hiding the
   tool because SOME mount cannot grep — is rejected for the same reason: it would remove
   line search from a vault it works on.

   A vault with no grep-capable mount at all still gets no `grep_search`, which is what keeps
   the frozen single-mount tool list unchanged: one filesystem mount with no `rg` declares
   no capability, so the tool is absent exactly as before.

   Per-mount truth stays in `vault_info.mounts[].capabilities`; a caller that has read
   `grep-search` there can scope a grep to that mount.
2. **Configuration — multiple mounts.** A multi-mount vault adds an OPTIONAL `scope`
   argument to the recall tools that rank (`hybrid_search`, `load_knowledge`,
   `search_artifacts`). It adds no tool and removes none. A single-mount vault's list is
   unchanged. `scope` must not be required: omitting it asks for the federated answer
   over every mount, and declaring it required would tell a client the whole-vault
   search does not exist. Each `scope` description must state what omitting it does, or
   a client reading only the schema cannot discover federation at all.
3. **Configuration — mount capabilities.** A mount declares what its storage can do
   (`vault_info.mounts[].capabilities`), and tools whose whole purpose depends on a
   capability are advertised only when at least one mount has it:
   - `version-history` adds `note_history`, `read_version` and `resolve_divergence`, and
     adds the `resolveDivergence` argument to `upsert_note`;
   - `soft-delete` adds `delete_note`.

   The two are checked separately, because they come apart: a read-only shared mount has a
   version history and no soft delete.

The same discipline governs all three: a tool that could only ever refuse is not
advertised. A tool that IS advertised may still refuse for a particular `path` — the mount
that owns it may lack the capability — and that refusal names the mount, its backend, and
the mounts that do support the operation.

This surface must never gain deletion of local vault files. `delete_note` exists only for a
backend whose removal is observable to other participants and recoverable from the note's
own version history; a filesystem mount refuses it, and a single-mount vault does not
advertise it at all.

### Line search per backend

`grep_search` means the same thing to a caller on every mount — the same match shape, the
same context, the same line numbers — but it is served three different ways, and only the
honesty carriers in the payload distinguish them.

| Mount | How line search is served | Exhaustive? | Cost |
| --- | --- | --- | --- |
| Filesystem | `rg` is spawned over the vault directory | Yes | One process, one tree walk |
| CouchDB (LiveSync) | An **exhaustive virtual scan**: the manifest supplies the corpus, the `glob` pre-filters it by path, then every surviving note is read back through the sidecar and matched line by line in-process, imitating ripgrep's own semantics | Yes | **One full corpus read per query** — a round trip per candidate note, decrypting on an E2EE vault. Narrow the `glob` or lower the `limit` to narrow the scan; there is no content cache |
| Algolia | **Candidate-bounded**: a lexical prefilter pulls the top-200 chunks, then the pattern runs over those | No | One search request; a match the index ranked below the cap is not found |

Consequences a caller can rely on:

- A CouchDB-served grep emits NO `exhaustive` key, exactly like a ripgrep-served one,
  because it did look everywhere. An Algolia-served one emits `exhaustive: false` plus
  `candidateCount`.
- The CouchDB scan runs in sorted path order, so a `limit`-truncated result is
  deterministic. Ripgrep walks in parallel, so which matches survive a truncation there is
  not. Neither claims to have stopped looking: `limit` truncates OUTPUT, and `exhaustive`
  is about whether there was a candidate shortlist.
- A federated (unscoped) grep concatenates every mount's matches in config order,
  CouchDB mounts included. A mount whose grep FAILS is named in `missingBackends` with
  `degraded: true` and `exhaustive: false`.
- A CouchDB grep applies the same visibility rules as everything else on that mount:
  tombstones, binary attachments, hidden paths and ignored directories are excluded.
  Ripgrep is run with
  `--hidden` and so does search hidden and ignored directories on a filesystem mount; it
  also honours `.ignore`/`.gitignore` files, which have no equivalent in a LiveSync vault.
  The `glob` — not the `.md` extension — decides which of the remaining entries are read,
  so `glob: "*.txt"` reaches text notes on both kinds of mount.

### Payload additions are conditional on being true

An answer that is incomplete must say so, and an answer that is complete must not carry the
apparatus for saying so. So these fields appear ONLY in the case they describe, and a
backend that cannot produce that case emits payloads byte-identical to the ones it always
emitted:

- `list_children` gains `foldersTruncated` / `foldersTruncatedReason` only when a mount
  could not enumerate every subfolder. A mount whose directories are real directories never
  sets it.
- `grep_search` gains `exhaustive: false`, `candidateCount` and `exhaustiveNote` only when
  the search could not read every line in scope. A ripgrep-served search emits none of them,
  so an absent `exhaustive` continues to mean what it has always meant: exhaustive.
- `hybrid_search` and `load_knowledge` gain `nativeRecall`, `recallMode`, `mountId` and
  `exhausted` only when the answer came from a mount's OWN index rather than from the local
  one — and then omit `semanticBackend`, `degraded`, `semanticScore` and `bm25Score`, which
  describe the local ranker and would be fabricated values here.

`exhaustive` and `exhausted` are different facts and must not be unified. `exhaustive:
false` (grep) means the search did not look everywhere, so an empty result is not proof of
absence. `exhausted: false` (recall) means the search looked everywhere and there are simply
more results than `limit` asked for.

No payload may advertise a continuation a caller cannot take. A tool that reports more
results available must either declare the argument that fetches them or offer no cursor at
all.

`upsert_note` must preserve explicit author control. If `content` is provided, it must be written as-is. If `title` or `frontmatter` are provided, they must only be written when explicitly requested. `content` and the compose fields (`body`/`title`/`frontmatter`) are mutually exclusive; as a robustness concession to clients that fill every schema property, a call providing both `content` and `body` with identical text succeeds (writing `content`, with a `warning` in the result), while diverging text is rejected.

`upsert_session_note` must preserve the provided markdown body as-is, except for optional trailing `## Manual Notes` preservation when requested. It must not inject an implicit title or heading.

Resources must preserve:

- `obsidian://vault/info`
- `obsidian://note?path=...`
- `obsidian://heading?path=...&slug=...`
- `obsidian://block?path=...&id=...`

## Multi-mount vaults

A vault may be composed of several **mounts**, each a prefix of the logical namespace
served by its own storage backend. This is experimental and off by default: a config with
no `mounts` table resolves to exactly one filesystem mount at the root and every rule
below collapses to what it has always been.

Any mount may be the ROOT, including a remote one, so a vault may have no filesystem in it at
all — a CouchDB root on its own, or a CouchDB root with an Algolia mount under it. Such a
table needs no `multiVault` flag when it has one entry: a one-mount table is the legacy shape
spelled out longhand. The rules below apply to a fully-remote vault exactly as written; the
only thing that changes is which mount is at `""`. The shape of the config, and the per-backend
setup, live in [CONFIGURATION.md § Multiple vaults](../CONFIGURATION.md#multiple-vaults-mounts)
and [docs/algolia-mounts.md](./algolia-mounts.md); this section states only the rules a
*client* can rely on, and it is a reference — the per-doc details are not repeated here.

### One namespace, one path vocabulary

Every path a client sends or receives is **logical**: relative to the vault root, with the
owning mount's prefix included. A client never sees a backend's own addressing, and a
mount's internal layout is not discoverable from a path. Routing is longest-prefix over
`mountAt`, so exactly one mount owns any given path.

### The honesty rules

These are the same rules stated per-payload under "Payload additions are conditional on
being true" above, and they are collected here because multi-mount is where they all
become reachable at once:

- **`degraded` / `missingBackends` / `degradationReason`** — a federated answer that could
  not consult every mount says so, names the mounts it lost, and says what happened. These
  describe *the answer just produced*, not a latched state: once the mount answers again,
  the same query stops being degraded and stops naming it.
- **Skipped is not missing.** A mount that legitimately holds nothing a tool asks for (an
  Algolia mount has no artifact store) is reported as skipped with a reason, and the answer
  is **not** degraded. Nothing was omitted. Calling that a missing backend would teach a
  reader to ignore `missingBackends`.
- **`foldersTruncated`** — a mount that could not enumerate every subfolder says so. A
  mount whose folders are real directories never sets it.
- **`exhaustive` (grep) and `exhausted` (recall) are different facts** and must not be
  unified. `exhaustive: false` means the search did not look everywhere, so an empty result
  is not proof of absence. `exhausted: false` means the search looked everywhere and there
  are more results than `limit` asked for.
- **Native recall is labelled.** When a mount ranked with its own index rather than the
  local one, the result carries `nativeRecall`, `recallMode`, `mountId` and `exhausted`,
  and omits the fields that describe the local ranker — which would otherwise be fabricated
  values.
- **Enumeration refuses rather than under-reporting.** A whole-vault listing that cannot
  reach a mount fails, naming the mount. A listing that silently omitted it would assert
  those notes do not exist.
- **Readiness names the mount.** A failing mount degrades readiness (`503`,
  `degradedMounts`); when it is a non-root mount the vault root keeps serving, and when it is
  a remote root the server still starts so it can say so. The frozen top-level wording is
  unchanged; the mount id appears in the additive per-mount detail rather than being laundered
  into `lastError`.
- **No local index is not a degraded index.** An Algolia mount has none by design, so it
  reports `indexStatus: "none"` and `localIndex: false` and never appears in
  `degradedMounts`. When such a mount is the vault ROOT there is no root index at all: the
  index-derived tools (`vault_info`, `build_index`, `recommend_folder`, the vault-overview
  resource) refuse with the reason a scoped call on an index-less mount already gives, reads
  and writes work normally, and readiness stays green with no index statistics in the
  payload rather than another mount's numbers presented as the vault's.

### Capability gating, and its one exception

A mount declares what its storage can do (`vault_info.mounts[].capabilities`). A tool whose
whole purpose depends on a capability is advertised only when at least one mount has it, and
an advertised tool may still refuse a particular path whose owning mount lacks it — naming
the mount, its backend, and the mounts that do support the operation. See "Tool availability"
above for the current mapping.

The exception is **binary content**, and it is per-path rather than per-mount. An Algolia
mount stores Markdown only — there is no record shape for bytes — so it advertises Markdown
capabilities normally and refuses every binary operation on its prefix, including a raw byte
read of a `.md` path. `request_vault_upload` is refused **at the mint**, before a token is
issued, so a caller never streams a body to an impossible destination. These refusals must
not mention `writable`: turning writes on cannot help, and naming a setting that would not
fix it sends the reader to the wrong place. See
[docs/algolia-mounts.md § The binary exception](./algolia-mounts.md#the-binary-exception).

### What multi-mount does not promise

- **A write is never fanned out.** A write lands in the single mount owning its path.
- **No content crosses a backend boundary.** Mounts meet as *ranks*: fusion consumes a
  logical path, a chunk index, a rank and a mount id, and nothing else. The final rerank
  does read candidate text and vectors, but it reads and scores them inside the server, over
  content the server already holds to render snippets — no mount is ever asked to score
  another mount's content, and nothing is sent to a backend that did not already have it.
- **Change cursors are not persisted.** A backend rebuilt from config starts with no cursor
  and replays from the beginning, so it misses nothing but does more work. Cursors survive a
  child-process restart within one supervisor's life, not a server restart.
- **Recovery is eventual, not immediate.** A CouchDB mount whose remote was unreachable when
  the sidecar hand-shook re-hand-shakes on a bounded backoff (capped at 30s) until the remote
  answers, with no process restart — but the verdict is cached between attempts, so there is a
  window after the remote returns in which the mount still refuses. A mount whose verdict
  could only be fixed by editing the config is not retried at all; see "Startup failure"
  above.

## Fixture Vault Contract

Verification scripts use a tiny fixture vault with these invariants:

- the vault contains a small graph of linked notes
- at least one note has a direct wiki-link path to another note
- the fixture names are stable and predictable
- the fixture is readable without any user-specific configuration

Expected fixture root for CLI and integration tests:

- `tests/fixtures/vault`

## Verification

Preferred verification commands:

- `cargo test --workspace`
- `cargo run -p deep-obsidian-cli --bin deep-obsidian-mcp -- doctor --vault tests/fixtures/vault`
- `cargo run -p deep-obsidian-cli --bin deep-obsidian-mcp -- print-config --vault tests/fixtures/vault`
- `cargo build --release -p deep-obsidian-cli --bin deep-obsidian-mcp`
- `codesign --force --sign - --timestamp=none target/release/deep-obsidian-mcp`
- `codesign --verify --verbose=2 target/release/deep-obsidian-mcp`

The maintained runtime path is Rust only.
