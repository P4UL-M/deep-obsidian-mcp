# Changelog

All notable changes to deep-obsidian-mcp are documented here.

## v0.2.0-alpha.1 — PENDING

Date set when the tag is pushed — replace `PENDING` with the tag date as the first
step of the release (see [docs/release-checklist.md](./docs/release-checklist.md)).

### Added

- **Multi-backend vaults (experimental, off by default).** A vault can now be
  composed of several **mounts**, each grafting other storage onto a folder of
  your vault while your agent keeps seeing one namespace. Nothing changes for a
  config with no `mounts` table: it resolves to exactly one filesystem mount at
  the root and every payload stays byte-identical to what it was. Every backend
  beyond the filesystem is behind its own `experimental` flag and is read-only
  until you opt in per mount.

  - **A `VaultBackend` boundary.** All vault IO — reads, listings, writes,
    uploads, grep, recall — goes through one trait, with the filesystem as its
    first implementation and a parameterized conformance suite every backend must
    pass. Indexing was generalized behind a `NoteSource` trait so a mount's index
    no longer assumes a directory.
  - **A multi-mount router.** Longest-prefix routing over `mountAt`: exactly one
    mount owns any path, each mount gets its own runtime and index in its own
    directory, and a write lands in the single mount that owns its path.
  - **CouchDB / Self-hosted LiveSync mounts** (`experimental.couchdbVaults`).
    Mount a LiveSync database as a folder of your vault. Served by a supervised
    Node **sidecar** speaking a versioned stdio protocol, pinned to the upstream
    library triple it was built against so a schema drift fails closed instead of
    reassembling notes wrong. Reads, listings, live change feed, revision-guarded
    writes, soft deletes, conflict enumeration, binary uploads, and export/restore.
    E2EE and path obfuscation supported. **`grep_search` works on a LiveSync
    mount**: with no files for ripgrep to open, the backend imitates ripgrep
    in-process over note text read back through the sidecar — same pattern
    semantics (`regex`/`fixedStrings`/`caseSensitive`, ripgrep's own glob engine),
    same match shape, same context lines. It is exhaustive in the same sense
    ripgrep is, so its payload carries no `exhaustive` key, and a differential test
    asserts its output is byte-identical to real ripgrep's over the same corpus.
    The cost is stated rather than hidden: one full corpus read per query, so
    narrow the `glob` or lower the `limit` on a large vault.
  - **Algolia mounts** (`experimental.algoliaVaults`). Mount a **shared,
    Markdown-only** corpus that a whole team can mount at once, with per-note
    version history, guarded fork-on-stale writes, divergence recording and
    resolution, retention, and a `note`/`chunk` record model. Ranking is served by
    Algolia itself rather than by a local index.
  - **Federated recall.** An unscoped `hybrid_search` / `load_knowledge` /
    `search_artifacts` asks every mount and fuses the answers with weighted
    reciprocal-rank fusion plus a final single-ranker rerank, so one mount's best
    hit can actually be compared with another's. Optional per-mount
    `recallWeight`; `federatedRerank: false` exposes the pure fusion order. Recall
    quality is gated against the single-index baseline by a dedicated eval suite.
  - **Capability-gated tools.** `note_history`, `read_version`,
    `resolve_divergence` and `delete_note` are advertised only when a mount can
    actually serve them, and refuse per-path by naming the mount, its backend, and
    the mounts that do support the operation. A tool that could only ever refuse is
    not advertised at all.
  - **`delete_note` works on a writable CouchDB mount**, as a LiveSync tombstone:
    the note leaves every listing, enumeration and search immediately, the removal
    replicates to your other devices, and a repeated delete is a no-op that costs no
    write. Recovery differs from an Algolia mount and the payload says so per call
    rather than assuming a history exists: a LiveSync vault has no readable revision
    history, so no `recoverableFrom` is offered — instead the tombstone keeps the
    note's last content, so reading the path still returns it (`read_file`, or
    `read_artifact` for an attachment) and `upsert_note` writing it back resurrects
    it. A read-only CouchDB mount refuses the delete by naming
    `"writable"`, and a filesystem mount still refuses it outright: this surface
    exposes no deletion of local vault files.
  - **Any backend can be the vault ROOT, including a fully-remote vault.** A
    `couchdb` or `algolia` mount may sit at `mountAt: ""`, so a vault can have no
    local directory in it at all — a LiveSync database on its own, or a LiveSync
    root with a shared Algolia corpus grafted under it. A one-mount table needs only
    that backend's own experimental flag. What is still refused is a table with *no*
    root mount: `""` is the only prefix that matches every path, and without it a
    typo in a prefix would silently become "no such path". Nothing changes for a
    filesystem-rooted config, and every frozen payload is byte-identical: `vaultPath`
    still renders the same directory through the same code, and a remote root renders
    a secret-free `url/database` or `appId/indexName` instead of an empty line.
    `setup-service --vault-snippets` reports `skip` on a remote root (there is no
    `.obsidian` folder to install into) without taking `--mcp` and `--skills` down
    with it, `doctor`'s `vault` check reports `ok` and points at `--probe-remote`, and
    a remote root's index defaults to the application data directory rather than
    inside a vault that does not exist. An `algolia` root has no local search index by
    design, so the index-derived tools refuse on such a vault while reads, writes,
    listings and `grep_search` all work; a `couchdb` root has one, so a fully-remote
    LiveSync vault keeps the whole surface.
  - **An unreachable remote root starts DEGRADED and recovers by itself.** A missing
    *local* root directory still aborts startup — it is a permanent mistake, and a
    green process serving errors for the whole vault would hide it. A remote root that
    cannot be reached is an outage instead, so the service starts, `/readyz` answers
    503 naming the mount, and every path refuses with the backend's own reason rather
    than with an empty result. A CouchDB mount then **re-hand-shakes on a bounded
    backoff until its remote answers, with no process restart** — previously the
    compatibility verdict was decided once per child and an operator whose CouchDB was
    down at startup had to restart the service. A verdict only a config change could
    fix (rejected credentials, a wrong E2EE passphrase) is not retried, because a
    running process cannot re-read its config.
  - **`grep_search` is advertised when ANY mount can serve it**, rather than only when
    the root can. Keying it on the root was defensible only while the root was
    guaranteed to be a local directory. One pre-existing configuration changes: a
    filesystem root on a host with no `rg`, plus a remote mount, now advertises the
    tool — honestly, because an unscoped grep federates and names the mounts it could
    not search in `missingMounts`. A vault with no grep-capable mount still gets no
    `grep_search`.
  - **Packaging and diagnostics.** The sidecar ships in the `.deb`, `doctor`
    reports per-mount status, and readiness degrades by mount name (`503`,
    `degradedMounts`) while the vault root keeps serving. A single fully-remote mount
    gets the additive `mounts[]` report too, so a LiveSync vault with no filesystem
    beside it still has a surface for its capabilities and its unreconciled conflicts.
  - **`mounts add` / `mounts list` / `mounts remove`: a checkable way to build a mount
    table.** Until now a mount table could only be written by hand — `setup-service`
    refuses to rewrite one, because a mount table is the thing in that file it cannot
    reproduce faithfully — so the invariants an editor cannot check (unique ids and
    prefixes, exactly one root mount, an `experimental` flag per remote backend, a
    credential that belongs in the secret store) were left to the operator to get right.

    Every write now goes through the **same config loader the server uses at startup**,
    so the command cannot produce a config the server would then refuse to load. On a
    legacy `vaultPath`-only config, `mounts add` first converts the vault to an explicit
    root mount (`id: "vault"`, `mountAt: ""`, the same path — it resolves to exactly the
    same vault path and index directory) and reports the conversion, then appends. A flag
    is never enabled silently: the command names the `experimental.*` flag, says what it
    turns on and that it may change, and asks; `--yes` answers for a script.

    Credentials are never flag values — a `--password` on the command line lands in `ps`
    output and shell history — so the CouchDB password and the Algolia API key are
    **prompted masked**, or read from stdin with `--password-stdin` / `--api-key-stdin`.
    Only the `SecretRef` reaches the config file. The new mount is then **probed before
    the config is written** (a directory read, one sidecar handshake, or one index read —
    nothing that can mutate anything), and on failure nothing is written **and the
    credential just stored is removed**, so a typo'd URL leaves no orphan in your keyring.
    `--keep-anyway` writes it regardless and says `doctor --probe-remote` will report it
    degraded. A content-changing write leaves the previous file at `config.json.bak` and
    carries across any key this build does not understand.

    `mounts remove` **unmounts**: nothing is ever deleted from a couchdb or algolia
    backing store, the local index survives unless `--purge-index`, and the stored
    credential is always kept and named — a reference can be shared by more than one
    mount, so deleting one silently could break the other. Removing the root mount while
    others exist is refused (they resolve beneath it), and so is removing the last one (a
    config needs a root mount; edit the file directly to start over). `mounts list`
    reports every mount with its writability and the flags currently on, and works on a
    legacy config, where it shows the implicit root as such. All three take `--json` and
    honour the global `--dry-run`.

    Both `list` and `remove` also work on a config the loader **refuses** — a hand-edited
    table with a duplicate prefix, or one whose `experimental` flag was deleted. That is
    deliberate and it is the point: a broken table is exactly when you need to see what is
    declared, and removing a mount is one of the ways to fix one. `list` prints the table
    with a warning and the loader's reason (index directories omitted, since they derive
    from the resolved root); `remove` still validates the table it is about to **write**.

    One behaviour outside the new commands changed with them: a config backup is now named
    after the file's own extension, so a `config.toml` is backed up to `config.toml.bak`
    rather than to `config.json.bak`. The old name held valid TOML under a `.json` name,
    which is a trap, because the obvious way to use a backup is to rename it back. A
    `.json` config still gets exactly `config.json.bak`.
  - **`setup-service --wizard` is a first-init flow, and `mounts add` is guided.** The
    wizard asked four questions, none of which could describe a vault that was not a single
    local folder — so the one setup path a newcomer is told to run could not reach any of
    the above. It now walks six screens: where your notes live (a local folder, or a remote
    LiveSync/CouchDB vault or a shared Algolia index **as the vault root**), any further
    vaults mounted under subfolders, embeddings, transport, the `--mcp`/`--skills`/
    `--vault-snippets` installs, and a recap.

    Each mount goes through the same sequence `mounts add` does, because it is the same
    code: the legacy `vaultPath` is migrated to an explicit root mount, the
    `experimental.*` flag is named and confirmed, the whole table is re-validated through
    the server's own loader on every addition, the credential becomes a `SecretRef`, and
    the mount is **probed before anything is written** — a failed probe stops and asks
    whether to keep it anyway. The id of an additional mount is suggested as a slug of the
    `mountAt` you type (`Team/Alpha` → `team-alpha`) and is editable.

    The last screen prints the config it is about to write through the same renderer
    `print-config` uses, with credentials shown only as references, and asks once; then it
    reports the local `doctor` checks and your next steps. It does not re-probe the remote
    mounts it contacted a few questions earlier. **An interrupted run leaves nothing
    behind**: a probe has to happen after its credential reaches the store, so every later
    exit — a failed probe, a declined recap, `Ctrl-D` at any question — deletes the
    credentials that run stored and writes no config. `--dry-run` walks every screen and
    stores, probes and writes nothing.

    A local root with no further mounts is still written as a plain top-level `vaultPath`,
    so the common case produces exactly the file it always did. The wizard still refuses to
    **edit** a config that already declares a mount table, and now says so before its first
    question: re-deriving one from fresh answers would drop every per-mount setting it never
    asks about (`options`, `cache`, `retention`, `recallWeight`, an explicit `indexDir`).
    `mounts add` / `mounts list` / `mounts remove` are named as the way to change one.

    `mounts add`'s identifying flags are now optional, and an omitted one is a question:
    `mounts add couchdb --url https://couch.example` on a terminal prompts for exactly the
    rest, treating the flags you did give as answers already supplied. With no terminal — a
    script, a pipe, `--yes`, or `--password-stdin`/`--api-key-stdin`, which have already
    claimed stdin for the credential — a missing required flag is a clap-shaped error naming
    every one of them at once, never a prompt that would hang.

    Two smaller behaviours changed with it. `setup-service` now honours an explicit
    `--transport` instead of silently overriding it (without one it still writes an HTTP
    config, which is what the packaged service wants) — that is what lets the wizard offer
    stdio, its default, since a client-launched subprocess is the simplest thing that
    works. And the wizard's embedding API key now falls back to the encrypted secrets file
    automatically, with a message, when the OS keyring is unavailable, instead of asking:
    the only alternative was "no semantic search", and it is the rule `mounts add` already
    applied to a mount's credential.

### Improved

- **A hash conflict on `edit_note` says what to do next.** It named two opaque
  hashes and nothing else, so the only recovery was a full re-read — and that is
  the dominant failure of the section-write path, with `conflict → read_file →
  retry` the one fail-then-retry loop that recurs in practice. The error now
  names the hash to retry with and carries a unified diff of what the same call
  would change against the note as it now stands, which is the diff a caller can
  act on: it answers whether retrying is still the right edit. Three outcomes are
  distinguished — the edits still apply, they no longer do, or a concurrent write
  already produced the intended result and a retry would be a no-op.

- **Multi-backend performance: five measured fixes.** All five came out of a
  release-build audit of the multi-backend stack and each is verified against that
  audit's own harness. No payload changes and no new staleness: every cache here is
  invalidated by a signal, not merely aged out.

  - **A CouchDB mount's listings no longer re-walk the vault.** The manifest cache
    was valid for a fixed two seconds, which interactive traffic misses almost every
    time, so `vault_info`, `resources/list`, `list_children` and `conflicted_paths`
    each paid a full cursor-looped manifest walk — O(notes), measured at **+45.7 ms
    per mount** over 300 notes and growing linearly (112 ms at 1200 notes). The cache
    is now valid **until invalidated**, by a local write (synchronously, before the
    writing caller is answered) or by the change feed. Cold `vault_info` with a
    CouchDB mount: **57.2 ms → 14.1 ms**, and the cost is now **flat** in vault size
    (10.7 / 11.2 / 11.1 ms at 600 / 900 / 1500 notes, against a recorded 39.9 / 82.0 /
    112.4 ms). While the change feed is not running the old two-second window applies
    unchanged — with nothing watching, that conservatism is still correct.
  - **A federated `grep_search` searches its mounts at once.** It cost one whole
    ripgrep run per mount, end to end, `+17.9 ms` for each mount added. The fan-out is
    now concurrent — and the reason it could not already be was that the filesystem
    backend ran `rg` *synchronously inside an async fn*, holding a reactor thread for
    the entire search and making every other request on that thread wait behind it,
    federated or not. Grep over 1 / 2 / 3 mounts: **59.4 / 77.3 / 93.9 ms → 60.0 /
    57.7 / 58.4 ms**. Results are still merged in mount order, so the output is
    byte-identical to before, including `exhaustive: false` when a limit truncates the
    answer with mounts left to search.
  - **`read_file`'s `knownHash` finally saves something on a CouchDB mount.** It
    saved only response bytes (2.64 ms against 2.60 ms unconditional), because the
    note was fully fetched, reassembled and decrypted just to compute the hash. The
    hash now travels to the backend, which keeps a bounded `path → (revision, hash)`
    cache and answers "unchanged" from metadata alone. **2.64 ms → 0.064 ms**, faster
    than reading the same note off local disk. A write records its own hash too, so
    feeding `upsert_note`'s `newHash` straight back costs one metadata call rather
    than a hydration. The filesystem backend ignores the hint and says why: there is
    no way to know what a file hashes to without reading it.
  - **An Algolia mount's listings are validated by a generation sentinel.** A
    whole-corpus `browse` ran on every `resources/list`. One small `meta:generation`
    record is now replaced by every write path, and a listing is reused only while
    that token is unchanged — so an unchanged listing costs one object lookup instead
    of a cursor-followed browse, and a write invalidates it immediately with no wait.
    A mount whose API key cannot read the sentinel browses every time, exactly as
    before.
  - **`note_history` takes a `limit`.** It was O(versions) with no way to ask for
    less. `limit` (default 50, max 500) keeps the newest versions; a shortened answer
    adds `truncated`, `totalCount` and `truncationNote`, and an untruncated one is
    byte-identical to before.

- **Answers say what they could not do.** A federated answer that lost a mount
  carries `degraded`, `missingBackends` and a `degradationReason` naming it; a
  mount that legitimately holds nothing a tool asks for is reported as *skipped*
  and the answer is **not** degraded. A truncated folder listing sets
  `foldersTruncated`; a non-exhaustive grep sets `exhaustive: false`. Every one of
  these fields appears only in the case it describes, so a backend that cannot
  produce that case emits the payloads it always emitted. `docs/behavior-contract.md`
  now has a consolidated **Multi-mount vaults** section stating the rules a client
  can rely on.
- **Resilience is now tested end to end.** A sidecar child killed outright is
  restarted by the next call and an edit made while it was down still arrives, via
  a `changesSince` catch-up. A remote answering `500`s — or dropping sockets —
  fails reads honestly and recovers when it stops, without recycling the child. A
  mount unreadable at startup recovers when its vault appears, and an Algolia mount
  taken down mid-session refuses reads rather than serving its own cache, then
  serves again when the backend returns. Two limits are asserted rather than
  papered over: change cursors are not persisted across a server restart (a rebuilt
  backend replays from the beginning, so it misses nothing), and a CouchDB mount
  whose remote was unreachable at handshake time stays degraded until the child
  re-hand-shakes.
- **The black-box MCP surface is frozen** by a golden-based contract suite, so a
  single-mount vault's `tools/list` and payloads cannot drift while multi-mount
  work lands. Unknown config fields are retained rather than dropped on rewrite.
- **Docker packaging: an image, a compose deployment, and a parity smoke test.** A
  multi-stage `Dockerfile` builds the release binary and the LiveSync sidecar bundle
  from sources into a `node:20-slim` runtime carrying `ripgrep`, `curl` and the
  packaged skills/snippets/assets — nothing else. The install prefix is
  `/opt/deep-obsidian-mcp`, chosen so the binary's own exe-relative probe finds the
  bundle at exactly the path `PACKAGED_BUNDLE_PREFIX` derives, i.e. the same
  arrangement the `.deb` and Homebrew use, with no environment variable pointing at
  it in any of the three.

  `docker-compose.example.yml` is a runnable deployment: CouchDB, a one-shot job that
  applies the settings Self-hosted LiveSync requires (CORS for the Obsidian origins,
  `require_valid_user`, the size ceilings) and finishes single-node cluster setup, and
  the MCP server reading that database **as its vault root**. The settings go through
  CouchDB's `_config` API rather than a mounted `local.ini` because the official
  image's entrypoint chowns everything under `/opt/couchdb` before starting: a
  read-only bind mount makes that fail and the container exits 1 with no log output at
  all.

  Three container behaviours are worth stating, because each is a decision rather
  than a default:

  - **Auth is required.** No `/run/secrets/auth_token` and no explicit
    `DO_INSECURE_NO_AUTH=1` is a refusal to start, with a message naming the missing
    file — before anything binds. The image runs as a non-root user, `/healthz` stays
    open for the `HEALTHCHECK`, and TLS is delegated to a reverse proxy.
  - **Config: both ways, with a stated precedence.** A mounted
    `/etc/deep-obsidian/config.json` WINS and is only validated (through
    `print-config`, i.e. the real loader); otherwise a config is derived from `DO_*`
    variables once, onto the volume, and later boots reuse it. Host and port are the
    documented exception: the container forces them, because a config written on a
    laptop says `127.0.0.1` and a drifting port would silently break the healthcheck.
  - **Secrets never touch the volume.** `XDG_CONFIG_HOME` is inside the container, so
    the encrypted store dies with it; the entrypoint deletes it and re-injects every
    secret from `/run/secrets` on every boot. The volume holds an index and a config
    and no credential, which is also what makes the file store's derived-key weakness
    irrelevant in this deployment.

  A CI `docker` job builds natively on `ubuntu-24.04` and `ubuntu-24.04-arm` (a
  matrix, not a QEMU cross-build, so each leg can *run* what it built) and executes
  `scripts/docker-smoke-test.sh`: the same assertions `scripts/linux-smoke-test.sh`
  makes about the `.deb` — including `doctor` run from `/` locating the packaged
  bundle — plus the entrypoint contract and a live-CouchDB proof that a remote root
  starts degraded on a database no client has synced and readies itself, with no
  restart, once the LiveSync documents appear. Nothing is published: the GHCR push
  and its tag policy are present but commented out. See
  [docs/docker.md](./docs/docker.md).

- **Homebrew now ships the LiveSync sidecar bundle.** The release attaches a prebuilt,
  checksummed `livesync-sidecar-<version>.mjs` (a new job in
  `.github/workflows/release-deb.yml`), and the formula fetches it as a `resource` into
  the one exe-relative path the binary probes for — the same layout the `.deb` and the
  container image use. A couchdb mount on a `brew` install therefore needs Node 20+ on
  the PATH and nothing else; building the bundle by hand is now only for source
  installs. Every other mount kind still needs no Node at all.

### Fixed

- **Secrets stored in the OS keyring actually persisted.** `keyring` was declared
  without its platform features, so every `put()` landed in the crate's in-memory
  mock store, reported success, and evaporated with the process — and the
  encrypted-file fallback never engaged, because the mock returns `Ok`. HTTP auth
  tokens and embedding API keys were affected. Platform backends are now enabled
  (with a vendored dbus, so the `.deb` gains no runtime dependency).
- **`setup-service --wizard` no longer replaces a config with no way back.** The
  auth prompt always marked the config changed, bypassing the overwrite guard, and
  its hardcoded default silently turned auth off on Enter. Content-changing
  overwrites now back the previous file up to `config.json.bak`, and every prompt is
  prefilled from the existing config.
- **`grep_search` no longer reports an in-vault index directory's SQLite files as
  notes.** A custom `indexDir` inside the vault surfaced as phantom matches under
  caller-supplied globs. They are now excluded before the match limit, so a phantom
  can never displace a real note.

## v0.1.0-alpha.12 — 2026-07-02

### Improved

- **Vault IO errors now carry the offending path and permission remediation
  hints**, and the server warns when the startup scan stalls (#34).

### Fixed

- **The `.deb` installs again on Debian 12 and Ubuntu 22.04.** The alpha.11
  package was built on Ubuntu 24.04 runners, so it declared
  `libc6 (>= 2.39)` and `apt install` failed with unmet dependencies on every
  older distro. Release builds now run inside a Debian bookworm container
  (floor drops to `libc6 (>= 2.34)`), CI fails if the floor ever rises above
  2.35, and a local Docker harness
  (`scripts/run-linux-integration-docker.sh`) reproduces the full
  install + smoke test on `debian:12`, `ubuntu:24.04`, and `ubuntu:22.04`.
- **`upsert_note` no longer fails clients that send both `content` and
  `body`.** Some tool-callers (e.g. Grok) fill every schema property on each
  call, and the two fields looked interchangeable, so every call died on the
  server's mutual-exclusion check. Identical text is now accepted (writing
  `content`, with a `warning` in the result); diverging text still errors. The
  tool description, the `content`/`body`/`title`/`frontmatter` descriptions,
  and a new `oneOf` in the input schema now state the exclusivity explicitly.
- **A failed tool call no longer kills the stdio server.** The stdio loop
  treated any request-level error (failed tool call, bad params, unknown
  method) as fatal and exited, so the first error made every subsequent call
  fail until restart. Errors are now sent to the client as JSON-RPC error
  responses and the server keeps serving, matching the HTTP transport.

## v0.1.0-alpha.11 — 2026-06-26

### Added

- **Debian/Ubuntu packaging (`apt`), amd64 + arm64.** The server now installs
  via `apt` alongside the Homebrew tap. Add the signed APT repository hosted on
  GitHub Pages and `apt install deep-obsidian-mcp`, or grab a single `.deb` from
  the release. It installs the binary to `/usr/bin`, packaged
  skills/snippets/assets to `/usr/share/deep-obsidian-mcp/`, and a systemd
  **user** unit to `/usr/lib/systemd/user/`
  (`systemctl --user enable --now deep-obsidian-mcp`). Built with `cargo-deb`
  (`scripts/build-deb.sh`); a `release-deb` GitHub Actions workflow builds both
  architectures natively, smoke-tests each package, validates the signed repo by
  installing from it, and on tags publishes the repo to Pages and attaches the
  `.deb`s to the release. See [docs/debian-package.md](./docs/debian-package.md).
- **Optional HTTP bearer authentication** for the HTTP transport (off by
  default). Enable via `setup-service --wizard` or `setup-service --auth`
  (generates, stores, and prints a token once); disable with
  `setup-service --no-auth`. `DEEP_OBSIDIAN_AUTH_TOKEN` provides a literal-token
  override. Protected routes (`/mcp`, `/upload`) require the token; health stays
  open. Includes Origin validation and a fail-closed guard that refuses to bind a
  non-loopback host without auth (`--insecure-no-auth` to override).

### Changed

- **Packaged index location is now platform-native.** On Linux, packaged-mode
  indexes live under `$XDG_DATA_HOME/deep-obsidian-mcp/indexes/` (default
  `~/.local/share/...`) instead of the macOS-only `Application Support` path.

## v0.1.0-alpha.10 — 2026-06-12

### ⚠️ Breaking changes (MCP tool surface: 19 → 18 tools)

- **Removed `find_similar_notes`.** The editorial style/structure/tone/format
  similarity tool had no internal callers and overlapped conceptually with
  `related_notes` (subject similarity). For content-relevant neighbours use
  `related_notes` (by note path) or `hybrid_search` (by query).

## v0.1.0-alpha.9

A large release centred on a retrieval-pipeline overhaul, a security pass, and a
tighter MCP tool surface. Existing indexes rebuild automatically on first run.

### ⚠️ Breaking changes (MCP tool surface: 24 → 19 tools)

Six tools were removed or merged. Migrate as follows:

| Removed tool | Use instead |
|---|---|
| `write_file_to_vault` | `request_vault_upload` (binary/large) · `upsert_note` (markdown) |
| `bm25_search` | `hybrid_search` with `semanticWeight: 0` |
| `semantic_search` | `hybrid_search` with `bm25Weight: 0` |
| `list_folders` | `list_children` with `foldersOnly: true` |
| `backlinks` | `graph_traverse` with `direction: "incoming", depth: 1` |
| `read_chunk` | `read_file` with `startLine` / `endLine` |

- **New tools:** `request_vault_upload` (capability-token binary upload) and
  `search_artifacts` (semantic search over non-markdown artifacts).
- Artifact-scope semantic search (formerly `semantic_search` `scope:"artifacts"`)
  is now exposed via the dedicated `search_artifacts` tool.

### Security (issue #22 — resolved)

- **Fixed a verified RCE** in `grep_search`: a query beginning with `-`/`--`
  (e.g. `--pre=…`) was parsed by ripgrep as a flag, enabling arbitrary command
  execution. Queries and paths are now passed after a `--` end-of-options guard.
- **Fixed symlink vault-escape**: `ensure_inside_vault` now canonicalizes and
  verifies the target stays under the vault root (handles not-yet-existing write
  targets and symlinked vault roots).
- Upload-store lock sites recover from mutex poisoning instead of propagating a panic.

### Retrieval pipeline overhaul (issue #6 — resolved)

- **Heading-aware chunking** — section-based chunks that never split fenced code or
  tables; embedding text carries the heading path.
- **Reciprocal Rank Fusion** for hybrid search (scale-free, replaces weighted-sum).
- **Asymmetric query encoding** for instruction-tuned embedding models (qwen3).
- **Small-to-big retrieval** — match at chunk granularity, return the enclosing section.
- **Note-level dense vector dropped**; **`related_notes` reimplemented as late-interaction
  (max-sim)** over chunk vectors (semantic, no stored note vector).
- **Graph-aware re-rank** — lightly boost candidates one wikilink hop from top hits.
- **Deterministic retrieval-quality eval harness** + manual real-model protocol.

### Reliability

- **Embedding reindex robustness** — request timeout, per-batch partial progress
  (one failed note no longer aborts the reindex), partial-index load, `Sparse`
  downgrade + auto-recovery on total failure.
- **Query-time graceful degradation (#4-#3)** — `hybrid_search`/`load_knowledge`
  fall back to BM25 with a `degraded` flag when the embedding backend is down;
  `search_artifacts` returns a clear message; `vault_info` reports backend
  reachability non-fatally.
- **`grep_search` rg-or-disabled (#5 — resolved)** — resolve ripgrep at startup;
  hide the tool when unavailable; never the misleading `No such file (os error 2)`.

### Agent ergonomics (issue #4)

- `request_vault_upload` — binary files via an out-of-band capability-token URL (#4-#0).
- `read_file` conditional reads via `knownHash` (skip unchanged bodies) (#4-#2).
- Aggregate output caps with truncation-with-continuation on search tools (#4-#6).
- Descriptive required-argument errors + conditional `heading` schema for
  `update_note_section` (#4-#5).

### Migration notes

- **Index auto-migration:** the index schema version was bumped (v4 → v6); existing
  indexes fail the version check and rebuild automatically on first run — no manual
  action required.
- **`vault_info`** now performs a bounded (~3s) embedding-backend reachability probe
  on an embedding backend (previously pure-local), reported as `embeddingBackendStatus`.

### Internal

- Dead-code audit removing stale `#[allow(dead_code)]` and an unused chunker parameter.

### Known / not in this release

- Open enhancements (non-blocking): `update_note_section` batch edits / section-scoped
  hashing (#4-#4) and basename/fuzzy path resolution (#4-#5 basename).
- Out of scope: automatic restart of the external embedding (llama-server) process.
