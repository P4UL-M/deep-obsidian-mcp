/**
 * The versioned sidecar protocol.
 *
 * Wire format: JSON-RPC 2.0 objects, one per line ("newline-delimited JSON"),
 * UTF-8, over the child process's stdin/stdout. Requests carry `id`; the
 * sidecar's unsolicited `change` messages are JSON-RPC notifications (no `id`).
 * Nothing else is ever written to stdout -- all logging goes to stderr.
 *
 * This file is the contract. The Rust supervisor codes against these shapes,
 * so every change here is a protocol change: bump `PROTOCOL_VERSION` for
 * anything a v1 host could not tolerate, and keep additive fields optional.
 *
 * ## Versioning rule: additive methods do NOT bump the version
 *
 * `PROTOCOL_VERSION` describes what a host must *tolerate*, not what the
 * sidecar happens to offer. The supervisor pins the `SUPPORTED` triple exactly
 * and refuses any drift in it, so the version number is load-bearing in one
 * direction only: it must change when an existing shape changes meaning.
 *
 * Adding a *method* cannot break a v1 host -- a host that does not know `write`
 * never sends it, and one that does gets `method-not-found` from an older
 * sidecar, which is already a modelled failure. Adding an *optional request
 * field* with a backwards-compatible default (`initialize.mode`, which defaults
 * to `"read-only"`, i.e. exactly v1 behaviour) is likewise invisible to a host
 * that omits it. Adding a *response field* is safe because neither side
 * deserialises strictly.
 *
 * So the write surface below is additive on protocol version 1. What WOULD
 * require a bump: changing `SUPPORTED`'s shape, changing an existing method's
 * params or result, changing an error code's meaning, or flipping
 * `initialize.mode`'s default. Under no circumstances edit `SUPPORTED` to
 * advertise a capability -- the supervisor asserts that object field by field.
 */

/** Wire version of this protocol. Bumped on any breaking change. */
export const PROTOCOL_VERSION = 1;

/** Version of the sidecar package itself (kept in step with package.json). */
export const SIDECAR_VERSION = "0.1.0";

/**
 * The pinning surface the Rust supervisor enforces.
 *
 * `commonlibVersion` must equal the exact version resolved at build time (the
 * dependency is pinned without a caret and the lockfile freezes it), because
 * upstream is pre-1.0 and explicitly documents its semantics as "not final".
 * `maxSchemaVersion` is upstream's `VER` constant; a remote database declaring
 * a higher `obsydian_livesync_version` is refused rather than guessed at.
 */
export const SUPPORTED = {
    protocolVersion: PROTOCOL_VERSION,
    commonlibVersion: "0.1.2",
    maxSchemaVersion: 12,
    pluginVersionTested: "1.0.3",
} as const;

export type Supported = typeof SUPPORTED;

/* -------------------------------------------------------------------------- */
/* JSON-RPC envelopes                                                          */
/* -------------------------------------------------------------------------- */

export type JsonRpcId = string | number;

export type JsonRpcRequest = {
    jsonrpc: "2.0";
    id: JsonRpcId;
    method: string;
    params?: unknown;
};

export type JsonRpcSuccess = {
    jsonrpc: "2.0";
    id: JsonRpcId;
    result: unknown;
};

export type JsonRpcErrorBody = {
    code: number;
    message: string;
    data?: JsonRpcErrorData;
};

export type JsonRpcFailure = {
    jsonrpc: "2.0";
    id: JsonRpcId | null;
    error: JsonRpcErrorBody;
};

export type JsonRpcNotification = {
    jsonrpc: "2.0";
    method: string;
    params: unknown;
};

/**
 * Machine-readable error detail. `kind` is the stable discriminator; hosts
 * should branch on it rather than on `code` or `message`.
 */
export type JsonRpcErrorData = {
    kind: ErrorKind;
    /** Human-readable, already redacted. Never contains secrets or raw paths. */
    detail?: string;
    /** Present when the failure is a compatibility refusal. */
    status?: CompatibilityStatus;
    /**
     * Present iff `kind` is `"conflict"`. Everything a host needs to retry a
     * compare-and-swap without a second round trip.
     */
    conflict?: ConflictDetail;
};

/**
 * Why a guarded write lost, and what the remote looks like now.
 *
 * `currentRev` is the winning revision at the moment the conflict was detected;
 * it is absent only when the document does not exist at all (a guarded write
 * against a path that has since been purged). `expected` echoes what the caller
 * asked for so a log line is self-contained: `null` means create-only.
 */
export type ConflictDetail = {
    currentRev?: string;
    expected?: string | null;
    /** The remote entry is soft-deleted (`deleted: true`), not absent. */
    deleted?: boolean;
    /** The remote entry already has sibling conflict revisions. */
    conflicted?: boolean;
    mtimeMs?: number;
    size?: number;
};

/* -------------------------------------------------------------------------- */
/* Error codes                                                                 */
/* -------------------------------------------------------------------------- */

/** Standard JSON-RPC codes plus the sidecar's application range. */
export const ErrorCodes = {
    parseError: -32700,
    invalidRequest: -32600,
    methodNotFound: -32601,
    invalidParams: -32602,
    internalError: -32603,

    /** A data method was called before a successful `initialize`. */
    notInitialized: -32000,
    /** `initialize.protocolVersion` is not `PROTOCOL_VERSION`. */
    unsupportedProtocolVersion: -32001,
    /** `initialize` called twice on the same process. */
    alreadyInitialized: -32002,
    /** `initialize` succeeded but reported a compatibility status other than "ok". */
    incompatibleRemote: -32003,
    /** No entry at the requested path (or the path is not vault-visible). */
    notFound: -32004,
    /** CouchDB/transport failure while serving the request. */
    remoteError: -32005,
    /** Chunk decryption failed (wrong passphrase, or salt unavailable). */
    decryptFailed: -32006,
    /** The entry exists but one or more of its chunks could not be assembled. */
    corruptedDocument: -32007,
    /** A guarded write lost: the remote revision is not the one the caller expected. */
    conflict: -32008,
    /** A write method was called on a sidecar initialized `mode: "read-only"`. */
    readOnly: -32009,
} as const;

export type ErrorKind =
    | "parse-error"
    | "invalid-request"
    | "method-not-found"
    | "invalid-params"
    | "internal-error"
    | "not-initialized"
    | "unsupported-protocol-version"
    | "already-initialized"
    | "incompatible-remote"
    | "not-found"
    | "remote-error"
    | "decrypt-failed"
    | "corrupted-document"
    | "conflict"
    | "read-only";

export const ERROR_CODE_BY_KIND: Record<ErrorKind, number> = {
    "parse-error": ErrorCodes.parseError,
    "invalid-request": ErrorCodes.invalidRequest,
    "method-not-found": ErrorCodes.methodNotFound,
    "invalid-params": ErrorCodes.invalidParams,
    "internal-error": ErrorCodes.internalError,
    "not-initialized": ErrorCodes.notInitialized,
    "unsupported-protocol-version": ErrorCodes.unsupportedProtocolVersion,
    "already-initialized": ErrorCodes.alreadyInitialized,
    "incompatible-remote": ErrorCodes.incompatibleRemote,
    "not-found": ErrorCodes.notFound,
    "remote-error": ErrorCodes.remoteError,
    "decrypt-failed": ErrorCodes.decryptFailed,
    "corrupted-document": ErrorCodes.corruptedDocument,
    conflict: ErrorCodes.conflict,
    "read-only": ErrorCodes.readOnly,
};

/* -------------------------------------------------------------------------- */
/* Compatibility                                                               */
/* -------------------------------------------------------------------------- */

/**
 * Outcome of the pre-serve compatibility gate.
 *
 * Only `"ok"` unlocks the data methods. Every other value is reported as a
 * *successful* `initialize` result so the supervisor can surface a precise
 * reason to the user; subsequent data calls then fail with
 * `incompatible-remote`.
 */
export type CompatibilityStatus =
    /** Remote is readable at a schema version we support. */
    | "ok"
    /** Milestone chunk-version ranges leave no version this client can read. */
    | "incompatible"
    /** Remote's preferred tweak values disagree with the options we were given. */
    | "mismatched"
    /** Milestone `locked` is set: the vault is mid-rebuild, do not read. */
    | "locked"
    /** Milestone `locked` + `cleaned`: chunks were purged, a resync is required. */
    | "cleaned"
    /** `obsydian_livesync_version` is missing, malformed, or newer than we support. */
    | "unknown-schema"
    /** CouchDB rejected the credentials (401/403). */
    | "auth-failed"
    /** CouchDB could not be reached (DNS, refused, timeout, TLS). */
    | "unreachable"
    /** Encrypted chunks are present but no passphrase was supplied. */
    | "e2ee-required"
    /** A passphrase was supplied but cannot decrypt the remote. */
    | "e2ee-invalid"
    /** Classification failed for a reason we do not model. */
    | "unknown";

export type Compatibility = {
    status: CompatibilityStatus;
    /** Redacted explanation, safe to log and to show a user. */
    detail?: string;
};

/* -------------------------------------------------------------------------- */
/* initialize                                                                  */
/* -------------------------------------------------------------------------- */

/**
 * Tuning knobs forwarded to commonlib. All optional; the defaults are
 * upstream's own defaults. These must match how the vault was written, which
 * is why the remote's milestone tweak values are cross-checked (see
 * `"mismatched"`).
 */
export type InitializeOptions = {
    customChunkSize?: number;
    minimumChunkSize?: number;
    hashAlg?: string;
    useEden?: boolean;
    enableCompression?: boolean;
    handleFilenameCaseSensitive?: boolean;
    chunkSplitterVersion?: number;
    e2eeAlgorithm?: string;
    /** Per-HTTP-request timeout applied to the CouchDB transport. Default 30000. */
    requestTimeoutMs?: number;
};

/**
 * What the sidecar is allowed to do to the remote.
 *
 * `"read-only"` is the default and the only value a v1 host ever sends, so an
 * omitted `mode` is exactly v1 behaviour. `"read-write"` unlocks `write` and
 * `delete` and nothing else: the milestone document, the version document and
 * the sync-parameters document stay unwritten in BOTH modes (see
 * `manipulator.ts`). A writer is still not a LiveSync peer.
 */
export type SidecarMode = "read-only" | "read-write";

export const SIDECAR_MODES: readonly SidecarMode[] = ["read-only", "read-write"];

/**
 * `initialize` is the ONLY place secrets cross the process boundary: never
 * argv (visible in `ps`), never the environment (inherited by children and
 * often captured by crash reporters).
 */
export type InitializeParams = {
    protocolVersion: number;
    /** Defaults to `"read-only"`. */
    mode?: SidecarMode;
    couchdb: {
        /** Server origin, without the database path. E.g. `https://couch.example`. */
        url: string;
        database: string;
        username: string;
        password: string;
    };
    e2ee?: {
        passphrase: string;
        /** Set only when the vault has path obfuscation enabled. */
        obfuscatePassphrase?: string;
    };
    options?: InitializeOptions;
};

export type InitializeResult = {
    protocolVersion: number;
    /** Echoed back so a host never has to remember what it asked for. */
    mode: SidecarMode;
    sidecarVersion: string;
    commonlibVersion: string;
    supportedSchemaVersion: number;
    /** The full pinning triple, echoed so the supervisor can assert on it. */
    supported: Supported;
    compatibility: Compatibility;
    remote: {
        /** `obsydian_livesync_version.version`, absent when the doc is missing. */
        schemaVersion?: number;
        /** Encrypted chunks (`h:+`) were observed in the remote. */
        encrypted: boolean;
        /** Obfuscated entry ids (`f:`) were observed in the remote. */
        pathObfuscation: boolean;
    };
};

/* -------------------------------------------------------------------------- */
/* Vault entries                                                               */
/* -------------------------------------------------------------------------- */

/**
 * How the sidecar classifies an entry.
 *
 * `"markdown"` is really "stored as text" -- it maps to LiveSync's `plain`
 * (and legacy `notes`) entry type, which the plugin assigns to anything it
 * decided was text, not only `.md`. `"binary"` maps to `newnote`, whose chunks
 * are base64. `"internal"` covers `i:`-prefixed hidden-file entries, which the
 * manifest does not list (see README).
 */
export type EntryKind = "markdown" | "binary" | "internal";

export type ManifestEntry = {
    /** Vault-relative path with LiveSync prefixes stripped. */
    path: string;
    size: number;
    mtimeMs: number;
    ctimeMs: number;
    /** LiveSync's soft delete: the entry document carries `deleted: true`. */
    deleted: boolean;
    /** The document has sibling `_conflicts` revisions; the winner is served. */
    conflicted: boolean;
    kind: EntryKind;
};

/* -------------------------------------------------------------------------- */
/* manifest                                                                    */
/* -------------------------------------------------------------------------- */

export type ManifestParams = {
    /**
     * Must be `true` or omitted. The manifest is metadata-only by construction;
     * `false` is rejected rather than silently ignored.
     */
    metaOnly?: true;
    /** Opaque continuation token from a previous `nextCursor`. */
    cursor?: string | null;
    /** Page size. Default 500, clamped to 2000. */
    limit?: number;
};

export type ManifestResult = {
    entries: ManifestEntry[];
    /** Present iff `exhausted` is false. Opaque: pass back verbatim. */
    nextCursor?: string;
    /**
     * True when this page was the last one.
     *
     * `entries` may be EMPTY while this is false: a page's budget can be spent
     * entirely on documents that are filtered out. Drive the loop on
     * `exhausted`, never on `entries.length`.
     */
    exhausted: boolean;
};

/* -------------------------------------------------------------------------- */
/* read / stat                                                                 */
/* -------------------------------------------------------------------------- */

export type ReadParams = { path: string };

type ReadCommon = {
    path: string;
    size: number;
    mtimeMs: number;
    ctimeMs: number;
    deleted: boolean;
    conflicted: boolean;
    /** Revision of the winning document that produced this content. */
    rev: string;
};

export type ReadResult =
    | (ReadCommon & { kind: "text"; text: string })
    | (ReadCommon & { kind: "binary"; base64: string });

export type StatParams = { path: string };

export type StatResult = ReadCommon & { kind: EntryKind };

/* -------------------------------------------------------------------------- */
/* write / delete / conflicts  (read-write mode only for the first two)        */
/* -------------------------------------------------------------------------- */

/**
 * Content to store. Mirrors `ReadResult`'s discriminator so a read result can be
 * fed back into a write without reshaping.
 *
 * `"text"` becomes a LiveSync `plain` entry, `"binary"` a `newnote` entry whose
 * chunks are base64 fragments. The choice is not the caller's to override: it is
 * derived from the blob type, exactly as upstream's `put` derives it, so a
 * sidecar-written entry is indistinguishable from a plugin-written one.
 */
export type WriteContent = { kind: "text"; text: string } | { kind: "binary"; base64: string };

/**
 * Compare-and-swap write.
 *
 * `baseRev` selects the CAS mode, and the three cases are deliberately distinct
 * values rather than a flag:
 *
 *   * `null` -- **create-only**. Fails `conflict` if any document exists at the
 *     path, *including* a soft-deleted one (LiveSync's `deleted: true` entry is
 *     a live document with a revision; the conflict detail carries
 *     `deleted: true` so a host can decide to resurrect instead).
 *   * `"<rev>"` -- **guarded update**. Fails `conflict` unless the remote's
 *     winning revision is exactly this. The failure carries the current rev.
 *   * absent -- **unguarded upsert**. Writes over whatever is there. Supported
 *     because tooling (import, export, repair) legitimately needs it; the Rust
 *     side may still refuse to expose it.
 *
 * `mtimeMs` defaults to now, `ctimeMs` to the existing entry's ctime (or
 * `mtimeMs` for a create), which is what the plugin does when it writes a file
 * it did not create.
 */
export type WriteParams = {
    path: string;
    content: WriteContent;
    baseRev?: string | null;
    mtimeMs?: number;
    ctimeMs?: number;
};

export type WriteResult = {
    path: string;
    /** Revision of the entry document this write produced. */
    rev: string;
    /**
     * The entry still has sibling conflict revisions.
     *
     * A guarded write neither creates nor resolves conflicts -- it targets the
     * winning revision -- so this reports a *pre-existing* conflict the host
     * should surface. It is never set by the write itself.
     */
    conflicted: boolean;
    size: number;
    mtimeMs: number;
    ctimeMs: number;
    kind: EntryKind;
    /** No document existed at this path before the write. */
    created: boolean;
    /** The write replaced a soft-deleted entry, bringing it back. */
    resurrected: boolean;
};

/**
 * Soft delete, matching the plugin's default.
 *
 * Sets `deleted: true` and bumps `mtime` on the entry document; the chunks and
 * the `children` list are left alone. It is NOT a CouchDB tombstone: this slice
 * never sends `_deleted: true`, because that would make the entry invisible to
 * `_all_docs` and unrecoverable, and because upstream only does it when the
 * user opted into `deleteMetadataOfDeletedFiles` (default off).
 *
 * `baseRev` guards the delete exactly as it guards a write. `null` and absent
 * both mean unguarded (create-only is meaningless for a delete). A path with no
 * document at all fails `not-found`.
 */
export type DeleteParams = { path: string; baseRev?: string | null };

export type DeleteResult = { path: string; rev: string; deleted: true };

export type ConflictRevision = {
    rev: string;
    mtimeMs: number;
    size: number;
    /** This revision is itself a soft delete. */
    deleted: boolean;
    /** Set when the revision's body could not be fetched; the other fields are 0. */
    unavailable?: boolean;
};

/**
 * Enumerates CouchDB's `_conflicts` for one path so a host can surface or
 * preserve the losing revisions.
 *
 * Read-only, and therefore available in BOTH modes -- refusing it on a
 * read-only mount would hide exactly the information a read-only host most
 * needs. Resolution (picking a winner, deleting the losers) is deliberately not
 * in this slice: it is destructive and needs a merge policy the sidecar has no
 * business choosing.
 */
export type ConflictsParams = { path: string };

export type ConflictsResult = {
    path: string;
    /** The revision `read`/`stat` serve. */
    winning: string;
    /** Sibling revisions, empty when the entry is not conflicted. */
    conflicts: ConflictRevision[];
};

/* -------------------------------------------------------------------------- */
/* changesSince                                                                */
/* -------------------------------------------------------------------------- */

export type ChangeEntry = {
    path: string;
    deleted: boolean;
    kind: EntryKind;
};

export type ChangesSinceParams = {
    /** Opaque cursor. Omit or pass null to start from the beginning of time. */
    cursor?: string | null;
    /** Maximum changes to return. Default 500, clamped to 2000. */
    limit?: number;
};

export type ChangesSinceResult = {
    changes: ChangeEntry[];
    /** Cursor to pass to the next call. Always present. Opaque. */
    nextCursor: string;
    /**
     * True when the feed was drained (no more changes right now).
     *
     * `changes` is frequently EMPTY while this is false. A real vault's change
     * feed is dominated by `leaf` chunk documents, which are filtered out, so a
     * page can consume its whole limit and yield nothing. Loop until
     * `exhausted`, carrying `nextCursor` forward; do not stop on an empty page.
     */
    exhausted: boolean;
};

/* -------------------------------------------------------------------------- */
/* watch / unwatch                                                             */
/* -------------------------------------------------------------------------- */

export type WatchParams = Record<string, never>;

export type WatchResult = {
    watching: true;
    /** Cursor the live feed started from. */
    cursor: string;
};

export type UnwatchParams = Record<string, never>;

export type UnwatchResult = { watching: false };

/** Notification method emitted while watching. */
export const CHANGE_NOTIFICATION = "change";

export type ChangeNotificationParams = ChangeEntry & {
    /** Cursor positioned immediately after this change. Opaque. */
    cursor: string;
};

/* -------------------------------------------------------------------------- */
/* health / shutdown                                                           */
/* -------------------------------------------------------------------------- */

export type HealthParams = Record<string, never>;

export type HealthResult = {
    /**
     * `"uninitialized"` before `initialize`; `"degraded"` once initialize has
     * run but the remote is not serveable, or after a remote error; otherwise
     * `"ok"`.
     */
    status: "ok" | "degraded" | "uninitialized";
    compatibility: Compatibility;
    /**
     * The mode this process was initialized in (`"read-only"` before
     * `initialize`).
     *
     * There is deliberately no orphan-chunk counter here. See the README: a
     * chunk is orphaned only relative to *every* entry's `children` list, so
     * "did my last write orphan anything" is not cheaply knowable -- it needs a
     * full-database refcount, which is what upstream's own maintenance report
     * does and what its (commented-out) GC would have needed.
     */
    mode: SidecarMode;
    watching: boolean;
    /** Redacted message of the most recent failure, if any. */
    lastError?: string;
    /** Change notifications emitted since `watch` (0 when not watching). */
    pendingChanges?: number;
    uptimeMs: number;
};

export type ShutdownParams = Record<string, never>;

export type ShutdownResult = { ok: true };

/* -------------------------------------------------------------------------- */
/* Method table                                                                */
/* -------------------------------------------------------------------------- */

export const Methods = {
    initialize: "initialize",
    manifest: "manifest",
    read: "read",
    stat: "stat",
    conflicts: "conflicts",
    changesSince: "changesSince",
    watch: "watch",
    unwatch: "unwatch",
    write: "write",
    delete: "delete",
    health: "health",
    shutdown: "shutdown",
} as const;

export type MethodName = (typeof Methods)[keyof typeof Methods];

/** Methods that require `initialize` to have reported `status: "ok"`. */
export const DATA_METHODS: readonly MethodName[] = [
    Methods.manifest,
    Methods.read,
    Methods.stat,
    Methods.conflicts,
    Methods.changesSince,
    Methods.watch,
    Methods.unwatch,
    Methods.write,
    Methods.delete,
];

/** Methods refused unless `initialize` was given `mode: "read-write"`. */
export const WRITE_METHODS: readonly MethodName[] = [Methods.write, Methods.delete];

/** A typed error the dispatcher converts into a JSON-RPC failure. */
export class SidecarError extends Error {
    readonly kind: ErrorKind;
    readonly status?: CompatibilityStatus;
    readonly conflict?: ConflictDetail;

    constructor(kind: ErrorKind, message: string, status?: CompatibilityStatus) {
        super(message);
        this.name = "SidecarError";
        this.kind = kind;
        if (status !== undefined) {
            this.status = status;
        }
    }

    /** Builds the `conflict` failure, the only error carrying structured data. */
    static conflict(message: string, detail: ConflictDetail): SidecarError {
        const error = new SidecarError("conflict", message);
        (error as { conflict?: ConflictDetail }).conflict = detail;
        return error;
    }

    toErrorBody(): JsonRpcErrorBody {
        const data: JsonRpcErrorData = { kind: this.kind };
        if (this.message) {
            data.detail = this.message;
        }
        if (this.status !== undefined) {
            data.status = this.status;
        }
        if (this.conflict !== undefined) {
            data.conflict = this.conflict;
        }
        return {
            code: ERROR_CODE_BY_KIND[this.kind],
            message: this.message || this.kind,
            data,
        };
    }
}
