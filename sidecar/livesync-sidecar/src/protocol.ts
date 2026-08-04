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
    | "corrupted-document";

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
 * `initialize` is the ONLY place secrets cross the process boundary: never
 * argv (visible in `ps`), never the environment (inherited by children and
 * often captured by crash reporters).
 */
export type InitializeParams = {
    protocolVersion: number;
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
    changesSince: "changesSince",
    watch: "watch",
    unwatch: "unwatch",
    health: "health",
    shutdown: "shutdown",
} as const;

export type MethodName = (typeof Methods)[keyof typeof Methods];

/** Methods that require `initialize` to have reported `status: "ok"`. */
export const DATA_METHODS: readonly MethodName[] = [
    Methods.manifest,
    Methods.read,
    Methods.stat,
    Methods.changesSince,
    Methods.watch,
    Methods.unwatch,
];

/** A typed error the dispatcher converts into a JSON-RPC failure. */
export class SidecarError extends Error {
    readonly kind: ErrorKind;
    readonly status?: CompatibilityStatus;

    constructor(kind: ErrorKind, message: string, status?: CompatibilityStatus) {
        super(message);
        this.name = "SidecarError";
        this.kind = kind;
        if (status !== undefined) {
            this.status = status;
        }
    }

    toErrorBody(): JsonRpcErrorBody {
        const data: JsonRpcErrorData = { kind: this.kind };
        if (this.message) {
            data.detail = this.message;
        }
        if (this.status !== undefined) {
            data.status = this.status;
        }
        return {
            code: ERROR_CODE_BY_KIND[this.kind],
            message: this.message || this.kind,
            data,
        };
    }
}
