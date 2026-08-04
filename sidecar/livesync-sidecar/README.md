# `@deep-obsidian/livesync-sidecar`

A Node process that exposes a [Self-hosted LiveSync](https://github.com/vrtmrz/obsidian-livesync)
CouchDB vault to the `deep-obsidian` MCP server over a versioned JSON-RPC-over-stdio
protocol.

**Read-only by default.** Writing is opt-in per process via
`initialize.mode: "read-write"`, and even then only entry and chunk documents are
ever written — see [Writing](#writing) and [Security posture](#security-posture).

## Why a sidecar and not Rust

LiveSync's CouchDB format is not a spec, it is an implementation: content-defined
chunking, chunk id derivation, HKDF-based end-to-end encryption, path
obfuscation, soft-delete tombstones, Eden inline chunks, and milestone gating.
All of it moves with the community plugin. Reimplementing it in Rust would mean
tracking a moving target with no compatibility contract, and being wrong would
mean silently serving corrupt or stale note content.

So the sidecar wraps the upstream library (`@vrtmrz/livesync-commonlib`) and the
Rust server supervises it as a child process. The cost is a Node runtime
dependency for this one backend; the benefit is that format changes are an
upstream bump plus a fixture regeneration, not a reverse-engineering project.

## Protocol

Newline-delimited JSON-RPC 2.0 over stdin/stdout, UTF-8, one message per line.
Requests carry an `id`; the sidecar's unsolicited `change` messages are
notifications with no `id`. **stdout carries protocol frames and nothing else**;
all logging goes to stderr.

`PROTOCOL_VERSION` is `1`. `src/protocol.ts` is the authoritative definition — the
summary below is for orientation.

### Methods

| Method | Purpose |
| --- | --- |
| `initialize` | Handshake, credentials, mode, compatibility gate. The only method that receives secrets. |
| `manifest` | Paginated vault metadata listing. |
| `read` | Full content of one entry, assembled from its chunks. |
| `stat` | Metadata for one entry, no chunk fetch. |
| `conflicts` | The entry's sibling conflict revisions. Read-only, available in both modes. |
| `changesSince` | One-shot change feed from an opaque cursor. |
| `watch` / `unwatch` | Subscribe to / unsubscribe from live `change` notifications. |
| `write` | Compare-and-swap write of one entry. **`read-write` mode only.** |
| `delete` | Soft delete of one entry. **`read-write` mode only.** |
| `health` | Liveness, compatibility status, mode, watch state, last error. |
| `shutdown` | Graceful exit 0. |

`initialize` must come first. Every data method **fails closed** until
`initialize` has returned `compatibility.status: "ok"`. `health` and `shutdown`
are always available — the supervisor needs them exactly when something is wrong.

### Versioning rule: additive methods do not bump the version

`PROTOCOL_VERSION` describes what a host must *tolerate*. Adding a method cannot
break a v1 host: a host that does not know `write` never sends it, and one that
does gets `method-not-found` from an older sidecar, which is already a modelled
failure. Adding an optional request field with a backwards-compatible default
(`initialize.mode`, which defaults to `"read-only"`, i.e. exactly v1 behaviour) is
invisible to a host that omits it, and neither side deserialises strictly, so new
response fields are safe too.

So the whole write surface is **additive on protocol version 1**. What *would*
require a bump: changing `SUPPORTED`'s shape, changing an existing method's params
or result, changing an error code's meaning, or flipping `mode`'s default.

**Never edit `SUPPORTED` to advertise a capability.** The Rust supervisor asserts
that object field by field on every handshake, so a new key there is a hard
version mismatch, not a feature flag.

### `initialize`

```json
{
  "jsonrpc": "2.0", "id": 1, "method": "initialize",
  "params": {
    "protocolVersion": 1,
    "mode": "read-only",
    "couchdb": { "url": "https://couch.example", "database": "vault",
                 "username": "user", "password": "..." },
    "e2ee": { "passphrase": "...", "obfuscatePassphrase": "..." },
    "options": { "requestTimeoutMs": 30000 }
  }
}
```

The result echoes the pinning triple and reports what the remote is:

```json
{
  "protocolVersion": 1,
  "mode": "read-only",
  "sidecarVersion": "0.1.0",
  "commonlibVersion": "0.1.2",
  "supportedSchemaVersion": 12,
  "supported": { "protocolVersion": 1, "commonlibVersion": "0.1.2",
                 "maxSchemaVersion": 12, "pluginVersionTested": "1.0.3" },
  "compatibility": { "status": "ok" },
  "remote": { "schemaVersion": 12, "encrypted": false, "pathObfuscation": false }
}
```

A wrong `protocolVersion` is a JSON-RPC error (`-32001`) and the process **stays
alive**, so the supervisor can report a clean version mismatch instead of an EOF.

### Compatibility status

A remote-side problem is never a JSON-RPC error on `initialize`: the call
succeeds and reports a status, so the supervisor has exactly one precise reason
to show a user. Data methods then fail with `incompatible-remote` (`-32003`)
carrying the same status.

| Status | Meaning |
| --- | --- |
| `ok` | Serveable. |
| `unknown-schema` | `obsydian_livesync_version` missing, malformed, or newer than 12. |
| `locked` | Milestone `locked`: a rebuild/cleanup is in progress. |
| `cleaned` | Milestone `locked` + `cleaned`: chunks were purged; clients must resync. |
| `incompatible` | Accepted nodes agree on no chunk format version this client can read. |
| `mismatched` | Remote's preferred tweak values conflict with the supplied options. |
| `auth-failed` | CouchDB answered 401/403. |
| `unreachable` | DNS/refused/timeout/TLS/5xx. |
| `e2ee-required` | Encrypted chunks or obfuscated ids present, passphrase missing. |
| `e2ee-invalid` | Passphrase supplied but unusable (see below). In `read-write` mode, also reported when a passphrase is supplied and the remote has no replication salt for an encrypted write to use. |
| `unknown` | Unclassified. |

### Entry shapes

`kind` is `"markdown"` (stored as text — upstream's `plain`/`notes` types, which
the plugin assigns to anything it judged textual, not only `.md`), `"binary"`
(upstream's `newnote`, base64 chunks), or `"internal"` (`i:`-prefixed
hidden-file entries).

`manifest` yields `{path, size, mtimeMs, ctimeMs, deleted, conflicted, kind}`
plus `nextCursor` / `exhausted`. `read` yields
`{kind: "text", text, ...}` or `{kind: "binary", base64, ...}`, both with
`{path, size, mtimeMs, ctimeMs, deleted, conflicted, rev}`.

- **Soft deletes** are LiveSync's default: the entry document carries
  `deleted: true` and is still listed and readable, with `deleted: true` set.
  It is not a CouchDB tombstone.
- **Conflicts**: the winning revision is served and `conflicted: true` is set. The
  losing revisions are enumerable with [`conflicts`](#conflicts).
- **Cursors** (`manifest`, `changesSince`, `change.cursor`) are **opaque**. Pass
  them back verbatim; do not parse, compare, or persist assumptions about them.
- **An empty page does not mean the end.** Both `manifest` and `changesSince` can
  return zero items with `exhausted: false`, because the page's budget was spent
  on documents that get filtered out — and a real vault's change feed is mostly
  `leaf` chunk documents. Drive the loop on `exhausted`, never on
  `changes.length` / `entries.length`.

### What the manifest excludes

Upstream enumerates five id ranges and the gaps between them are the exclusions.
The sidecar copies those ranges verbatim, so it excludes:

- `h:` — content chunks (`h:+` when encrypted)
- `i:` — internal / hidden files (`.obsidian/**` and similar)
- `ix:` — internal-file index documents
- `ps:` — plugin-sync settings

It additionally hides paths containing `:` and paths beginning with `.`, which
mirrors what commonlib's `isTargetFile` refuses on read. Without that, `manifest`
would advertise entries `read` then reports as missing. The same rule gates
`write` and `delete`, which refuse such paths with `invalid-params`.

## Writing

`write` and `delete` exist only when `initialize` was given
`mode: "read-write"`. Otherwise they refuse with `read-only` (`-32009`) before any
request reaches the remote. Writes are additionally refused whenever the
compatibility gate reported anything other than `ok`: a vault the sidecar cannot
fully classify must not be written to, however the host asked.

### Compare-and-swap

`baseRev` selects the CAS mode, and the three cases are distinct JSON values
rather than a flag:

| `baseRev` | Meaning | Failure |
| --- | --- | --- |
| `null` | **create-only** | `conflict` if any document exists at the path, *including a soft-deleted one* |
| `"<rev>"` | **guarded update** | `conflict` unless the remote's winning revision is exactly this |
| absent | **unguarded upsert** | `conflict` only if another writer landed between this one's read and its write |

The last row is not a contradiction: "unguarded" means the *caller* states no
precondition, but the write itself is still revision-guarded against the revision
observed a moment earlier, so it can never create a conflict branch. Losing that
narrow race is reported with `currentRev`, and retrying is a one-liner.

`conflict` (`-32008`) carries `error.data.conflict`:

```json
{ "currentRev": "3-abc…", "expected": "2-def…",
  "deleted": false, "conflicted": false, "mtimeMs": 1700000000000, "size": 4096 }
```

`currentRev` is absent only when the document does not exist at all.
`deleted: true` on a create-only refusal is the signal that the path *looks* free
but a soft-deleted entry still occupies it, so the right move is a resurrect
rather than a create.

`delete` takes the same `baseRev` guard, except that `null` and absent both mean
unguarded — create-only has no meaning for a delete. A path with no document at
all is `not-found`, never a silent success.

`write` returns `{path, rev, conflicted, size, mtimeMs, ctimeMs, kind, created,
resurrected}`. `conflicted` reports a *pre-existing* conflict: a rev-guarded write
extends the winning branch only, so it neither creates nor resolves conflict
branches. That is asserted against a real CouchDB with a real replication-style
sibling revision, not only against the mock — grafting a sibling needs
`new_edits: false`, which is precisely the request shape the mock refuses to
model.

**Resurrection is structural, not a special case.** Upstream's `putDBEntry`
rebuilds the entry root from scratch, so the `deleted` flag is simply not carried
over — any successful write over a soft-deleted entry brings it back, and
`resurrected: true` reports that it happened.

**Writing over a hard CouchDB tombstone (`_deleted: true`) fails with
`conflict`.** So does upstream's own `put`, for the same reason: a rev-less PUT
against a deleted document is a 409 in CouchDB, and neither upstream nor this
sidecar goes looking for the tombstone's revision. It does not arise in practice
because LiveSync's delete is soft unless the user turns on
`deleteMetadataOfDeletedFiles` (off by default).

#### Why the CAS is implemented around upstream rather than by it

`DirectFileManipulator.put` cannot express CAS and cannot be made to. Upstream's
`putDBEntry` writes the entry root with `localDatabase.put(doc, {force: true})`,
and PouchDB turns `force` into `new_edits: false` plus a *fabricated* child
revision — so a stale base revision never produces a 409, it silently grafts a
second leaf onto the revision tree. That is correct for a replicating peer and
wrong for a tool whose contract is `expectedHash`. Its `conflictBaseRev` parameter
only chooses which revision the forced write chains from.

So `write` keeps all of `putDBEntry` — target-file filtering, blob typing, the
splitter, chunk batching, Eden, chunk id derivation, the encryption transform —
and replaces exactly one operation: the final root `put`, re-issued without
`force` under the revision the precondition validated, so CouchDB adjudicates. The
interceptor asserts it fired exactly once; if upstream ever stops routing the root
write through `localDatabase.put`, the write fails loudly with `internal-error`
instead of silently reverting to force-write semantics.

`delete` is likewise re-implemented (read, set `deleted: true`, bump `mtime`, put
under a revision guard) because `deleteDBEntryByPath` force-puts too.

### Publication order, and what an interrupted write leaves behind

Chunks first, entry root **last** — upstream's order, and the one the plugin
relies on. Every leaf goes out in `POST /{db}/_bulk_docs` before the root's
`PUT /{db}/{id}`.

The invariant that matters: **an interrupted write can leave orphan chunks, never
a root pointing at chunks that do not exist.** A dangling root would be
unreadable and unrepairable; an orphan chunk is inert.

### Retry safety

The sidecar performs no retries of its own — retry policy belongs to the Rust
supervisor, which knows about backoff and user intent. What the sidecar guarantees
is that retrying is *safe*, and it is deterministic in both directions:

- **Chunks are content-addressed** (hash of content, salted by the passphrase when
  E2EE is on), so re-publishing identical content re-derives identical ids. The
  duplicate writes come back 409 and upstream's own write layer counts them as
  "duplicated". Republishing is idempotent.
- **A retry with the same `baseRev` either succeeds or fails `conflict`, never
  double-writes.** If the first attempt died before the root, the retry succeeds.
  If the first attempt's root landed but the response was lost, the retry loses
  the CAS and comes back with `currentRev` — which is exactly the information
  needed to decide whether the lost write was in fact the caller's own.

### Orphan chunks: deliberately not collected

Nothing in this sidecar reclaims orphan chunks, and `health` deliberately has no
orphan counter. Two reasons.

It is not cheaply knowable. A chunk is orphaned only relative to *every* entry's
`children` list, including every conflict revision, so "did my last write orphan
anything" needs a full-database refcount — not a per-write fact.

And it is upstream's own stance. The plugin's maintenance command computes exactly
that refcount and *reports* orphans as an `__orphan` row in a CSV dump
(`CmdLocalDatabaseMainte.ts`), but its "Remove all orphaned chunks" UI is
**commented out** under the heading "Garbage Collection (Old and Experimental)"
(`PaneMaintenance.ts`). Upstream's real reclamation path is a rebuild — which is
what the `locked` / `locked+cleaned` milestone states signal, and which this
sidecar already refuses to read through. A sidecar-side GC would be a destructive
operation with no upstream contract behind it; deferred to a later slice, if ever.

Orphans arise from ordinary editing anyway, not only from failed writes: every
edit that changes a chunk's content leaves the old chunk referenced by nothing
once the previous revision is compacted away.

### `conflicts`

`conflicts {path}` → `{path, winning, conflicts: [{rev, mtimeMs, size, deleted}]}`.

Read-only, and therefore available in **both** modes — refusing it on a read-only
mount would hide exactly the information a read-only host most needs. A revision
CouchDB has already compacted away is reported with `unavailable: true` rather
than dropped, so a host never silently under-reports a conflict.

Resolution — picking a winner, deleting the losers — is deliberately not here. It
is destructive and needs a merge policy the sidecar has no business choosing.

### E2EE and obfuscated writes

Both come free from routing the root write through the *transformed* PouchDB
handle: commonlib installs `transform-pouch` with an `incoming` hook that encrypts
and an `outgoing` hook that decrypts, so the CAS interceptor sits above the
transform and never sees or bypasses it. Issuing raw HTTP instead would write
plaintext into an encrypted vault.

- **Chunks**: with a passphrase configured, chunk ids gain the `+` marker (`h:+…`)
  and each chunk is stored as `{e_: true, data: "%=<HKDF ciphertext>"}`.
- **Obfuscated paths**: the entry id becomes `f:<hash>` and the *metadata* is
  encrypted too — the stored document's `path` is a `/\:`-prefixed envelope and
  `mtime`, `ctime`, `size` are zeroed with the real values inside it, `children`
  emptied. Reading restores all of it.
- **An encrypting writer needs the replication salt to already exist.** The
  key is derived from passphrase + the `pbkdf2salt` in
  `_local/obsidian_livesync_sync_parameters`, and the sidecar refuses to create
  that document in *either* mode. A read-write handshake with a passphrase and no
  salt is refused up front with `e2ee-invalid` rather than failing deep inside the
  first write. A read-only handshake against the same vault is unaffected.
- **Obfuscation is all-or-nothing.** `path2id` obfuscates unconditionally once the
  passphrase is set, so a client with obfuscation on can only address `f:` entries
  by path and one with it off can only address plaintext ids. That matches the
  plugin, which requires a rebuild to switch modes.

This is also what finally proves the E2EE **success** path — see
[Tests](#tests).

## Version pinning policy

```ts
SUPPORTED = {
  protocolVersion: 1,
  commonlibVersion: "0.1.2",
  maxSchemaVersion: 12,
  pluginVersionTested: "1.0.3",
}
```

The Rust supervisor enforces this triple. `commonlibVersion` is **exact-pinned**
(no caret) and frozen by `package-lock.json`: upstream is pre-1.0 and documents
its own semantics as "not final", so a minor bump is a potential behaviour
change, not a patch. `test/upstream-constants.test.mjs` asserts the installed
version matches what `SUPPORTED` advertises, so a lockfile drift fails CI.

### Upgrade procedure

1. Bump the exact version in `package.json`, run `npm install`, commit the lockfile.
2. Run `npm run typecheck && npm run build && npm test`. `test/upstream-constants.test.mjs`
   will fail if any restated control-document id, id prefix, or `VER` moved.
3. If `VER` moved, raise `maxSchemaVersion` **only** after checking the new
   remote format against a real vault — a schema bump means the stored data
   changed shape.
4. Regenerate fixtures against the real plugin (see below) rather than
   hand-editing them to make tests pass. A fixture edited to satisfy a test no
   longer proves anything about the format.
5. Review `src/manipulator.ts` against the new upstream source, specifically the
   drift notes in its header — each one is a place where upstream's public API
   was insufficient and might now have changed.

`manipulator.ts` is the only file that imports commonlib. That is the blast
radius, on purpose.

## Security posture

- **Secrets arrive only via `initialize`.** Never argv (world-readable in `ps`),
  never the environment (inherited by children, captured by crash reporters).
- **stderr is redacted.** The CouchDB password, both passphrases, and the URL are
  registered as secret literals and masked by value, so it does not matter which
  code path leaks them. URL userinfo is masked by pattern even when never
  registered. `error.message` and `error.data.detail` returned over the protocol
  go through the same redaction, since a host may log them verbatim.
- **With path obfuscation enabled, paths are also suppressed from stderr.** The
  point of that mode is that the server never sees plaintext paths; the log must
  not either.
- **Read-only unless asked, structurally.** `put` is refused unless the process
  was initialized `mode: "read-write"` *and* the compatibility gate said `ok`;
  commonlib's `delete` is refused unconditionally (it force-writes, so `delete` is
  re-implemented instead); `putSyncParameters` is refused **in both modes**.
  `putSyncParameters` matters most: upstream's `getReplicationPBKDF2Salt` will
  *create* the remote's `_local/obsidian_livesync_sync_parameters` document when
  it is missing, and it is wired into the encryption *and* decryption paths as a
  lazy callback — so an E2EE operation could otherwise write to someone's vault
  mid-read. The sidecar checks for the salt up front and reports `e2ee-invalid`
  instead.
- **The milestone document is never written, in either mode.** Upstream's
  `ensureDatabaseIsCompatible` registers the calling node and updates
  `last_connected`/tweak values as a side effect of *checking* compatibility. The
  sidecar reimplements the checks read-only: a writer client is still not a
  LiveSync peer and must never appear in `accepted_nodes`.
- **The version document is never written, in either mode.** Upstream's
  `checkRemoteVersion` falls through to `bumpRemoteVersion` on a miss, which PUTs
  `obsydian_livesync_version`.
- **No CouchDB tombstones, no compaction, no purge.** Deletion is soft; revision
  history is never destroyed.
- Asserted at the transport, not by inspection. The mock CouchDB records every
  mutating request plus every document write it applies (`{method, id, type}`).
  `test/vault.test.mjs` requires the ledger to be **empty** for a read-only
  sidecar; `test/write.test.mjs` requires that in read-write mode it contains
  **only** `leaf`, `plain` and `newnote` documents and no `_local/` or
  `obsydian_livesync_version` id. The live CouchDB test re-checks the same thing
  against a real server by diffing `_all_docs`.

## How the Rust server runs this

The supervisor lives in `rust/crates/deep-obsidian-backend/src/sidecar.rs`. It spawns
`node <bundle.mjs>` — argv is *exactly* those two elements and nothing is ever
appended, which is what makes the "no secret in argv" assertion checkable rather than
aspirational.

### Configuring a mount

A CouchDB vault is a `couchdb` entry in the config's `mounts` table:

```json
{
  "mounts": [
    { "id": "vault", "mountAt": "", "backend": {
        "kind": "filesystem", "vaultPath": "~/Vault" } },
    { "id": "live", "mountAt": "LiveSync", "backend": {
        "kind": "couchdb",
        "url": "https://couch.example",
        "database": "vault",
        "username": "vaultuser",
        "passwordRef": { "kind": "encryptedFile", "id": "livesync-password" },
        "e2ee": { "passphraseRef": { "kind": "osKeyring",
                                     "service": "deep-obsidian-mcp",
                                     "account": "livesync-e2ee" } },
        "options": { "requestTimeoutMs": 45000 } } }
  ],
  "experimental": { "multiVault": true, "couchdbVaults": true }
}
```

Constraints the config layer enforces, each with a user-facing error:

- **Both experimental flags are required.** `couchdbVaults` gates the backend;
  `multiVault` is tripped as well because a couchdb mount is always a second mount.
  The `couchdbVaults` error wins when neither is set, since it names the flag for the
  feature actually being used.
- **Non-root only.** `ResolvedServiceConfig.vault_path` is the root mount's local
  directory and still feeds `doctor` and the packaged index-dir derivation; a CouchDB
  vault has no local directory, so it cannot be the root mount.
- **Secrets are references, never literals.** There is no plaintext `password` field
  at all — mirroring `embedding.apiKeyRef` and `auth.tokenRef`. That is what makes the
  CLI's `redact_config` an identity function: the persisted config has nothing secret
  in it to strip.
- **`url` must not carry userinfo.** `https://user:pw@host` is rejected, because the
  url is printed verbatim by `doctor` and `print-config`.

### Locating the bundle (what packaging must honour)

In precedence order:

1. the mount's `sidecarPath`;
2. `DEEP_OBSIDIAN_LIVESYNC_SIDECAR`;
3. `sidecar/livesync-sidecar/dist/sidecar.mjs`, searched next to the running
   executable and up its ancestors, then from the working directory and up.

A packaging slice therefore has exactly two options: keep that relative layout under
the same prefix as the binary, or set `DEEP_OBSIDIAN_LIVESYNC_SIDECAR` in the service
unit. Both are checked before the child is spawned, so a missing bundle is a clear
configuration error rather than an EOF on a pipe. `DEEP_OBSIDIAN_NODE` overrides the
`node` executable for hosts where it is not on `PATH`.

### Supervision

Construction does no IO, so a mount whose CouchDB is down still builds and is reported
as degraded instead of taking the server down. The handshake runs on first use: it
sends `initialize`, asserts the echoed `supported` triple **exactly**, and records the
compatibility verdict. A non-`ok` verdict leaves the child alive (so `health` still
answers) and fails every data method with that status — which is what degrades the one
mount while the vault root keeps serving. If the child exits, the next call restarts it
with bounded exponential backoff (capped at 30s), replays `changesSince` from the last
opaque cursor, then re-arms `watch`; the catch-up is what stops a restart from silently
dropping edits made while it was down.

**The supervisor currently sends no `mode`, so every supervised sidecar is
read-only.** Nothing in the Rust crates calls `write`, `delete` or `conflicts` yet;
exposing them through the MCP `upsert_note` family is the next slice. Until then
the write plane is reachable only by driving the bundle directly, and a CouchDB
mount is read-only end to end.

## Tests

```sh
npm ci
npm run typecheck
npm run build      # required: the suite drives dist/sidecar.mjs
npm test
```

| File | Covers |
| --- | --- |
| `test/protocol.test.mjs` | Framing, handshake, malformed input, shutdown. |
| `test/compat.test.mjs` | The compatibility gate, status by status. |
| `test/vault.test.mjs` | Read plane, and the read-only ledger assertion. |
| `test/write.test.mjs` | CAS matrix, soft delete, chunk order, retry safety, concurrent writers, mode gating. |
| `test/e2ee.test.mjs` | Real encrypt/decrypt round trips, obfuscation, and the committed fixture regression. |
| `test/redaction.test.mjs` | Secrets and paths never reach stderr. |
| `test/upstream-constants.test.mjs` | Restated upstream ids/prefixes still match the installed library. |
| `test/live-couch.test.mjs` | Opt-in, against a real CouchDB. Skipped without `DEEP_OBSIDIAN_COUCHDB_URL`. |

### Committed E2EE fixtures

`npm run fixtures:e2ee` regenerates:

- `test/fixtures/e2ee-written-vault.json` — encrypted chunks under plaintext ids
- `test/fixtures/e2ee-obfuscated-vault.json` — encrypted chunks under `f:` ids

These are the closest thing to plugin-generated fixtures obtainable without
installing Obsidian: the ciphertext, chunk ids, encrypted metadata envelope and
obfuscated document ids are all produced by `octagonal-wheels` and
`livesync-commonlib` through the real sidecar. Only the *database* is a mock — and
CouchDB does not participate in the codec, it stores what it is given.

Both shapes of proof are needed, and they are different:

- a **round trip** (write, then read back through a second sidecar process; a third
  with the wrong passphrase must fail) proves the encrypt and decrypt halves agree;
- the **committed fixtures**, read back by a fresh sidecar with no writing
  involved, prove the codec has not *drifted*. A round trip alone would happily
  write and read a new, mutually consistent, wrong format after a commonlib bump.

Regenerate only on a deliberate commonlib bump, and treat the diff as the signal to
review `maxSchemaVersion`. **Never hand-edit them**: a fixture edited to make a
test pass proves nothing about the format. The passphrases and the `pbkdf2salt` are
part of the data — HKDF derives the content key from passphrase + salt — so they
live in `test/e2ee-fixture.mjs` and changing either without regenerating makes the
dumps unreadable.

### Against a real CouchDB

```sh
docker run -d --name couch -p 5984:5984 \
  -e COUCHDB_USER=admin -e COUCHDB_PASSWORD=pw couchdb:3
curl -X PUT http://admin:pw@127.0.0.1:5984/livevault
DEEP_OBSIDIAN_COUCHDB_URL=http://127.0.0.1:5984 \
DEEP_OBSIDIAN_COUCHDB_DB=livevault \
DEEP_OBSIDIAN_COUCHDB_USER=admin \
DEEP_OBSIDIAN_COUCHDB_PASSWORD=pw npm test
```

Skipped, not failed, when the variable is absent: the hermetic suite is the
contract and CI must not require a container. Two things only a real server can
prove — that a database the sidecar cannot classify is refused for writing even in
`read-write` mode with valid credentials, and that `MockCouch`'s 409 semantics are
CouchDB's, by replaying the CAS matrix against the real conflict adjudicator. The
write test creates and drops its own scratch database and never touches the one
named in the environment; point it at a throwaway container regardless.

`npm run mock-couch` starts `test/mock-couch.mjs` over a real socket
(`test/mock-couch-server.mjs`), so a non-Node parent can drive the same fixture vaults.
The Rust suite `deep-obsidian-backend/tests/couchdb_sidecar.rs` uses it to run the real
bundle against the real fixtures; reimplementing the emulator in Rust was rejected
because its endpoint set was discovered empirically against upstream's request shapes
and a second copy would drift from it silently.

The suite spawns the **built bundle as a child process** and speaks the real
protocol over real pipes. That is deliberate: it exercises the newline framing,
the stdout/stderr split, and the shutdown path, none of which an in-process call
would check. It also means nothing can be injected into the sidecar, which is why
`test/mock-couch.mjs` is a real HTTP server emulating CouchDB rather than a fake
`fetch`.

The mock's endpoint set was discovered empirically (`DEBUG_REQUESTS=1` logs every
request), and anything it does not model answers `501` and is recorded, so a new
upstream request shape fails loudly instead of silently degrading.

Non-writable is the mock's **default**, and that is what keeps the read-only proof
honest. `writable: true` opts into a real update-conflict model — `PUT` compares
`_rev` and answers 409 exactly as CouchDB does, `_bulk_docs` reports per-document
409s (which is how upstream's chunk writer learns a content-addressed chunk already
exists). `new_edits: false`, the shape PouchDB's `{force: true}` produces, is
deliberately **not** modelled: it lands in `unhandled` as a 501, so a regression
that reverts the sidecar to force-writes fails loudly here instead of appearing to
pass. Failure injection (`failNextWrites`, `dropNextEntryPutResponses`) and
`injectConflict` (a replication-style sibling revision, which cannot be produced
through the write API — that is the point of CAS) are what make the retry-safety and
`conflicts` tests possible.

### Closed by the write plane: E2EE and obfuscation

Earlier slices could only prove *classification* — `e2ee-required` and
`e2ee-invalid` against synthetic ciphertext, so a **successful** decrypt was never
exercised and wrong-passphrase detection rested on an AEAD failure that was never
demonstrated. Writing closed that hermetically, because the sidecar can now
generate real ciphertext with the plugin's own library (see
[Committed E2EE fixtures](#committed-e2ee-fixtures)). Covered now: a successful
E2EE round trip through two processes, a wrong-passphrase refusal against real
ciphertext, obfuscated-id resolution in both directions, and drift detection
against committed dumps.

One marker detail is still worth knowing when reading fixtures: the `h:+` id
prefix is what *selects* a chunk for the decryption transform, but `e_: true` plus
a `%=`/`%` header on `data` is what makes upstream actually attempt a decrypt. An
`h:+` chunk without `e_` is classified UNENCRYPTED and passed through verbatim.
The sidecar reads `h:+` presence as "this vault is configured for E2EE" (which is
what `e2ee-required` needs) and relies on a real chunk read for `e2ee-invalid`.

A **plugin-generated** fixture would still be worth having, because it would prove
the sidecar agrees with Obsidian and not merely with itself:

1. install Self-hosted LiveSync in a scratch vault, point it at a throwaway CouchDB,
   and replicate a few notes, one attachment, one deletion and one deliberate conflict;
2. dump the database (`_all_docs?include_docs=true`) and commit it, recording the
   plugin version in the fixture;
3. repeat with E2EE enabled, and again with path obfuscation;
4. re-run on every `commonlibVersion` bump; a diff in the dump is the signal that
   `maxSchemaVersion` needs review.

### Still deferred

These have no hermetic coverage and are **not** faked green:

- **Compression** (`enableCompression`) and **Eden** inline chunks.
- **Chunk-splitter variants** and non-default `hashAlg`, which change how content
  is fragmented on write and therefore what a reader must reassemble.
- **The pre-base64 binary chunk encoding.** Upstream still reads `%`-prefixed
  UTF-16-packed attachment chunks for old documents but states it always writes
  base64 now. The sidecar refuses them with `corrupted-document` rather than
  guessing, because guessing would hand a caller silently wrong bytes.
- **Conflict resolution.** `conflicts` enumerates; nothing merges or deletes.
- **Orphan-chunk collection**, deliberately — see
  [Orphan chunks](#orphan-chunks-deliberately-not-collected).
- **Live-feed reconnection.** Upstream's `beginWatch` retries after 10s on error;
  the tests cover the happy path and cancellation, not a mid-stream drop.
- **Large vaults**: manifest pagination is covered at `limit=2` over six entries,
  not at a scale where CouchDB's own paging behaviour matters.
- **Genuinely interleaved concurrent writers.** Two sidecars racing is covered as a
  strict sequence (A wins, B's stale-base write is refused with A's revision),
  which is what CAS guarantees; mock-side response gating to interleave the two
  round trips would add flake surface without adding coverage.
