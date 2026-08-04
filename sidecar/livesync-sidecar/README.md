# `@deep-obsidian/livesync-sidecar`

A read-only Node process that exposes a [Self-hosted LiveSync](https://github.com/vrtmrz/obsidian-livesync)
CouchDB vault to the `deep-obsidian` MCP server over a versioned JSON-RPC-over-stdio
protocol.

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

The sidecar **only reads**. It never writes to the remote database — see
[Security posture](#security-posture).

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
| `initialize` | Handshake, credentials, compatibility gate. The only method that receives secrets. |
| `manifest` | Paginated vault metadata listing. |
| `read` | Full content of one entry, assembled from its chunks. |
| `stat` | Metadata for one entry, no chunk fetch. |
| `changesSince` | One-shot change feed from an opaque cursor. |
| `watch` / `unwatch` | Subscribe to / unsubscribe from live `change` notifications. |
| `health` | Liveness, compatibility status, watch state, last error. |
| `shutdown` | Graceful exit 0. |

`initialize` must come first. Every data method (`manifest`, `read`, `stat`,
`changesSince`, `watch`, `unwatch`) **fails closed** until `initialize` has
returned `compatibility.status: "ok"`. `health` and `shutdown` are always
available — the supervisor needs them exactly when something is wrong.

### `initialize`

```json
{
  "jsonrpc": "2.0", "id": 1, "method": "initialize",
  "params": {
    "protocolVersion": 1,
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
| `e2ee-invalid` | Passphrase supplied but unusable (see below). |
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
- **Conflicts**: the winning revision is served and `conflicted: true` is set.
  Conflict revisions are not exposed in v1.
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
would advertise entries `read` then reports as missing.

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
- **Read-only, structurally.** `put`, `delete`, and `putSyncParameters` are
  overridden to throw. `putSyncParameters` matters most: upstream's
  `getReplicationPBKDF2Salt` will *create* the remote's
  `_local/obsidian_livesync_sync_parameters` document when it is missing, and it
  is wired into the decryption path as a lazy callback — so an E2EE read could
  otherwise write to someone's vault mid-read. The sidecar checks for the salt up
  front and reports `e2ee-invalid` instead.
- **The milestone document is never written.** Upstream's
  `ensureDatabaseIsCompatible` registers the calling node and updates
  `last_connected`/tweak values as a side effect of *checking* compatibility. The
  sidecar reimplements the checks read-only: it is a reader, not a peer, so it
  never appears in `accepted_nodes`.
- **The version document is never written.** Upstream's `checkRemoteVersion`
  falls through to `bumpRemoteVersion` on a miss, which PUTs
  `obsydian_livesync_version`.
- `test/vault.test.mjs` asserts this at the transport: the mock CouchDB records
  every `PUT`/`DELETE` and every mutating `POST`, and the test requires that list
  to be empty.

## Tests

```sh
npm ci
npm run typecheck
npm run build      # required: the suite drives dist/sidecar.mjs
npm test
```

The suite spawns the **built bundle as a child process** and speaks the real
protocol over real pipes. That is deliberate: it exercises the newline framing,
the stdout/stderr split, and the shutdown path, none of which an in-process call
would check. It also means nothing can be injected into the sidecar, which is why
`test/mock-couch.mjs` is a real HTTP server emulating CouchDB rather than a fake
`fetch`.

The mock's endpoint set was discovered empirically (`DEBUG_REQUESTS=1` logs every
request), and anything it does not model answers `501` and is recorded, so a new
upstream request shape fails loudly instead of silently degrading.

### Deferred to slice 3c (real CouchDB + real plugin)

These are **not** faked green here; they have no hermetic coverage and are called
out so the next slice knows what to exercise:

- **Real E2EE round-trip.** Fixture ciphertext would need the plugin's HKDF key
  schedule to be trustworthy. `test/compat.test.mjs` covers the
  `e2ee-required` / `e2ee-invalid` *classification*, including a chunk whose
  decryption is genuinely attempted and fails — but the ciphertext is synthetic,
  so a **successful** decrypt is never exercised. Two consequences: correct-vault
  reads through the E2EE path are unproven here, and wrong-passphrase detection
  is only as strong as the underlying AEAD's failure (reliable for real
  ciphertext, but not demonstrated against it).

  Note the marker semantics, which are easy to get wrong when building fixtures:
  the `h:+` id prefix is what *selects* a chunk for the decryption transform, but
  `e_: true` plus a `%=`/`%` header on `data` is what makes upstream actually
  attempt a decrypt. An `h:+` chunk without `e_` is classified UNENCRYPTED and
  passed through verbatim. The sidecar reads `h:+` presence as "this vault is
  configured for E2EE" (which is what `e2ee-required` needs) and relies on a real
  chunk read for `e2ee-invalid`.
- **Real path obfuscation.** Same reason: `f:` ids are salted hashes. The
  detection path and the stderr path-suppression behaviour are covered; resolving
  an obfuscated id back to a path is not.
- **Compression** (`enableCompression`) and **Eden** inline chunks.
- **Chunk-splitter variants** and non-default `hashAlg`, which change how content
  is fragmented on write and therefore what a reader must reassemble.
- **Live-feed reconnection.** Upstream's `beginWatch` retries after 10s on error;
  the tests cover the happy path and cancellation, not a mid-stream drop.
- **Large vaults**: manifest pagination is covered at `limit=2` over six entries,
  not at a scale where CouchDB's own paging behaviour matters.
