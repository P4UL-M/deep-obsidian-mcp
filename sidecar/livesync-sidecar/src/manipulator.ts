/**
 * The one file that touches `@vrtmrz/livesync-commonlib`.
 *
 * Upstream is pre-1.0 and documents its own semantics as "not final", so the
 * whole surface is quarantined here: `main.ts` sees only the protocol types.
 * When commonlib moves, this file is the blast radius.
 *
 * Write posture, enforced structurally rather than by convention:
 *
 *   * `GuardedManipulator` overrides every writing method commonlib exposes.
 *     `putSyncParameters` is refused **unconditionally, in both modes**:
 *     upstream's `getReplicationPBKDF2Salt` will happily call it to *create* the
 *     remote's `_local/obsidian_livesync_sync_parameters` document when it is
 *     missing, and neither a reader nor a writer gets to establish another
 *     vault's replication salt. `put` is refused unless the sidecar was
 *     initialized `mode: "read-write"`; `delete` is refused always, because
 *     upstream's own `delete` cannot express compare-and-swap (see below).
 *   * The compatibility gate reimplements the milestone checks from
 *     `ensureRemoteIsCompatible` instead of calling it, because that function
 *     writes the milestone document (node registration, tweak values,
 *     `last_connected`) as a side effect of *checking* it. This stays true in
 *     read-write mode: a writer client is still not a LiveSync peer and must
 *     never appear in `accepted_nodes`.
 *   * The version check reimplements `checkRemoteVersion` for the same reason:
 *     upstream's version misses fall through to `bumpRemoteVersion`, which
 *     PUTs `obsydian_livesync_version`.
 *
 * Documented drift from the public `DirectFileManipulator` API, and why:
 *
 *   * **`put` has no compare-and-swap, and cannot be given one.** Upstream's
 *     `putDBEntry` writes the entry root with `localDatabase.put(doc, {force:
 *     true})`. PouchDB turns `force` into `new_edits: false` plus a *fabricated*
 *     child revision (`pouchdb-core/lib/index.js`,
 *     `transformForceOptionToNewEditsOption`), so a stale base revision never
 *     produces a 409 -- it silently grafts a second leaf onto the revision tree,
 *     i.e. creates a conflict. That is the right behaviour for a replicating
 *     peer and the wrong behaviour for an MCP tool whose contract is
 *     `expectedHash`. Its `conflictBaseRev` parameter does not help either: it
 *     only chooses *which* revision the forced write chains from.
 *
 *     So `write` keeps all of `putDBEntry` (target-file filtering, blob typing,
 *     the splitter, chunk batching, Eden, and above all chunk id derivation and
 *     the encryption transform) and replaces exactly one operation: the final
 *     root `put`. A one-shot interceptor on `localDatabase.put` re-issues the
 *     entry-root write *without* `force` and with the revision the CAS
 *     precondition validated, so CouchDB itself adjudicates the race. The
 *     interceptor asserts it fired exactly once: if upstream ever stops routing
 *     the root write through `localDatabase.put`, the write fails loudly instead
 *     of silently reverting to force-write semantics.
 *   * **`delete` is not used.** `deleteDBEntryByPath` also force-puts, for the
 *     same reason. `delete` re-implements it: read the entry, set `deleted:
 *     true` and bump `mtime`, put it back under a revision guard. It never sets
 *     `_deleted` -- upstream only does that when `deleteMetadataOfDeletedFiles`
 *     is on, and it is off by default.
 *
 *   * `enumerateAllNormalDocs` is unusable for `manifest`: it is a generator
 *     with no exposed position (no resumable cursor) and it hardcodes
 *     `findEntries(start, end, {})`, so `conflicts: true` cannot be threaded
 *     through and `_conflicts` never comes back. `manifest` therefore drives
 *     `liveSyncLocalDB.localDatabase.allDocs` over the *same five id ranges*
 *     upstream uses, copied verbatim so the `h:` / `i:` / `ix:` / `ps:`
 *     exclusions stay identical.
 *   * `get(path, metaOnly)` cannot request conflicts either. `read`/`stat` call
 *     `liveSyncLocalDB.getDBEntryMeta(path, { conflicts: true }, true)`, which
 *     does accept PouchDB get options and an include-deleted flag.
 *   * `followUpdates()` is NOT used. It requests `filter: "replicate/pull"`,
 *     i.e. a `_design/replicate` design document with a `pull` filter. No such
 *     document is created anywhere in the plugin or in commonlib, so against a
 *     real LiveSync vault that call 404s. `changesSince` drives
 *     `localDatabase.changes({ since, live: false })` directly and filters
 *     note types client-side.
 *   * `enumerate(cond)` is an untested upstream stub (an empty generator).
 *     Never called.
 */
import { DirectFileManipulator } from "@vrtmrz/livesync-commonlib";
import type { DirectFileManipulatorOptions } from "@vrtmrz/livesync-commonlib";
import type {
    ChangeEntry,
    Compatibility,
    CompatibilityStatus,
    ConflictDetail,
    ConflictsResult,
    ConflictRevision,
    DeleteResult,
    EntryKind,
    InitializeOptions,
    ManifestEntry,
    ReadResult,
    SidecarMode,
    StatResult,
    WriteParams,
    WriteResult,
} from "./protocol.js";
import { SidecarError, SUPPORTED } from "./protocol.js";
import { logStderr, registerSecrets, setSuppressPaths } from "./logging.js";

/* -------------------------------------------------------------------------- */
/* Upstream constants, restated                                                */
/* -------------------------------------------------------------------------- */

/**
 * Restated rather than imported, deliberately.
 *
 * These ids and prefixes are the *remote data format*, not an implementation
 * detail of the library: they are what a CouchDB vault physically contains. If
 * a commonlib upgrade silently renamed one of them, importing the constant
 * would make the sidecar follow the rename and quietly talk to the wrong
 * document; restating it makes the mismatch a test failure instead. The values
 * are asserted against upstream in `test/upstream-constants.test.mjs`.
 */
const VERSIONING_DOCID = "obsydian_livesync_version";
const MILESTONE_DOCID = "_local/obsydian_livesync_milestone";
const SYNC_PARAMETERS_DOCID = "_local/obsidian_livesync_sync_parameters";

const PREFIX_ENCRYPTED_CHUNK = "h:+";
const PREFIX_OBFUSCATED = "f:";

/** Sentinel above every valid id character, as used by upstream range scans. */
const MAX_CHAR = "\u{10ffff}";

/**
 * The id ranges `DirectFileManipulator.enumerateAllNormalDocs` scans, copied
 * verbatim. The gaps are the exclusions: `h:` chunks, `i:` internal (hidden)
 * files, `ix:` internal-file index docs, and `ps:` plugin-sync settings.
 */
const NORMAL_DOC_RANGES: readonly (readonly [string, string])[] = [
    ["", "h:"],
    [`h:${MAX_CHAR}`, "i:"],
    [`i:${MAX_CHAR}`, "ix:"],
    [`ix:${MAX_CHAR}`, "ps:"],
    [`ps:${MAX_CHAR}`, MAX_CHAR],
];

const DEFAULT_REQUEST_TIMEOUT_MS = 30_000;
const READY_TIMEOUT_MS = 45_000;

/* -------------------------------------------------------------------------- */
/* Minimal structural types for the pieces of PouchDB we touch                 */
/* -------------------------------------------------------------------------- */

type RawDoc = {
    _id: string;
    _rev?: string;
    _deleted?: boolean;
    _conflicts?: string[];
    type?: string;
    path?: string;
    ctime?: number;
    mtime?: number;
    size?: number;
    deleted?: boolean;
    children?: string[];
    version?: number;
    [key: string]: unknown;
};

/**
 * Structural stand-ins for `MetaEntry` / `ReadyEntry`.
 *
 * The published package's root export is exactly `DirectFileManipulator` plus
 * its options types -- the entry-document types are not re-exported -- so these
 * are declared locally rather than reaching into a `compat/` deep import that
 * upstream may drop.
 */
type LoadedEntryLike = RawDoc & { data: string[] };

type AllDocsRow = { id: string; key: string; value?: unknown; doc?: RawDoc | null };
type AllDocsResponse = { total_rows?: number; offset?: number; rows: AllDocsRow[] };
type ChangesRow = { id: string; seq: string | number; deleted?: boolean; doc?: RawDoc | null };
type ChangesResponse = { results: ChangesRow[]; last_seq: string | number };

type PutResponse = { ok?: boolean; id?: string; rev?: string };

/**
 * The pieces of the PouchDB handle this file touches.
 *
 * Note this is the handle *after* `transform-pouch` has wrapped it (commonlib
 * installs the encryption transform while initialising the database), so every
 * method here is the transforming one: a `put` through this type encrypts, a
 * `get` decrypts. Bypassing it -- e.g. issuing raw HTTP -- would write plaintext
 * into an E2EE vault.
 */
type LocalDatabase = {
    allDocs(options: Record<string, unknown>): Promise<AllDocsResponse>;
    changes(options: Record<string, unknown>): Promise<ChangesResponse>;
    info(): Promise<{ update_seq?: string | number; doc_count?: number }>;
    get(id: string, options?: Record<string, unknown>): Promise<RawDoc>;
    put(doc: RawDoc, options?: Record<string, unknown>): Promise<PutResponse>;
};

type MilestoneDoc = {
    locked?: boolean;
    cleaned?: boolean;
    accepted_nodes?: string[];
    node_chunk_info?: Record<string, { min: number; max: number; current?: number }>;
    tweak_values?: Record<string, Record<string, unknown>>;
};

/* -------------------------------------------------------------------------- */
/* Guarded subclass                                                            */
/* -------------------------------------------------------------------------- */

/**
 * A write the sidecar structurally forbids, in any mode.
 *
 * Deliberately `internal-error` rather than `read-only`: reaching one of these
 * means an upstream code path tried to write a control document behind our back,
 * which is a bug or an upstream change, not a caller asking for something the
 * configuration disallows. `read-only` is reserved for the latter.
 */
function refuseWrite(operation: string): never {
    throw new SidecarError(
        "internal-error",
        `sidecar refused a forbidden write attempt (${operation})`
    );
}

/**
 * `DirectFileManipulator` with every write path fused, then selectively opened.
 *
 * `putSyncParameters` is the important one: it is reachable from
 * `getReplicationPBKDF2Salt`, which commonlib wires into the *encryption and
 * decryption* path as a lazily-invoked callback, so an E2EE read (or write) of a
 * vault whose sync-parameters document lacks a salt would otherwise write to the
 * remote mid-operation. That stays refused in read-write mode: establishing
 * another client's replication salt is not this process's business.
 *
 * `put` is gated on `writesAllowed`, which the vault sets after a successful
 * `initialize` in read-write mode. `delete` stays refused unconditionally
 * because it force-writes; `LiveSyncVault.remove` replaces it.
 */
class GuardedManipulator extends DirectFileManipulator {
    /** Set by the owning vault once the mode is known. */
    writesAllowed = false;

    override putSyncParameters(): Promise<boolean> {
        return refuseWrite("putSyncParameters");
    }
    override put(...args: Parameters<DirectFileManipulator["put"]>): Promise<boolean> {
        if (!this.writesAllowed) {
            return refuseWrite("put");
        }
        return super.put(...args);
    }
    override delete(): Promise<boolean> {
        return refuseWrite("delete");
    }
}

/* -------------------------------------------------------------------------- */
/* Cursors                                                                     */
/* -------------------------------------------------------------------------- */

/**
 * Manifest cursors are opaque to the host: base64url of `{r, k}`, where `r` is
 * the index into `NORMAL_DOC_RANGES` and `k` the last id emitted.
 */
type ManifestCursor = { r: number; k: string | null };

function encodeManifestCursor(cursor: ManifestCursor): string {
    return Buffer.from(JSON.stringify(cursor), "utf8").toString("base64url");
}

function decodeManifestCursor(raw: string | null | undefined): ManifestCursor {
    if (raw === undefined || raw === null || raw === "") return { r: 0, k: null };
    try {
        const parsed = JSON.parse(Buffer.from(raw, "base64url").toString("utf8")) as unknown;
        if (
            typeof parsed === "object" &&
            parsed !== null &&
            typeof (parsed as ManifestCursor).r === "number"
        ) {
            const value = parsed as ManifestCursor;
            return { r: value.r, k: typeof value.k === "string" ? value.k : null };
        }
    } catch {
        /* fall through to the typed error below */
    }
    throw new SidecarError("invalid-params", "malformed manifest cursor");
}

/* -------------------------------------------------------------------------- */
/* Classification helpers                                                      */
/* -------------------------------------------------------------------------- */

function statusFromTransportError(error: unknown): { status: CompatibilityStatus; detail: string } {
    const anyError = error as { status?: number; code?: string; message?: string; name?: string };
    const httpStatus = typeof anyError?.status === "number" ? anyError.status : undefined;
    if (httpStatus === 401 || httpStatus === 403) {
        return { status: "auth-failed", detail: `CouchDB rejected the credentials (HTTP ${httpStatus})` };
    }
    const code = anyError?.code ?? "";
    const message = anyError?.message ?? String(error);
    if (
        code === "ECONNREFUSED" ||
        code === "ENOTFOUND" ||
        code === "EAI_AGAIN" ||
        code === "ETIMEDOUT" ||
        code === "ECONNRESET" ||
        code === "UND_ERR_CONNECT_TIMEOUT" ||
        anyError?.name === "AbortError" ||
        anyError?.name === "TimeoutError" ||
        /fetch failed|network|timeout|socket hang up/i.test(message)
    ) {
        return { status: "unreachable", detail: `CouchDB could not be reached (${code || message})` };
    }
    if (httpStatus !== undefined && httpStatus >= 500) {
        return { status: "unreachable", detail: `CouchDB returned HTTP ${httpStatus}` };
    }
    return { status: "unknown", detail: message };
}

function kindOfEntry(doc: RawDoc): EntryKind {
    if (doc._id.startsWith("i:")) return "internal";
    if (doc.type === "newnote") return "binary";
    return "markdown";
}

/**
 * The visibility rule shared by `manifest`, `read`, and `stat`.
 *
 * Commonlib's `isTargetFile` refuses paths containing `:` and -- with default
 * settings -- paths starting with `.`, and `getDBEntryMeta` returns `false` for
 * them. Applying the same rule when enumerating keeps `manifest` and `read`
 * from disagreeing about what exists.
 */
function isVisiblePath(path: string): boolean {
    if (path === "") return false;
    if (path.includes(":")) return false;
    const basename = path.startsWith("/") ? path.slice(1) : path;
    if (basename.startsWith(".")) return false;
    return true;
}

/* -------------------------------------------------------------------------- */
/* The adapter                                                                 */
/* -------------------------------------------------------------------------- */

export type ConnectParams = {
    url: string;
    database: string;
    username: string;
    password: string;
    passphrase?: string;
    obfuscatePassphrase?: string;
    options?: InitializeOptions;
    /** Defaults to `"read-only"`. */
    mode?: SidecarMode;
};

export type ConnectOutcome = {
    compatibility: Compatibility;
    remote: { schemaVersion?: number; encrypted: boolean; pathObfuscation: boolean };
};

export class LiveSyncVault {
    private manipulator: GuardedManipulator | undefined;
    private compatibility: Compatibility = { status: "unknown" };
    private encrypted = false;
    private pathObfuscation = false;
    private watchCallback: ((change: ChangeEntry & { cursor: string }) => void) | undefined;
    private watching = false;
    private mode: SidecarMode = "read-only";

    get isServeable(): boolean {
        return this.manipulator !== undefined && this.compatibility.status === "ok";
    }

    get compatibilityStatus(): Compatibility {
        return this.compatibility;
    }

    get isWatching(): boolean {
        return this.watching;
    }

    get currentMode(): SidecarMode {
        return this.mode;
    }

    private db(): LocalDatabase {
        const manipulator = this.manipulator;
        if (!manipulator) {
            throw new SidecarError("not-initialized", "initialize has not been called");
        }
        return manipulator.liveSyncLocalDB.localDatabase as unknown as LocalDatabase;
    }

    private requireServeable(): GuardedManipulator {
        const manipulator = this.manipulator;
        if (!manipulator) {
            throw new SidecarError("not-initialized", "initialize has not been called");
        }
        if (this.compatibility.status !== "ok") {
            throw new SidecarError(
                "incompatible-remote",
                this.compatibility.detail ?? `remote is not serveable (${this.compatibility.status})`,
                this.compatibility.status
            );
        }
        return manipulator;
    }

    /**
     * The write gate.
     *
     * The mode is checked *before* serveability on purpose: "this sidecar was
     * started read-only" is a configuration fact the host can act on
     * immediately, and reporting it as `incompatible-remote` would send a host
     * chasing the vault instead of its own config.
     */
    private requireWritable(): GuardedManipulator {
        if (this.mode !== "read-write") {
            throw new SidecarError(
                "read-only",
                'this sidecar was initialized with mode "read-only"; write methods are refused'
            );
        }
        return this.requireServeable();
    }

    /**
     * Connects, runs the compatibility gate, and reports the outcome.
     *
     * Never throws for a remote-side problem: transport, credential, schema,
     * milestone, and E2EE failures all come back as a `CompatibilityStatus` so
     * the supervisor can report one precise reason. Only programming errors
     * escape.
     */
    async connect(params: ConnectParams): Promise<ConnectOutcome> {
        registerSecrets([params.password, params.passphrase, params.obfuscatePassphrase, params.url]);
        setSuppressPaths(Boolean(params.obfuscatePassphrase));
        this.mode = params.mode ?? "read-only";

        const options = params.options ?? {};
        const timeoutMs = options.requestTimeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS;

        const manipulatorOptions: DirectFileManipulatorOptions = {
            url: params.url.replace(/\/+$/, ""),
            database: params.database,
            username: params.username,
            password: params.password,
            passphrase: params.passphrase,
            obfuscatePassphrase: params.obfuscatePassphrase,
            ...(options.customChunkSize !== undefined ? { customChunkSize: options.customChunkSize } : {}),
            ...(options.minimumChunkSize !== undefined ? { minimumChunkSize: options.minimumChunkSize } : {}),
            ...(options.hashAlg !== undefined
                ? { hashAlg: options.hashAlg as DirectFileManipulatorOptions["hashAlg"] }
                : {}),
            ...(options.useEden !== undefined ? { useEden: options.useEden } : {}),
            ...(options.enableCompression !== undefined
                ? { enableCompression: options.enableCompression }
                : {}),
            ...(options.handleFilenameCaseSensitive !== undefined
                ? { handleFilenameCaseSensitive: options.handleFilenameCaseSensitive }
                : {}),
            ...(options.chunkSplitterVersion !== undefined
                ? {
                      chunkSplitterVersion:
                          options.chunkSplitterVersion as unknown as DirectFileManipulatorOptions["chunkSplitterVersion"],
                  }
                : {}),
            ...(options.e2eeAlgorithm !== undefined
                ? { E2EEAlgorithm: options.e2eeAlgorithm as DirectFileManipulatorOptions["E2EEAlgorithm"] }
                : {}),
        };

        const manipulator = new GuardedManipulator(manipulatorOptions, {
            fetch: makeTimeoutFetch(timeoutMs),
        });
        this.manipulator = manipulator;

        try {
            await withTimeout(
                manipulator.ready.promise,
                READY_TIMEOUT_MS,
                "opening the remote database timed out"
            );
        } catch (error) {
            // `ready` also rejects when the live change feed commonlib opens
            // during initialisation cannot be established, so fall through to
            // an explicit reachability probe before deciding it is fatal.
            const classified = statusFromTransportError(error);
            const probe = await this.probeReachable();
            if (probe !== "ok") {
                this.compatibility = probe;
                return this.outcome();
            }
            logStderr("compat", `database open reported a problem, continuing: ${classified.detail}`);
        }

        const probe = await this.probeReachable();
        if (probe !== "ok") {
            this.compatibility = probe;
            return this.outcome();
        }

        const gate = await this.runCompatibilityGate(params);
        this.compatibility = gate;
        // Writes are unlocked only once the gate says "ok": a vault we do not
        // fully understand must not be written to even in read-write mode. This
        // is what makes the live "unknown-schema still refuses writes" test
        // fail-closed rather than fail-friendly.
        manipulator.writesAllowed = this.mode === "read-write" && gate.status === "ok";
        return this.outcome();
    }

    private outcome(): ConnectOutcome {
        return {
            compatibility: this.compatibility,
            remote: {
                ...(this.schemaVersion !== undefined ? { schemaVersion: this.schemaVersion } : {}),
                encrypted: this.encrypted,
                pathObfuscation: this.pathObfuscation,
            },
        };
    }

    private schemaVersion: number | undefined;

    /** Cheapest possible liveness/credential check: `GET /{db}`. */
    private async probeReachable(): Promise<Compatibility | "ok"> {
        try {
            await this.db().info();
            return "ok";
        } catch (error) {
            const { status, detail } = statusFromTransportError(error);
            return { status, detail };
        }
    }

    /**
     * The pre-serve gate. Order matters: schema first (a wrong schema makes
     * every later interpretation meaningless), then the milestone lock/cleaned
     * states (the vault is explicitly telling clients to stay out), then
     * chunk-version compatibility, then tweak agreement, then E2EE.
     */
    private async runCompatibilityGate(params: ConnectParams): Promise<Compatibility> {
        // 1. Schema version. Reimplemented rather than delegated: upstream's
        //    `checkRemoteVersion` PUTs the version document when it is missing
        //    or behind.
        let versionDoc: RawDoc | false;
        try {
            versionDoc = await this.rawGet(VERSIONING_DOCID);
        } catch (error) {
            const { status, detail } = statusFromTransportError(error);
            return { status, detail };
        }
        if (versionDoc === false) {
            return {
                status: "unknown-schema",
                detail: `remote has no ${VERSIONING_DOCID} document; it is not a LiveSync vault (or has never been initialised by the plugin)`,
            };
        }
        if (versionDoc.type !== "versioninfo" || typeof versionDoc.version !== "number") {
            return { status: "unknown-schema", detail: `${VERSIONING_DOCID} is malformed` };
        }
        this.schemaVersion = versionDoc.version;
        if (versionDoc.version > SUPPORTED.maxSchemaVersion) {
            return {
                status: "unknown-schema",
                detail: `remote schema version ${versionDoc.version} is newer than the supported maximum ${SUPPORTED.maxSchemaVersion}`,
            };
        }

        // 2. Milestone. Read-only: never registered as an accepted node, never
        //    written. `locked` means a rebuild is in flight; `locked+cleaned`
        //    means chunks were purged and any read would be torn.
        let milestone: RawDoc | false;
        try {
            milestone = await this.rawGet(MILESTONE_DOCID);
        } catch (error) {
            const { status, detail } = statusFromTransportError(error);
            return { status, detail };
        }
        if (milestone !== false) {
            const info = milestone as unknown as MilestoneDoc;
            if (info.locked && info.cleaned) {
                return {
                    status: "cleaned",
                    detail: "remote milestone is locked and cleaned: chunks were purged, every client must resync before this vault can be read",
                };
            }
            if (info.locked) {
                return {
                    status: "locked",
                    detail: "remote milestone is locked: a rebuild or cleanup is in progress",
                };
            }
            const chunkVersions = this.intersectChunkVersions(info);
            if (chunkVersions !== undefined && chunkVersions.max < chunkVersions.min) {
                return {
                    status: "incompatible",
                    detail: `accepted nodes agree on no common chunk format version (min ${chunkVersions.min} > max ${chunkVersions.max})`,
                };
            }
            const mismatch = this.checkTweaks(info, params);
            if (mismatch) {
                return { status: "mismatched", detail: mismatch };
            }
        }

        // 3. Encryption. Presence of an `h:+` chunk is the ground truth for
        //    "this vault is encrypted"; the milestone's `encrypt` tweak is only
        //    advisory and may be absent.
        let encryptedChunkId: string | undefined;
        try {
            encryptedChunkId = await this.firstIdWithPrefix(PREFIX_ENCRYPTED_CHUNK);
            this.pathObfuscation = (await this.firstIdWithPrefix(PREFIX_OBFUSCATED)) !== undefined;
        } catch (error) {
            const { status, detail } = statusFromTransportError(error);
            return { status, detail };
        }
        this.encrypted = encryptedChunkId !== undefined;

        if (this.encrypted && !params.passphrase) {
            return {
                status: "e2ee-required",
                detail: "remote holds end-to-end encrypted chunks but no passphrase was supplied",
            };
        }
        if (params.passphrase) {
            const e2ee = await this.verifyE2EE(encryptedChunkId);
            if (e2ee) return e2ee;
        }
        if (this.pathObfuscation && !params.obfuscatePassphrase) {
            return {
                status: "e2ee-required",
                detail: "remote holds obfuscated document ids but no obfuscation passphrase was supplied",
            };
        }

        return { status: "ok" };
    }

    /**
     * Chunk-format intersection over the milestone's accepted nodes, modelled
     * on `ensureRemoteIsCompatible` but without registering this client: we are
     * a reader, not a peer, so our own range never enters the intersection and
     * nothing is written back.
     */
    private intersectChunkVersions(info: MilestoneDoc): { min: number; max: number } | undefined {
        const accepted = info.accepted_nodes ?? [];
        const chunkInfo = info.node_chunk_info ?? {};
        if (accepted.length === 0) return undefined;
        let min = Number.NEGATIVE_INFINITY;
        let max = Number.POSITIVE_INFINITY;
        for (const node of accepted) {
            const range = chunkInfo[node];
            if (!range) {
                // Upstream treats an unknown peer as 0..0. Mirror that.
                min = Math.max(0, min);
                max = Math.min(0, max);
                continue;
            }
            min = Math.max(range.min, min);
            max = Math.min(range.max, max);
        }
        if (!Number.isFinite(min) || !Number.isFinite(max)) return undefined;
        return { min, max };
    }

    /**
     * Compares the remote's PREFERRED tweak values against the options we were
     * given, but only for the settings that change how bytes are *read*.
     *
     * Upstream compares its whole `TweakValuesShouldMatchedTemplate`, which
     * includes writer-only knobs (chunk sizes, splitter version, Eden limits).
     * A read-only client that disagrees about those still reads correctly, so
     * flagging them would refuse healthy vaults. What genuinely breaks reading
     * is disagreement about encryption, path obfuscation, or filename case
     * handling.
     */
    private checkTweaks(info: MilestoneDoc, params: ConnectParams): string | undefined {
        const preferred = info.tweak_values?.["PREFERRED"];
        if (!preferred) return undefined;
        const problems: string[] = [];
        if (preferred["encrypt"] === true && !params.passphrase) {
            problems.push("remote prefers end-to-end encryption but no passphrase was supplied");
        }
        if (preferred["usePathObfuscation"] === true && !params.obfuscatePassphrase) {
            problems.push("remote prefers path obfuscation but no obfuscation passphrase was supplied");
        }
        const wantCaseSensitive = params.options?.handleFilenameCaseSensitive ?? false;
        if (
            typeof preferred["handleFilenameCaseSensitive"] === "boolean" &&
            preferred["handleFilenameCaseSensitive"] !== wantCaseSensitive
        ) {
            problems.push(
                `remote prefers handleFilenameCaseSensitive=${String(preferred["handleFilenameCaseSensitive"])}`
            );
        }
        return problems.length > 0 ? problems.join("; ") : undefined;
    }

    /**
     * Verifies the passphrase without writing.
     *
     * The PBKDF2 salt lives in `_local/obsidian_livesync_sync_parameters`. If
     * that document is missing (or saltless) upstream's salt handler would
     * *create* it, so its absence is reported as `e2ee-invalid` rather than
     * letting the decryption path reach the write. Given a salt, one real chunk
     * read is the only honest test of the passphrase.
     *
     * A *writer* needs the salt even when the remote holds no encrypted chunk
     * yet: the encryption transform derives the key lazily on the first chunk it
     * encrypts, and the handler's miss path is `create` then `put`, which
     * `GuardedManipulator` refuses. Without this check an encrypting write into a
     * freshly-created vault would fail deep inside the write with an opaque
     * internal error instead of being refused up front.
     */
    private async verifyE2EE(encryptedChunkId: string | undefined): Promise<Compatibility | undefined> {
        let syncParams: RawDoc | false;
        try {
            syncParams = await this.rawGet(SYNC_PARAMETERS_DOCID);
        } catch (error) {
            const { status, detail } = statusFromTransportError(error);
            return { status, detail };
        }
        const hasSalt = syncParams !== false && typeof syncParams["pbkdf2salt"] === "string";
        if (encryptedChunkId === undefined) {
            if (!hasSalt && this.mode === "read-write") {
                return {
                    status: "e2ee-invalid",
                    detail: `remote has no usable ${SYNC_PARAMETERS_DOCID} document, so the replication salt an encrypted write would need cannot be derived; a LiveSync client must connect once to establish it (this sidecar will not write it)`,
                };
            }
            // A passphrase for a vault with no encrypted chunks: harmless, and
            // possibly just an empty vault. Say so on stderr and continue.
            logStderr("compat", "a passphrase was supplied but the remote holds no encrypted chunks");
            return undefined;
        }
        if (!hasSalt) {
            return {
                status: "e2ee-invalid",
                detail: `remote has no usable ${SYNC_PARAMETERS_DOCID} document, so the replication salt cannot be derived; a LiveSync client must connect once to establish it (this sidecar will not write it)`,
            };
        }
        try {
            const row = await this.db().allDocs({
                startkey: encryptedChunkId,
                endkey: encryptedChunkId,
                include_docs: true,
                limit: 1,
            });
            const doc = row.rows[0]?.doc;
            if (!doc || typeof doc["data"] !== "string" || doc["data"] === "") {
                return {
                    status: "e2ee-invalid",
                    detail: "an encrypted chunk decrypted to empty content; the passphrase is probably wrong",
                };
            }
        } catch (error) {
            const { status, detail } = statusFromTransportError(error);
            if (status === "unknown") {
                return {
                    status: "e2ee-invalid",
                    detail: `an encrypted chunk could not be decrypted (${detail})`,
                };
            }
            return { status, detail };
        }
        return undefined;
    }

    /** `rawGet` that maps a 404 to `false` and lets everything else escape. */
    private async rawGet(id: string): Promise<RawDoc | false> {
        const manipulator = this.manipulator;
        if (!manipulator) throw new SidecarError("not-initialized", "initialize has not been called");
        const doc = await manipulator.rawGet<RawDoc>(id as never);
        return doc === false ? false : doc;
    }

    /** Existence probe for an id prefix. Metadata only -- never decrypts. */
    private async firstIdWithPrefix(prefix: string): Promise<string | undefined> {
        const result = await this.db().allDocs({
            startkey: prefix,
            endkey: `${prefix}${MAX_CHAR}`,
            limit: 1,
            include_docs: false,
        });
        return result.rows[0]?.id;
    }

    /* ---------------------------------------------------------------------- */
    /* Data methods                                                           */
    /* ---------------------------------------------------------------------- */

    /**
     * One page of vault metadata.
     *
     * Walks `NORMAL_DOC_RANGES` with `allDocs`, carrying `{range, lastId}` in
     * the cursor so a page boundary is resumable across requests. `conflicts:
     * true` is what makes `_conflicts` (hence `conflicted`) available at all.
     */
    async manifest(
        cursorRaw: string | null | undefined,
        limit: number
    ): Promise<{ entries: ManifestEntry[]; nextCursor?: string; exhausted: boolean }> {
        this.requireServeable();
        const db = this.db();
        let cursor = decodeManifestCursor(cursorRaw);
        const entries: ManifestEntry[] = [];

        while (cursor.r < NORMAL_DOC_RANGES.length) {
            const range = NORMAL_DOC_RANGES[cursor.r];
            if (!range) break;
            const [rangeStart, rangeEnd] = range;
            const startkey = cursor.k ?? rangeStart;
            const wanted = limit - entries.length;
            if (wanted <= 0) break;

            let response: AllDocsResponse;
            try {
                response = await db.allDocs({
                    startkey,
                    endkey: rangeEnd,
                    inclusive_end: false,
                    include_docs: true,
                    conflicts: true,
                    limit: wanted,
                    ...(cursor.k !== null ? { skip: 1 } : {}),
                });
            } catch (error) {
                throw remoteError(error);
            }

            if (response.rows.length === 0) {
                cursor = { r: cursor.r + 1, k: null };
                continue;
            }

            for (const row of response.rows) {
                cursor = { r: cursor.r, k: row.id };
                const doc = row.doc;
                if (!doc || typeof doc.type !== "string") continue;
                if (doc.type !== "plain" && doc.type !== "newnote" && doc.type !== "notes") continue;
                const path = this.pathOf(doc);
                if (!isVisiblePath(path)) continue;
                entries.push({
                    path,
                    size: doc.size ?? 0,
                    mtimeMs: doc.mtime ?? 0,
                    ctimeMs: doc.ctime ?? 0,
                    deleted: Boolean(doc.deleted ?? doc._deleted),
                    conflicted: (doc._conflicts?.length ?? 0) > 0,
                    kind: kindOfEntry(doc),
                });
            }

            if (response.rows.length < wanted) {
                // Short page: this range is drained.
                cursor = { r: cursor.r + 1, k: null };
            }
        }

        const exhausted = cursor.r >= NORMAL_DOC_RANGES.length;
        return {
            entries,
            ...(exhausted ? {} : { nextCursor: encodeManifestCursor(cursor) }),
            exhausted,
        };
    }

    async stat(path: string): Promise<StatResult> {
        const meta = await this.metaOf(path);
        return {
            path,
            size: meta.size ?? 0,
            mtimeMs: meta.mtime ?? 0,
            ctimeMs: meta.ctime ?? 0,
            deleted: Boolean(meta.deleted ?? meta._deleted),
            conflicted: (meta._conflicts?.length ?? 0) > 0,
            rev: meta._rev ?? "",
            kind: kindOfEntry(meta),
        };
    }

    /**
     * Assembles an entry's content from its chunks.
     *
     * `plain` (and legacy `notes`) chunks are text fragments and concatenate
     * directly.
     *
     * `newnote` chunks are base64 and must be decoded **one fragment at a time**,
     * then concatenated as bytes. Upstream's splitter cuts binary content at an
     * arbitrary *byte* offset and base64-encodes each piece independently
     * (`splitPiecesRabinKarp` -> `arrayBufferToBase64Single(subarray)`), so a
     * fragment whose byte length is not a multiple of three ends in `=` padding
     * *in the middle of the stream*. Concatenating the base64 first and decoding
     * once therefore truncates at the first interior `=`. Upstream's own reader
     * (`decodeBinary` -> `base64ToArrayBuffer(string[])`) decodes per fragment
     * and joins the buffers, and this mirrors it.
     *
     * The re-encoded output is canonical base64 of the whole content, so it is
     * not necessarily a concatenation of the stored fragments.
     */
    async read(path: string): Promise<ReadResult> {
        const manipulator = this.requireServeable();
        const meta = await this.metaOf(path);

        const common = {
            path,
            size: meta.size ?? 0,
            mtimeMs: meta.mtime ?? 0,
            ctimeMs: meta.ctime ?? 0,
            deleted: Boolean(meta.deleted ?? meta._deleted),
            conflicted: (meta._conflicts?.length ?? 0) > 0,
            rev: meta._rev ?? "",
        };

        // Legacy `notes` entries keep their whole content inline in `data` and
        // have no children. They cannot be read through `getByMeta`: upstream's
        // `getDBEntryMeta` rewrites their type to `plain` and drops `data`, so
        // `getDBEntryFromMeta`'s own legacy branch no longer recognises them and
        // the chunk path returns empty content. Detected by the absence of
        // children and served from the raw document instead.
        if ((meta.children?.length ?? 0) === 0) {
            const inline = await this.inlineDataOf(meta);
            if (inline !== undefined) {
                return { ...common, kind: "text", text: inline };
            }
            return { ...common, kind: "text", text: "" };
        }

        let loaded: LoadedEntryLike;
        try {
            loaded = (await manipulator.getByMeta(meta as never)) as unknown as LoadedEntryLike;
        } catch (error) {
            if (error instanceof SidecarError) throw error;
            const message = (error as Error)?.message ?? String(error);
            if (/corrupted document|load failed/i.test(message)) {
                throw new SidecarError(
                    "corrupted-document",
                    "one or more chunks of this entry are missing from the remote"
                );
            }
            throw remoteError(error);
        }

        const fragments = Array.isArray(loaded.data) ? loaded.data : [String(loaded.data ?? "")];
        if (meta.type === "newnote") {
            return { ...common, kind: "binary", base64: decodeChunkedBinary(fragments).toString("base64") };
        }
        return { ...common, kind: "text", text: fragments.join("") };
    }

    /**
     * Content of a childless entry, taken from the raw document.
     *
     * Returns `undefined` for an entry that genuinely has no inline data (an
     * empty file), which the caller renders as empty text.
     */
    private async inlineDataOf(meta: RawDoc): Promise<string | undefined> {
        let raw: RawDoc | false;
        try {
            raw = await this.rawGet(meta._id);
        } catch (error) {
            throw remoteError(error);
        }
        if (raw === false) return undefined;
        const data = raw["data"];
        if (typeof data === "string") return data;
        if (Array.isArray(data)) return data.join("");
        return undefined;
    }

    /**
     * Metadata for one path, including soft-deleted entries and conflict revs.
     *
     * `getDBEntryMeta` is one level below `DirectFileManipulator.get` because
     * only it accepts PouchDB get options (`conflicts: true`) and the
     * include-deleted flag.
     */
    private async metaOf(path: string): Promise<RawDoc> {
        const manipulator = this.requireServeable();
        if (!isVisiblePath(path)) {
            throw new SidecarError("not-found", "path is not visible to the sidecar");
        }
        let meta: unknown;
        try {
            meta = await manipulator.liveSyncLocalDB.getDBEntryMeta(
                path as never,
                { conflicts: true } as never,
                true
            );
        } catch (error) {
            throw remoteError(error);
        }
        if (meta === false || meta === undefined || meta === null) {
            throw new SidecarError("not-found", "no entry at this path");
        }
        return meta as RawDoc;
    }

    /* ---------------------------------------------------------------------- */
    /* Write methods                                                          */
    /* ---------------------------------------------------------------------- */

    /**
     * Compare-and-swap write of one entry.
     *
     * Publication order is upstream's, and it matters: every leaf chunk is
     * written (`POST /{db}/_bulk_docs`) before the entry root
     * (`PUT /{db}/{id}`). An interrupted write therefore leaves *orphan chunks*,
     * never a root pointing at chunks that do not exist. Orphans are inert --
     * chunk ids are content-addressed, so a retry re-derives the same ids and the
     * duplicate writes come back 409 and are counted as "duplicated" by
     * upstream's own write layer. See the README for the orphan policy.
     *
     * The CAS precondition is evaluated against a fresh read *and* enforced by
     * CouchDB: the pre-read produces a good error message, the rev-guarded PUT is
     * what actually makes concurrent writers safe.
     */
    async write(params: WriteParams): Promise<WriteResult> {
        const manipulator = this.requireWritable();
        const path = params.path;
        if (!isVisiblePath(path)) {
            throw new SidecarError(
                "invalid-params",
                "this path is not writable through the sidecar (paths containing ':' and dot-paths are excluded, matching commonlib's isTargetFile)"
            );
        }

        let id: string;
        try {
            id = String(await manipulator.path2id(path as never));
        } catch (error) {
            throw remoteError(error);
        }

        const current = await this.currentState(id);

        // The CAS precondition. `baseRev === null` is create-only, a string is a
        // guarded update, absent is an unguarded upsert.
        let guardRev: string | undefined;
        if (params.baseRev === null) {
            if (current) {
                throw SidecarError.conflict(
                    current.deleted
                        ? "create-only write refused: a soft-deleted entry still occupies this path (write with its rev, or unguarded, to resurrect it)"
                        : "create-only write refused: an entry already exists at this path",
                    conflictDetailOf(current, null)
                );
            }
            guardRev = undefined;
        } else if (typeof params.baseRev === "string") {
            if (!current) {
                throw SidecarError.conflict(
                    "guarded write refused: no entry exists at this path any more",
                    { expected: params.baseRev }
                );
            }
            if (current.rev !== params.baseRev) {
                throw SidecarError.conflict(
                    "guarded write refused: the remote revision moved",
                    conflictDetailOf(current, params.baseRev)
                );
            }
            guardRev = current.rev;
        } else {
            guardRev = current?.rev;
        }

        const mtime = params.mtimeMs ?? Date.now();
        const ctime = params.ctimeMs ?? current?.ctimeMs ?? mtime;
        const payload =
            params.content.kind === "text"
                ? [params.content.text]
                : // `application/octet-stream` is what makes upstream classify the
                  // entry as `newnote` and base64 its chunks; `text/plain` would
                  // silently store binary as `plain`. Mirrors `createBinaryBlob`.
                  new Blob([decodeBase64(params.content.base64)], { type: "application/octet-stream" });
        const size =
            params.content.kind === "text"
                ? Buffer.byteLength(params.content.text, "utf8")
                : decodeBase64(params.content.base64).byteLength;

        const outcome = await this.withGuardedRootPut(guardRev, () =>
            manipulator.put(path as never, payload as never, { ctime, mtime, size })
        );

        if (outcome.conflicted) {
            const now = await this.currentState(id).catch(() => undefined);
            throw SidecarError.conflict(
                "write refused: CouchDB rejected the entry revision (a concurrent writer won)",
                now ? conflictDetailOf(now, params.baseRev) : { expected: params.baseRev }
            );
        }
        if (outcome.error !== undefined) {
            throw remoteError(outcome.error);
        }
        if (outcome.accepted === false) {
            // `putDBEntry` reports a refusal or a chunk-write failure as `false`
            // without an exception. Nothing has been rooted, so the vault is
            // unchanged apart from possibly-orphaned chunks.
            throw new SidecarError(
                "remote-error",
                "the remote refused the write: chunks could not be stored, or commonlib declined the path"
            );
        }
        if (outcome.rootPuts !== 1) {
            // The interceptor is the only thing standing between this method and
            // upstream's unconditional force-write. If it did not fire exactly
            // once, compare-and-swap was NOT enforced, and pretending otherwise
            // would be the worst possible failure.
            throw new SidecarError(
                "internal-error",
                `upstream no longer routes the entry root write through localDatabase.put (observed ${outcome.rootPuts} guarded puts); compare-and-swap could not be enforced, so this write is reported as failed`
            );
        }
        const rev = outcome.rev;
        if (rev === undefined || rev === "") {
            throw new SidecarError("internal-error", "the entry write returned no revision");
        }

        return {
            path,
            rev,
            // A rev-guarded write targets the winning revision, so it neither
            // creates nor resolves conflicts: the pre-read verdict still holds
            // and reporting it costs no extra round trip.
            conflicted: current?.conflicted ?? false,
            size,
            mtimeMs: mtime,
            ctimeMs: ctime,
            kind: params.content.kind === "binary" ? "binary" : "markdown",
            created: current === undefined,
            resurrected: current?.deleted ?? false,
        };
    }

    /**
     * Soft delete, matching the plugin's default.
     *
     * Re-implemented rather than delegated to `DirectFileManipulator.delete`,
     * which force-puts. The document is read back and edited in place, so an
     * obfuscated entry survives the round trip: the outgoing transform decrypts
     * `path`/`children`/`mtime`/`size` on the way in and the incoming transform
     * re-encrypts them on the way out. Chunks are deliberately left behind --
     * LiveSync deletions are recoverable.
     */
    async remove(path: string, baseRev?: string | null): Promise<DeleteResult> {
        this.requireWritable();
        if (!isVisiblePath(path)) {
            throw new SidecarError("invalid-params", "this path is not writable through the sidecar");
        }
        const manipulator = this.requireServeable();
        let id: string;
        try {
            id = String(await manipulator.path2id(path as never));
        } catch (error) {
            throw remoteError(error);
        }

        let doc: RawDoc;
        try {
            doc = await this.db().get(id, { conflicts: true });
        } catch (error) {
            if (isMissingDocError(error)) {
                throw new SidecarError("not-found", "no entry at this path");
            }
            throw remoteError(error);
        }
        if (doc.type === "leaf") {
            throw new SidecarError("not-found", "no entry at this path");
        }
        const state = stateOf(doc);
        if (typeof baseRev === "string" && state.rev !== baseRev) {
            throw SidecarError.conflict(
                "guarded delete refused: the remote revision moved",
                conflictDetailOf(state, baseRev)
            );
        }

        const next = stripReadOnlyMeta(doc);
        next.deleted = true;
        // Upstream bumps mtime on delete so replicas order the deletion after
        // the last edit. It also sets `_deleted` when
        // `deleteMetadataOfDeletedFiles` is on; the sidecar never does, in any
        // mode: a CouchDB tombstone leaves `_all_docs` and cannot be listed or
        // recovered.
        next.mtime = Date.now();

        let response: PutResponse;
        try {
            response = await this.db().put(next);
        } catch (error) {
            if (isConflictError(error)) {
                const now = await this.currentState(id).catch(() => undefined);
                throw SidecarError.conflict(
                    "delete refused: CouchDB rejected the entry revision (a concurrent writer won)",
                    now ? conflictDetailOf(now, baseRev) : { expected: baseRev }
                );
            }
            throw remoteError(error);
        }
        return { path, rev: String(response.rev ?? ""), deleted: true };
    }

    /**
     * Enumerates the entry's conflict revisions.
     *
     * Read-only, hence available in both modes. Each losing revision is fetched
     * by `rev` for its metadata; a revision CouchDB has already compacted away is
     * reported with `unavailable: true` rather than dropped, so a host never
     * silently under-reports a conflict.
     */
    async conflicts(path: string): Promise<ConflictsResult> {
        const meta = await this.metaOf(path);
        const revs = meta._conflicts ?? [];
        const conflicts: ConflictRevision[] = [];
        for (const rev of revs) {
            let doc: RawDoc | undefined;
            try {
                doc = await this.db().get(meta._id, { rev });
            } catch (error) {
                logStderr("conflicts", `conflict revision could not be read: ${describe(error)}`);
            }
            if (!doc) {
                conflicts.push({ rev, mtimeMs: 0, size: 0, deleted: false, unavailable: true });
                continue;
            }
            conflicts.push({
                rev,
                mtimeMs: doc.mtime ?? 0,
                size: doc.size ?? 0,
                deleted: Boolean(doc.deleted ?? doc._deleted),
            });
        }
        return { path, winning: meta._rev ?? "", conflicts };
    }

    /**
     * Current state of an entry document, or `undefined` when there is none.
     *
     * Goes through `localDatabase.get`, i.e. through the decryption transform, so
     * an obfuscated entry reports its real `mtime`/`size` rather than the zeros
     * the remote physically stores.
     */
    private async currentState(id: string): Promise<EntryState | undefined> {
        try {
            return stateOf(await this.db().get(id, { conflicts: true }));
        } catch (error) {
            if (isMissingDocError(error)) return undefined;
            throw remoteError(error);
        }
    }

    /**
     * Runs `body` with the entry-root write intercepted and re-issued under a
     * revision guard.
     *
     * This is the whole compare-and-swap mechanism. Upstream calls
     * `localDatabase.put(rootDoc, {force: true})`, which PouchDB rewrites into a
     * `new_edits: false` graft that can never conflict. The interceptor drops
     * `force`, substitutes the revision the precondition validated, and lets
     * CouchDB answer. Everything that is not an entry root -- chunk `bulkDocs`
     * never comes through `put` at all, but `_local` control documents would --
     * keeps upstream's own semantics untouched.
     *
     * Safe to install per-call because `main.ts` serialises every request, so
     * exactly one write is ever in flight. Restored in `finally` regardless.
     *
     * The 409 is captured *here* rather than being caught after `body` rejects:
     * it has to cross `chunkManager.transaction` and `serialized` frames, both of
     * which are free to change how they propagate, and losing the discriminator
     * would turn a conflict into a generic remote error.
     */
    private async withGuardedRootPut(
        guardRev: string | undefined,
        body: () => Promise<boolean>
    ): Promise<{
        accepted: boolean | undefined;
        rev: string | undefined;
        rootPuts: number;
        conflicted: boolean;
        error: unknown;
    }> {
        const db = this.db();
        const originalPut = db.put;
        let rootPuts = 0;
        let rev: string | undefined;
        let conflicted = false;

        db.put = async (doc: RawDoc, options?: Record<string, unknown>): Promise<PutResponse> => {
            if (!isEntryRoot(doc)) {
                return await originalPut.call(db, doc, options);
            }
            rootPuts += 1;
            const guarded: RawDoc = { ...doc };
            if (guardRev === undefined) {
                delete guarded._rev;
            } else {
                guarded._rev = guardRev;
            }
            try {
                const response = await originalPut.call(db, guarded, {});
                rev = response.rev;
                return response;
            } catch (error) {
                if (isConflictError(error)) {
                    conflicted = true;
                }
                throw error;
            }
        };

        let accepted: boolean | undefined;
        let error: unknown;
        try {
            accepted = await body();
        } catch (caught) {
            error = caught;
        } finally {
            db.put = originalPut;
        }
        // A captured 409 outranks whatever the upstream frames turned it into.
        if (conflicted) {
            return { accepted, rev, rootPuts, conflicted: true, error: undefined };
        }
        return { accepted, rev, rootPuts, conflicted: false, error };
    }

    /**
     * One-shot change feed.
     *
     * Not `followUpdates()`: that requests the `replicate/pull` filter, i.e. a
     * `_design/replicate` document that neither the plugin nor commonlib ever
     * creates, so it 404s against a real vault. The cursor is PouchDB's `since`
     * token, stringified and treated as opaque by the host.
     */
    async changesSince(
        cursor: string | null | undefined,
        limit: number
    ): Promise<{ changes: ChangeEntry[]; nextCursor: string; exhausted: boolean }> {
        this.requireServeable();
        const since = cursor === undefined || cursor === null || cursor === "" ? "0" : cursor;
        let response: ChangesResponse;
        try {
            response = await this.db().changes({
                since,
                live: false,
                include_docs: true,
                return_docs: true,
                limit,
            });
        } catch (error) {
            throw remoteError(error);
        }
        const changes: ChangeEntry[] = [];
        for (const row of response.results) {
            const doc = row.doc;
            if (!doc) continue;
            // Chunk documents dominate a LiveSync feed; drop them here rather
            // than with a server-side selector, which would need a Mango
            // filter round trip on every poll.
            if (doc.type !== "plain" && doc.type !== "newnote" && doc.type !== "notes") continue;
            const path = this.pathOf(doc);
            if (!isVisiblePath(path)) continue;
            changes.push({
                path,
                deleted: Boolean(doc.deleted ?? doc._deleted ?? row.deleted),
                kind: kindOfEntry(doc),
            });
        }
        return {
            changes,
            nextCursor: String(response.last_seq),
            exhausted: response.results.length < limit,
        };
    }

    /**
     * Subscribes to the live feed.
     *
     * Uses upstream's `beginWatch`, which loads each changed entry's full
     * content before invoking the callback even though the notification only
     * carries metadata. That is a wasted fetch per change and it means an entry
     * whose chunks are missing is silently skipped -- both accepted here to
     * stay on the public API; revisit if it shows up as latency in 3c.
     *
     * Re-subscribing needs care. `beginWatch` refuses with `false` while
     * upstream's own `watching` flag is set, and that flag is only cleared in
     * the change feed's asynchronous `"complete"` handler -- which fires *after*
     * `endWatch()` returns. So an immediate watch/unwatch/watch cycle (exactly
     * what a supervisor reconnect loop does) would otherwise return
     * `{watching: true}` with nothing actually subscribed. The refusal is
     * checked, and the settle is waited for first.
     */
    async watch(onChange: (change: ChangeEntry & { cursor: string }) => void): Promise<string> {
        const manipulator = this.requireServeable();
        if (this.watching) {
            return String(manipulator.since);
        }
        await this.awaitWatchSettled(manipulator);
        let startSeq = "0";
        try {
            const info = await this.db().info();
            startSeq = String(info.update_seq ?? "0");
        } catch (error) {
            throw remoteError(error);
        }
        manipulator.since = startSeq;
        this.watchCallback = onChange;
        const accepted = manipulator.beginWatch((doc: unknown, seq?: string | number) => {
            const raw = doc as RawDoc;
            const path = this.pathOf(raw);
            if (!isVisiblePath(path)) return;
            this.watchCallback?.({
                path,
                deleted: Boolean(raw.deleted ?? raw._deleted),
                kind: kindOfEntry(raw),
                cursor: String(seq ?? ""),
            });
        });
        if (accepted === false) {
            // Reporting success here would be a lie the host cannot detect.
            this.watchCallback = undefined;
            throw new SidecarError(
                "remote-error",
                "the previous live feed has not finished closing; retry the watch"
            );
        }
        this.watching = true;
        return startSeq;
    }

    /**
     * Waits for upstream's `watching` flag to clear after a cancellation.
     *
     * Bounded: if the feed never reports completion the caller gets a typed
     * refusal from `beginWatch` rather than a hang.
     */
    private async awaitWatchSettled(manipulator: GuardedManipulator): Promise<void> {
        const deadline = Date.now() + 3_000;
        while (manipulator.watching && Date.now() < deadline) {
            await new Promise((resolve) => {
                const timer = setTimeout(resolve, 25);
                timer.unref();
            });
        }
    }

    unwatch(): void {
        if (!this.watching) return;
        this.watching = false;
        this.watchCallback = undefined;
        try {
            this.manipulator?.endWatch();
        } catch (error) {
            logStderr("watch", `endWatch failed: ${(error as Error)?.message ?? String(error)}`);
        }
    }

    /**
     * `_id` to vault-relative path.
     *
     * `id2path` with `stripPrefix` handles both the obfuscated (`f:`) case,
     * where the plaintext path only exists in the decrypted `path` field, and
     * the plain case, where the id *is* the path.
     */
    private pathOf(doc: RawDoc): string {
        const manipulator = this.manipulator;
        if (manipulator) {
            try {
                return String(manipulator.$$id2path(doc._id as never, doc as never, true));
            } catch {
                /* fall through to the raw field */
            }
        }
        return String(doc.path ?? doc._id);
    }

    async close(): Promise<void> {
        this.unwatch();
        const manipulator = this.manipulator;
        this.manipulator = undefined;
        if (!manipulator) return;
        try {
            await withTimeout(manipulator.close(), 5_000, "closing the remote database timed out");
        } catch (error) {
            logStderr("shutdown", `close failed: ${(error as Error)?.message ?? String(error)}`);
        }
    }
}

/* -------------------------------------------------------------------------- */
/* Small utilities                                                             */
/* -------------------------------------------------------------------------- */

function remoteError(error: unknown): SidecarError {
    if (error instanceof SidecarError) return error;
    const { detail } = statusFromTransportError(error);
    return new SidecarError("remote-error", detail);
}

function describe(error: unknown): string {
    return (error as Error)?.message ?? String(error);
}

/**
 * Bytes behind a `newnote` entry's base64 fragments.
 *
 * Per-fragment decoding, matching upstream's `decodeBinary`. See `read`.
 *
 * The `%` prefix is upstream's pre-base64 encoding (`_decodeToArrayBuffer`, a
 * UTF-16 packing). Upstream still reads it for old documents but states it
 * "always uses base64" now, and this sidecar does not implement it: refusing is
 * the only honest option, because guessing would hand a caller silently wrong
 * bytes. (Encrypted chunks also start with `%`, but the decryption transform has
 * already run by the time content reaches here.)
 */
function decodeChunkedBinary(fragments: readonly string[]): Buffer {
    if (fragments[0]?.startsWith("%")) {
        throw new SidecarError(
            "corrupted-document",
            "this attachment uses upstream's pre-base64 chunk encoding, which the sidecar does not decode"
        );
    }
    return Buffer.concat(fragments.map((fragment) => Buffer.from(fragment, "base64")));
}

/* -------------------------------------------------------------------------- */
/* Write-path helpers                                                          */
/* -------------------------------------------------------------------------- */

/** The subset of an entry document a CAS precondition needs. */
type EntryState = {
    rev: string;
    deleted: boolean;
    conflicted: boolean;
    mtimeMs: number;
    ctimeMs: number;
    size: number;
};

function stateOf(doc: RawDoc): EntryState {
    return {
        rev: doc._rev ?? "",
        deleted: Boolean(doc.deleted ?? doc._deleted),
        conflicted: (doc._conflicts?.length ?? 0) > 0,
        mtimeMs: doc.mtime ?? 0,
        ctimeMs: doc.ctime ?? 0,
        size: doc.size ?? 0,
    };
}

function conflictDetailOf(state: EntryState, expected: string | null | undefined): ConflictDetail {
    return {
        currentRev: state.rev,
        ...(expected !== undefined ? { expected } : {}),
        deleted: state.deleted,
        conflicted: state.conflicted,
        mtimeMs: state.mtimeMs,
        size: state.size,
    };
}

/**
 * Is this the document that roots an entry (as opposed to a chunk or a control
 * document)?
 *
 * `leaf` chunks reach CouchDB through `bulkDocs`, never `put`, so in practice the
 * only other traffic on `put` is `_local/` control documents -- which must keep
 * upstream's semantics, and which `GuardedManipulator` refuses anyway.
 */
function isEntryRoot(doc: RawDoc): boolean {
    if (typeof doc._id !== "string" || doc._id.startsWith("_local/")) return false;
    return doc.type === "plain" || doc.type === "newnote" || doc.type === "notes";
}

/**
 * CouchDB rejects a document body carrying unknown underscore-prefixed fields,
 * and `_conflicts` / `_revs_info` / `_revisions` are exactly the ones a `get`
 * with options adds. Stripping them is what makes read-modify-write legal.
 */
function stripReadOnlyMeta(doc: RawDoc): RawDoc {
    const copy: RawDoc = { ...doc };
    delete copy._conflicts;
    delete copy["_revs_info"];
    delete copy["_revisions"];
    return copy;
}

function isConflictError(error: unknown): boolean {
    const anyError = error as { status?: number; name?: string; error?: string; message?: string };
    if (anyError?.status === 409) return true;
    return anyError?.name === "conflict" || anyError?.error === "conflict";
}

function isMissingDocError(error: unknown): boolean {
    const anyError = error as { status?: number; name?: string; reason?: string };
    if (anyError?.status === 404) return true;
    return anyError?.name === "not_found";
}

/**
 * Base64 to bytes, strictly.
 *
 * `Buffer.from(x, "base64")` silently ignores anything it cannot parse, so a
 * caller's typo would be stored as truncated content. Round-tripping the decode
 * catches that at the boundary instead.
 */
function decodeBase64(base64: string): Buffer {
    const bytes = Buffer.from(base64, "base64");
    if (bytes.toString("base64") !== normaliseBase64(base64)) {
        throw new SidecarError("invalid-params", "content.base64 is not valid base64");
    }
    return bytes;
}

function normaliseBase64(base64: string): string {
    const compact = base64.replace(/\s+/g, "").replace(/-/g, "+").replace(/_/g, "/").replace(/=+$/, "");
    const padding = compact.length % 4 === 0 ? "" : "=".repeat(4 - (compact.length % 4));
    return `${compact}${padding}`;
}

/**
 * Wraps `fetch` with a per-request timeout.
 *
 * PouchDB's HTTP adapter has no timeout of its own, so a black-holed CouchDB
 * would hang a request forever and, with it, the single-threaded protocol loop.
 * The live change feed is exempt: it is a long-poll that is *supposed* to stay
 * open.
 */
function makeTimeoutFetch(timeoutMs: number): typeof globalThis.fetch {
    type FetchInput = Parameters<typeof globalThis.fetch>[0];
    type FetchInit = Parameters<typeof globalThis.fetch>[1];
    return ((input: FetchInput, init?: FetchInit): Promise<Response> => {
        const url =
            typeof input === "string"
                ? input
                : input instanceof URL
                  ? input.href
                  : (input as { url: string }).url;
        const isFeed = /[?&]feed=/.test(url);
        if (isFeed || init?.signal) {
            return globalThis.fetch(input as never, init as never);
        }
        const controller = new AbortController();
        const timer = setTimeout(() => controller.abort(), timeoutMs);
        return globalThis
            .fetch(input as never, { ...(init ?? {}), signal: controller.signal } as never)
            .finally(() => clearTimeout(timer));
    }) as typeof globalThis.fetch;
}

function withTimeout<T>(promise: Promise<T>, ms: number, message: string): Promise<T> {
    return new Promise<T>((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error(message)), ms);
        promise.then(
            (value) => {
                clearTimeout(timer);
                resolve(value);
            },
            (error) => {
                clearTimeout(timer);
                reject(error);
            }
        );
    });
}
