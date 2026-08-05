# The multi-backend demo

```bash
scripts/demo-multi-backend.sh
```

One command. No prerequisites beyond **cargo, node >= 20 and curl** — no Docker, no
Algolia account, no CouchDB server, no credentials of your own. `jq` is used for
pretty-printing when it is installed and skipped when it is not.

The script builds a throwaway world in a temp directory, mounts three different
backends under one namespace, boots the real HTTP server against it, and drives the
real MCP tools over JSON-RPC — printing every response and asserting on the parts
that matter. It is therefore both a demo and a smoke test: **any failed assertion
stops the run with a non-zero exit.**

**Runtime:** ~12 s with a warm `target/` (a cold `cargo build` of the workspace
dominates the first run and can take several minutes). Add `--no-build` to skip the
build steps entirely once they are warm.

**Flags**

| flag | effect |
| --- | --- |
| `--keep` | leave the sandbox (config, index, logs, exports) on disk and print its path |
| `--no-build` | skip `cargo build` / `npm run build`; the artefacts must already exist |
| `-h`, `--help` | print the script header, including the secrets rationale |

## The world it builds

| mount | `mountAt` | backend | how it is served |
| --- | --- | --- | --- |
| `vault` | *(root)* | `filesystem` | a temp directory with 3 wiki-linked notes |
| `team` | `Team/` | `couchdb`, **writable** | the sidecar's own mock-CouchDB fixture (`--vault small --writable`), reached through the **real** Node sidecar |
| `wiki` | `Wiki/` | `algolia`, **writable** | `cargo run -p deep-obsidian-algolia --example mock_algolia`, pointed at via the mount's `baseUrl` override |

Both `experimental.multiVault` and the two backend gates
(`couchdbVaults`, `algoliaVaults`) are on. `autoReindex` is **off** on purpose: every
refresh in the demo is an explicit `build_index`, so nothing depends on a timer.

Everything lives under one `mktemp -d` directory. `HOME`, `XDG_CONFIG_HOME` and
`XDG_DATA_HOME` are repointed into it, and the environment variables that could
change the demo's meaning (`DEEP_OBSIDIAN_CONFIG`, `DEEP_OBSIDIAN_AUTH_TOKEN`,
`DEEP_OBSIDIAN_ALGOLIA_API_KEY`, `DEEP_OBSIDIAN_LIVESYNC_SIDECAR`, …) are cleared
rather than trusted. Your real config, vault, index and keyring are never touched.

Ports are probed free at startup (the CouchDB fixture picks its own ephemeral port
and announces it on its handshake line), so several copies of the demo can run at
once — verified with six concurrent runs, all green, including the shared
`npm run build` output. An `EXIT`/`INT` trap kills every child — server, sidecar,
both mocks — by **tracked PID only**, never by name, so a real installed service on
the same machine is never touched. The temp directory goes with it.

## Secrets, and why they are provisioned the way they are

A mount never carries a plaintext credential: `passwordRef` and `apiKeyRef` are
`SecretRef`s, and `SecretRef` has exactly two variants — `osKeyring` and
`encryptedFile` (`rust/crates/deep-obsidian-types/src/lib.rs`). There is no `env:`
variant, and no CLI subcommand that stores a secret non-interactively:
`setup-service --wizard` refuses a config that declares a `mounts` table.

So the demo writes **both** secrets into the sandbox's own encrypted secrets file at
`$XDG_CONFIG_HOME/deep-obsidian-mcp/secrets.json`, using a generated Node helper
that reproduces the XChaCha20-Poly1305 envelope of
`deep_obsidian_config::secrets::EncryptedFileStore`. One mechanism for both
backends, and `print-config` shows only the *references* — which the script asserts.

That key duplication is checked rather than assumed. Step 2 asserts that `doctor
--probe-remote` does **not** report a missing secret for the CouchDB mount, so if
`APP_SECRET_KEY` or the envelope ever changes, the demo fails there, by name,
instead of degrading into a mysteriously unavailable mount three steps later.

For the Algolia key only, `$DEEP_OBSIDIAN_ALGOLIA_API_KEY` also works and takes
precedence over `apiKeyRef` — that is what `scripts/try-algolia-mount.sh` uses. The
demo avoids it deliberately: the override logs a "SHADOWS the key its `apiKeyRef`
points at" warning that would read as a defect here.

Neither mock validates credentials, so the values are arbitrary. What is being
demonstrated is the resolution path, not authentication.

## What each step proves

| step | what you see | what it proves |
| --- | --- | --- |
| **1 Build** | binary, mock-Algolia example, sidecar bundle | nothing is stubbed: the demo runs the shipped code paths |
| **2 Assemble** | the vault, both mocks coming up, `secrets.json`, the config, `print-config`, `doctor --probe-remote` | three backends behind one config; secrets exist only as references; both remotes answer a read-only probe |
| **3 Boot** | `/healthz`, then `vault_info` → `mounts[]` | per-mount `backendKind` and `capabilities`. The Algolia mount advertises **no** `binary-read`/`binary-write`/`upload` and **no local index** (`indexStatus: none`); the CouchDB mount advertises `watch`; the CouchDB mount also volunteers the fixture's pre-existing `conflictedPaths` |
| **4 Routing** | `upsert_note` into `Team/` and `Wiki/`, then `curl` **into each backend directly**; a `grep_search` scoped to `Team/` | the path picks the backend. CouchDB shows a LiveSync *entry* document plus its *leaf* chunk; Algolia shows `note:` + `chunk:` records. `read_file` returns both byte-identical, and `list_children` at the root shows synthesized `Team/`, `Wiki/` beside the physical `Projects/` with no marker distinguishing them. The grep finds a line, with context, on a mount that has **no files for ripgrep to open** — the backend imitates ripgrep over note text read back through the sidecar — and its payload carries no `exhaustive` key, because that scan really did read every note the glob admitted |
| **5 Guarded writes** | a stale `expectedHash`, then a fresh one | compare-and-set on a remote backend: the refusal names both hashes, the refused write changes nothing, the fresh one applies and reports `previousHash` |
| **6 Federated recall** | one unscoped `hybrid_search` | `federated: true`, hits from all three mounts each carrying `mountId`, and a `mounts[]` provenance block: `source: native-recall` + `recallMode: lexical` for Algolia vs `source: local-index` for the two indexed mounts. `rerank: none` because no embedding backend is configured — the honest lexical fallback rather than a fake semantic score |
| **7 Honest degradation** | the Algolia mock is killed, searched, restarted, restored | recall does not silently shrink: `degraded: true` + `missingBackends: ["wiki"]` + a `degradationReason` naming the mount, surviving hits intact, and a read through the dead mount refused loudly. See *Degradation and recovery* below |
| **8 The binary exception** | `read_artifact` and `request_vault_upload` on `Wiki/`, then the same upload on the filesystem mount | the Markdown-only storage refuses by name at both doors, and the upload is refused **at the mint** — before a token exists — rather than after a body has been streamed. The same tools keep working on the filesystem mount |
| **9 Versioning** | two writes, `note_history`, `read_version`, `delete_note` | writes append versions rather than overwriting; every version records its `participantId` and what it superseded; deletion is a tombstone that carries its own `howToRecover`, and the history outlives the note |
| **10 Ops** | `doctor` mount lines, `couchdb export --mount team`, `algolia dump --mount wiki` | an operator can see what is mounted and get the data out of either remote backend as a directory tree plus a `manifest.json`. **Also surfaces one real bug** — see below |
| **11 Teardown** | a recap | every child killed, sandbox removed |

## Degradation and recovery: what is actually observed

Killing the Algolia mock and re-running the same search yields, on the **next call**:

```json
{ "degraded": true, "missingBackends": ["wiki"],
  "degradationReason": "mount 'wiki' could not be searched: algolia mount error: ..." }
```

with the four hits from the two surviving mounts unchanged, and the `wiki` entry still
listed in `mounts[]` carrying an `error` string and `candidateCount: 0`. A `read_file`
through the dead mount fails with the transport error rather than returning "no such
note". Recovery needs **no server restart and no reindex**: the flag is recomputed per
call, so the very next search after the mock comes back reports `degraded: false`.

There is one honest wrinkle the script says out loud rather than hiding. The mock keeps
its corpus **in memory**, so killing the process destroys the index along with it: after
the restart the mount is healthy but contributes `candidateCount: 0`. A real Algolia
outage would not lose the corpus. So the script takes an `algolia dump` *before* the
outage and uses `algolia restore` afterwards to bring the notes back — which is what an
operator would do anyway, and doubles as a dump/restore round-trip proof. One
consequence surfaces in step 9: the restored note's version chain starts at the restore,
because the earlier versions went with the mock's memory.

## One thing presenters get asked about

Step 8 uses `Wiki/diagram.png`, not a `.md` path. `read_artifact` on a Markdown path
under the same mount answers `unsupported artifact type` instead: the artifact-type
check fires before the request reaches the mount, so only a binary path surfaces the
mount's own Markdown-only refusal. Not a bug — just the order of two checks.

## A bug this demo found (fixed in the same PR)

An earlier revision of this demo caught `doctor` printing a **writable** CouchDB
mount as `(read-only)`: `render_mount_line`'s `Couchdb` arm hard-coded the suffix
and never read `writable`, while its `Algolia` arm conditioned on it correctly —
and no test pinned either string, so nothing caught it. The arm is fixed and both
states are pinned by `doctor_mount_lines_track_writability`; step 10 now asserts
that neither writable mount line carries `(read-only)`.

## Troubleshooting

**A port is in use.** The demo never hard-codes a port: it probes free ones and reads
the CouchDB fixture's ephemeral port off its handshake line. If a bind still fails,
another process grabbed the port in the window between probe and bind — just re-run.

**`node: command not found` / node too old.** Node >= 20 is a hard requirement: the
LiveSync sidecar, the mock-CouchDB fixture, and the demo's own JSON/secret helper are
all Node. `doctor` reports the version it found as `mount.team.sidecar-node`.

**Build failures.** Step 1 stops the run. Build by hand to see the full error:
`cargo build -p deep-obsidian-cli`, `cargo build -p deep-obsidian-algolia --example
mock_algolia`, and `npm run build` in `sidecar/livesync-sidecar`.

**"missing secret" in step 2.** The encrypted-secret envelope changed. Compare the
demo's Node helper against `APP_SECRET_KEY` and `EncryptedFileStore` in
`rust/crates/deep-obsidian-config/src/secrets.rs`.

**An assertion fails elsewhere.** Re-run with `--keep` and inspect the sandbox: it has
`config.json`, `server.log`, `mock-algolia.log`, `couch-stdout`, `couch-stderr`, the
index directory, and every export the run produced.

**Nothing is left behind.** If a run is killed hard enough to skip the trap
(`kill -9`), check for stray `mock_algolia`, `mock-couch-server.mjs`, `sidecar.mjs`
and `deep-obsidian-mcp serve` processes, and remove
`${TMPDIR:-/tmp}/deep-obsidian-demo.*`.

## Related

- [algolia-mounts.md](./algolia-mounts.md) — the Algolia backend in depth.
- [behavior-contract.md](./behavior-contract.md#multi-mount-vaults) — the multi-mount
  rules a client can rely on, including the federation and degradation carriers.
- `scripts/try-algolia-mount.sh` — point an agent at a **real** shared Algolia index
  instead of a mock.
