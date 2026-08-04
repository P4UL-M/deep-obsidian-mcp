/**
 * Sidecar entry point: newline-delimited JSON-RPC 2.0 on stdin/stdout.
 *
 * Requests are handled strictly in order. That is a deliberate simplification:
 * the supervisor is a single Rust task, the underlying PouchDB connection is not
 * usefully concurrent, and serial handling means `initialize` cannot race a data
 * method. Long operations are bounded by the per-request fetch timeout in
 * `manipulator.ts`, not by parallelism.
 */
import { createInterface } from "node:readline";
import {
    CHANGE_NOTIFICATION,
    DATA_METHODS,
    Methods,
    PROTOCOL_VERSION,
    SIDECAR_MODES,
    SIDECAR_VERSION,
    SUPPORTED,
    SidecarError,
} from "./protocol.js";
import type {
    ChangesSinceParams,
    ConflictsResult,
    DeleteResult,
    ErrorKind,
    ChangesSinceResult,
    HealthResult,
    InitializeParams,
    InitializeResult,
    JsonRpcFailure,
    JsonRpcId,
    JsonRpcRequest,
    JsonRpcSuccess,
    ManifestParams,
    ManifestResult,
    MethodName,
    ReadParams,
    ReadResult,
    ShutdownResult,
    SidecarMode,
    StatParams,
    StatResult,
    UnwatchResult,
    WatchResult,
    WriteContent,
    WriteResult,
} from "./protocol.js";
import { installLogging, logStderr, redact, writeFrame } from "./logging.js";
import { LiveSyncVault } from "./manipulator.js";

/**
 * Install the stderr routing before anything else runs. commonlib's logger
 * defaults to `console.log`, i.e. stdout, which would corrupt the frame stream
 * the moment a database is opened.
 */
installLogging();

const startedAt = Date.now();

const state = {
    initialized: false,
    vault: new LiveSyncVault(),
    lastError: undefined as string | undefined,
    changeCount: 0,
    shuttingDown: false,
};

const DEFAULT_PAGE_LIMIT = 500;
const MAX_PAGE_LIMIT = 2000;

/* -------------------------------------------------------------------------- */
/* Framing                                                                     */
/* -------------------------------------------------------------------------- */

function sendResult(id: JsonRpcId, result: unknown): void {
    const message: JsonRpcSuccess = { jsonrpc: "2.0", id, result };
    writeFrame(JSON.stringify(message));
}

function sendError(id: JsonRpcId | null, error: SidecarError): void {
    const body = error.toErrorBody();
    // The message reaches the host and may be surfaced to a user or a log, so
    // it goes through the same redaction as stderr.
    body.message = redact(body.message);
    if (body.data?.detail) {
        body.data.detail = redact(body.data.detail);
    }
    const message: JsonRpcFailure = { jsonrpc: "2.0", id, error: body };
    writeFrame(JSON.stringify(message));
}

function sendNotification(method: string, params: unknown): void {
    writeFrame(JSON.stringify({ jsonrpc: "2.0", method, params }));
}

/* -------------------------------------------------------------------------- */
/* Param validation                                                            */
/* -------------------------------------------------------------------------- */

function asObject(params: unknown, method: string): Record<string, unknown> {
    if (params === undefined || params === null) return {};
    if (typeof params !== "object" || Array.isArray(params)) {
        throw new SidecarError("invalid-params", `${method} expects an object for params`);
    }
    return params as Record<string, unknown>;
}

function requireString(value: unknown, name: string): string {
    if (typeof value !== "string" || value === "") {
        throw new SidecarError("invalid-params", `${name} must be a non-empty string`);
    }
    return value;
}

function optionalCursor(value: unknown, name: string): string | null {
    if (value === undefined || value === null) return null;
    if (typeof value !== "string") {
        throw new SidecarError("invalid-params", `${name} must be a string when present`);
    }
    return value;
}

/**
 * `baseRev` is tri-state, so the three JSON values must stay distinguishable:
 * absent (unguarded), `null` (create-only), a string (guarded). `undefined`
 * cannot be expressed in JSON, so "absent" is detected by key presence.
 */
function optionalBaseRev(object: Record<string, unknown>): string | null | undefined {
    if (!("baseRev" in object)) return undefined;
    const value = object["baseRev"];
    if (value === null) return null;
    if (value === undefined) return undefined;
    if (typeof value !== "string" || value === "") {
        throw new SidecarError(
            "invalid-params",
            "baseRev must be a non-empty string, null (create-only), or omitted (unguarded)"
        );
    }
    return value;
}

function optionalTimestamp(value: unknown, name: string): number | undefined {
    if (value === undefined || value === null) return undefined;
    if (typeof value !== "number" || !Number.isFinite(value) || value < 0) {
        throw new SidecarError("invalid-params", `${name} must be a non-negative number when present`);
    }
    return value;
}

function writeContent(value: unknown): WriteContent {
    const content = asObject(value, "write.content");
    const kind = content["kind"];
    if (kind === "text") {
        const text = content["text"];
        if (typeof text !== "string") {
            throw new SidecarError("invalid-params", "content.text must be a string");
        }
        return { kind: "text", text };
    }
    if (kind === "binary") {
        const base64 = content["base64"];
        if (typeof base64 !== "string") {
            throw new SidecarError("invalid-params", "content.base64 must be a string");
        }
        return { kind: "binary", base64 };
    }
    throw new SidecarError("invalid-params", 'content.kind must be "text" or "binary"');
}

function pageLimit(value: unknown): number {
    if (value === undefined || value === null) return DEFAULT_PAGE_LIMIT;
    if (typeof value !== "number" || !Number.isInteger(value) || value <= 0) {
        throw new SidecarError("invalid-params", "limit must be a positive integer");
    }
    return Math.min(value, MAX_PAGE_LIMIT);
}

/* -------------------------------------------------------------------------- */
/* Methods                                                                     */
/* -------------------------------------------------------------------------- */

async function handleInitialize(params: unknown): Promise<InitializeResult> {
    const object = asObject(params, Methods.initialize);

    // Protocol version is checked before anything else and before any secret is
    // touched: a mismatched host must get a clean refusal, and the process stays
    // alive so the supervisor can read it and report rather than see an EOF.
    const requested = object["protocolVersion"];
    if (requested !== PROTOCOL_VERSION) {
        throw new SidecarError(
            "unsupported-protocol-version",
            `sidecar speaks protocol version ${PROTOCOL_VERSION}, host requested ${JSON.stringify(requested)}`
        );
    }
    if (state.initialized) {
        throw new SidecarError("already-initialized", "initialize has already been called on this process");
    }

    // Absent means read-only. That is what keeps `mode` an additive field: a v1
    // host that never heard of it gets exactly v1 behaviour.
    const modeRaw = object["mode"];
    let mode: SidecarMode = "read-only";
    if (modeRaw !== undefined && modeRaw !== null) {
        if (typeof modeRaw !== "string" || !SIDECAR_MODES.includes(modeRaw as SidecarMode)) {
            throw new SidecarError(
                "invalid-params",
                `mode must be one of ${SIDECAR_MODES.map((value) => JSON.stringify(value)).join(", ")}`
            );
        }
        mode = modeRaw as SidecarMode;
    }

    const couchdb = asObject(object["couchdb"], "initialize.couchdb");
    const url = requireString(couchdb["url"], "couchdb.url");
    const database = requireString(couchdb["database"], "couchdb.database");
    const username = typeof couchdb["username"] === "string" ? couchdb["username"] : "";
    const password = typeof couchdb["password"] === "string" ? couchdb["password"] : "";

    const e2eeRaw = object["e2ee"];
    let passphrase: string | undefined;
    let obfuscatePassphrase: string | undefined;
    if (e2eeRaw !== undefined && e2eeRaw !== null) {
        const e2ee = asObject(e2eeRaw, "initialize.e2ee");
        passphrase = requireString(e2ee["passphrase"], "e2ee.passphrase");
        if (e2ee["obfuscatePassphrase"] !== undefined && e2ee["obfuscatePassphrase"] !== null) {
            obfuscatePassphrase = requireString(e2ee["obfuscatePassphrase"], "e2ee.obfuscatePassphrase");
        }
    }

    const options = (object["options"] ?? {}) as InitializeParams["options"];

    // Marked initialized before connecting: a second initialize must be refused
    // even while the first is still negotiating.
    state.initialized = true;
    const outcome = await state.vault.connect({
        url,
        database,
        username,
        password,
        mode,
        ...(passphrase !== undefined ? { passphrase } : {}),
        ...(obfuscatePassphrase !== undefined ? { obfuscatePassphrase } : {}),
        ...(options !== undefined ? { options } : {}),
    });

    if (outcome.compatibility.status !== "ok") {
        state.lastError = outcome.compatibility.detail ?? outcome.compatibility.status;
        logStderr("initialize", `remote is not serveable: ${outcome.compatibility.status}`);
    }

    return {
        protocolVersion: PROTOCOL_VERSION,
        mode,
        sidecarVersion: SIDECAR_VERSION,
        commonlibVersion: SUPPORTED.commonlibVersion,
        supportedSchemaVersion: SUPPORTED.maxSchemaVersion,
        supported: SUPPORTED,
        compatibility: outcome.compatibility,
        remote: outcome.remote,
    };
}

async function handleManifest(params: unknown): Promise<ManifestResult> {
    const object = asObject(params, Methods.manifest);
    const metaOnly = object["metaOnly"];
    if (metaOnly !== undefined && metaOnly !== true) {
        throw new SidecarError(
            "invalid-params",
            "manifest is metadata-only; metaOnly must be true or omitted"
        );
    }
    const cursor = optionalCursor(object["cursor"], "cursor");
    return await state.vault.manifest(cursor, pageLimit(object["limit"]));
}

async function handleRead(params: unknown): Promise<ReadResult> {
    const object = asObject(params, Methods.read);
    const path = requireString(object["path"], "path") as ReadParams["path"];
    return await state.vault.read(path);
}

async function handleStat(params: unknown): Promise<StatResult> {
    const object = asObject(params, Methods.stat);
    const path = requireString(object["path"], "path") as StatParams["path"];
    return await state.vault.stat(path);
}

async function handleConflicts(params: unknown): Promise<ConflictsResult> {
    const object = asObject(params, Methods.conflicts);
    return await state.vault.conflicts(requireString(object["path"], "path"));
}

async function handleWrite(params: unknown): Promise<WriteResult> {
    const object = asObject(params, Methods.write);
    const baseRev = optionalBaseRev(object);
    const mtimeMs = optionalTimestamp(object["mtimeMs"], "mtimeMs");
    const ctimeMs = optionalTimestamp(object["ctimeMs"], "ctimeMs");
    return await state.vault.write({
        path: requireString(object["path"], "path"),
        content: writeContent(object["content"]),
        ...(baseRev !== undefined ? { baseRev } : {}),
        ...(mtimeMs !== undefined ? { mtimeMs } : {}),
        ...(ctimeMs !== undefined ? { ctimeMs } : {}),
    });
}

async function handleDelete(params: unknown): Promise<DeleteResult> {
    const object = asObject(params, Methods.delete);
    const path = requireString(object["path"], "path");
    const baseRev = optionalBaseRev(object);
    // `null` and absent both mean unguarded here: create-only has no meaning for
    // a delete, and refusing `null` would only trip hosts that serialise their
    // optional fields eagerly.
    return await state.vault.remove(path, baseRev ?? undefined);
}

async function handleChangesSince(params: unknown): Promise<ChangesSinceResult> {
    const object = asObject(params, Methods.changesSince);
    const cursor = optionalCursor(object["cursor"], "cursor") as ChangesSinceParams["cursor"];
    return await state.vault.changesSince(cursor, pageLimit(object["limit"]));
}

async function handleWatch(params: unknown): Promise<WatchResult> {
    asObject(params, Methods.watch);
    state.changeCount = 0;
    const cursor = await state.vault.watch((change) => {
        state.changeCount += 1;
        sendNotification(CHANGE_NOTIFICATION, change);
    });
    return { watching: true, cursor };
}

function handleUnwatch(params: unknown): UnwatchResult {
    asObject(params, Methods.unwatch);
    state.vault.unwatch();
    return { watching: false };
}

function handleHealth(params: unknown): HealthResult {
    asObject(params, Methods.health);
    const compatibility = state.initialized ? state.vault.compatibilityStatus : { status: "unknown" as const };
    // Derived from serveability alone, deliberately NOT from `lastError`.
    // `lastError` records the most recent failure of any kind, including
    // caller-fault ones like reading a mistyped path -- letting that latch the
    // status would mark a perfectly healthy vault degraded forever, and the
    // supervisor branches on this field.
    const status: HealthResult["status"] = !state.initialized
        ? "uninitialized"
        : state.vault.isServeable
          ? "ok"
          : "degraded";
    return {
        status,
        compatibility,
        mode: state.vault.currentMode,
        watching: state.vault.isWatching,
        ...(state.lastError !== undefined ? { lastError: redact(state.lastError) } : {}),
        pendingChanges: state.vault.isWatching ? state.changeCount : 0,
        uptimeMs: Date.now() - startedAt,
    };
}

/* -------------------------------------------------------------------------- */
/* Dispatch                                                                    */
/* -------------------------------------------------------------------------- */

const KNOWN_METHODS = new Set<string>(Object.values(Methods));

/**
 * Error kinds that describe a bad request rather than a sick sidecar. These are
 * not recorded in `health.lastError`.
 */
const CALLER_FAULT_KINDS: ReadonlySet<ErrorKind> = new Set<ErrorKind>([
    "parse-error",
    "invalid-request",
    "method-not-found",
    "invalid-params",
    "not-found",
    "not-initialized",
    "already-initialized",
    "unsupported-protocol-version",
    // A lost compare-and-swap is the *expected* outcome of concurrent editing and
    // a read-only refusal is a configuration fact, not a sick remote. Latching
    // either into `lastError` would make a perfectly healthy vault look broken
    // forever, and the supervisor surfaces that field.
    "conflict",
    "read-only",
]);

async function dispatch(method: string, params: unknown): Promise<unknown> {
    if (!KNOWN_METHODS.has(method)) {
        throw new SidecarError("method-not-found", `unknown method ${JSON.stringify(method)}`);
    }
    const name = method as MethodName;

    // Fail closed: every data method is refused until initialize has run and
    // reported "ok". `requireServeable` inside the vault repeats the check --
    // this gate exists so the *typed* reason is "not-initialized" rather than
    // an incidental null dereference.
    if (DATA_METHODS.includes(name) && !state.initialized) {
        throw new SidecarError("not-initialized", `${method} requires a successful initialize first`);
    }

    switch (name) {
        case Methods.initialize:
            return await handleInitialize(params);
        case Methods.manifest:
            return await handleManifest(params);
        case Methods.read:
            return await handleRead(params);
        case Methods.stat:
            return await handleStat(params);
        case Methods.conflicts:
            return await handleConflicts(params);
        case Methods.write:
            return await handleWrite(params);
        case Methods.delete:
            return await handleDelete(params);
        case Methods.changesSince:
            return await handleChangesSince(params);
        case Methods.watch:
            return await handleWatch(params);
        case Methods.unwatch:
            return handleUnwatch(params);
        case Methods.health:
            return handleHealth(params);
        case Methods.shutdown:
            asObject(params, Methods.shutdown);
            queueShutdown();
            return { ok: true } satisfies ShutdownResult;
    }
}

function toSidecarError(error: unknown): SidecarError {
    if (error instanceof SidecarError) return error;
    const message = error instanceof Error ? error.message : String(error);
    return new SidecarError("internal-error", message);
}

async function handleLine(line: string): Promise<void> {
    const trimmed = line.trim();
    if (trimmed === "") return;

    let parsed: unknown;
    try {
        parsed = JSON.parse(trimmed);
    } catch {
        sendError(null, new SidecarError("parse-error", "request was not valid JSON"));
        return;
    }

    if (
        typeof parsed !== "object" ||
        parsed === null ||
        Array.isArray(parsed) ||
        (parsed as JsonRpcRequest).jsonrpc !== "2.0" ||
        typeof (parsed as JsonRpcRequest).method !== "string"
    ) {
        const maybeId = (parsed as { id?: JsonRpcId })?.id;
        sendError(
            typeof maybeId === "string" || typeof maybeId === "number" ? maybeId : null,
            new SidecarError("invalid-request", "expected a JSON-RPC 2.0 request object")
        );
        return;
    }

    const request = parsed as JsonRpcRequest;
    if (typeof request.id !== "string" && typeof request.id !== "number") {
        // A notification from the host: nothing in v1 defines one, and there is
        // no channel to answer on, so log and drop rather than guess.
        logStderr("rpc", `ignoring host notification for method ${request.method}`);
        return;
    }

    try {
        const result = await dispatch(request.method, request.params);
        sendResult(request.id, result);
    } catch (error) {
        const sidecarError = toSidecarError(error);
        // Only failures that say something about the *remote or the sidecar*
        // are worth remembering. A bad path or malformed params is the caller's
        // problem and would otherwise pollute every later health report.
        if (!CALLER_FAULT_KINDS.has(sidecarError.kind)) {
            state.lastError = sidecarError.message;
        }
        logStderr("rpc", `${request.method} failed: ${sidecarError.kind}: ${sidecarError.message}`);
        sendError(request.id, sidecarError);
    }
}

/* -------------------------------------------------------------------------- */
/* Lifecycle                                                                   */
/* -------------------------------------------------------------------------- */

/**
 * Shuts down after the `shutdown` reply has been flushed.
 *
 * The explicit `process.exit` is required, not defensive: commonlib opens a live
 * change feed while initialising the database and PouchDB keeps timers alive, so
 * the event loop does not drain on its own. `close()` is still awaited first so
 * the feed is cancelled cleanly, with a hard deadline in case it is not.
 */
function queueShutdown(): void {
    if (state.shuttingDown) return;
    state.shuttingDown = true;
    const hardExit = setTimeout(() => process.exit(0), 5_000);
    hardExit.unref();
    setImmediate(() => {
        void (async () => {
            try {
                await state.vault.close();
            } catch (error) {
                logStderr("shutdown", `close failed: ${(error as Error)?.message ?? String(error)}`);
            }
            process.exit(0);
        })();
    });
}

/**
 * Serialises handling. `readline` emits lines faster than requests complete, so
 * without this chain two `initialize` calls could interleave.
 */
let queue: Promise<void> = Promise.resolve();

const input = createInterface({ input: process.stdin, crlfDelay: Infinity });

input.on("line", (line) => {
    queue = queue.then(() => handleLine(line)).catch((error) => {
        logStderr("rpc", `unhandled dispatch failure: ${(error as Error)?.message ?? String(error)}`);
    });
});

// The supervisor closing stdin is the other shutdown signal, and the one that
// fires if the supervisor dies: never outlive the parent.
input.on("close", () => {
    queueShutdown();
});

process.on("uncaughtException", (error) => {
    logStderr("fatal", `uncaught exception: ${error?.message ?? String(error)}`);
});

process.on("unhandledRejection", (reason) => {
    logStderr("fatal", `unhandled rejection: ${(reason as Error)?.message ?? String(reason)}`);
});

logStderr("startup", `livesync-sidecar ${SIDECAR_VERSION} (protocol ${PROTOCOL_VERSION}) ready`);
