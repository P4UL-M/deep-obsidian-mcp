/**
 * Stderr logging with secret redaction, plus the stdout lockdown that makes a
 * newline-delimited JSON protocol survive a chatty dependency.
 *
 * Two problems this module solves:
 *
 *  1. commonlib logs through octagonal-wheels' `Logger`, whose default sink is
 *     `console.log` -- i.e. **stdout**. Verified empirically: an unconfigured
 *     `DirectFileManipulator` prints "[LiveSyncLocalDB] Opening Database..."
 *     on fd 1. Left alone that interleaves with protocol frames and corrupts
 *     the stream. `installLogging()` re-points `Logger` at stderr *and*
 *     rebinds `console.log`/`console.info`/`console.debug`, because
 *     `DirectFileManipulator` also calls bare `console.warn` and other
 *     transitive code may reach for `console.log` directly.
 *
 *  2. Anything logged may embed the CouchDB URL (with userinfo), the password,
 *     or the passphrase. `redact()` masks registered secrets by value, so it
 *     does not matter which code path leaked them. When path obfuscation is
 *     enabled the vault's plaintext paths are themselves sensitive, so path
 *     logging is suppressed wholesale in that mode.
 */
import { setGlobalLogFunction } from "@vrtmrz/livesync-commonlib/compat/common/logger";

const MASK = "[redacted]";

/** Registered secret literals, longest first so overlapping values mask fully. */
let secrets: string[] = [];
let suppressPaths = false;
let minLevel = 32; // LOG_LEVEL_INFO; commonlib's verbose traffic is dropped by default.

/** The one writable handle to fd 1. Captured before console is rebound. */
const realStdoutWrite = process.stdout.write.bind(process.stdout);

/**
 * Registers values that must never appear on stderr. Called from `initialize`
 * before the manipulator is constructed.
 */
export function registerSecrets(values: (string | undefined)[]): void {
    for (const value of values) {
        // One- and two-character values would mask half the log; a real
        // credential is never that short, and a stray "a" is not a leak.
        if (typeof value === "string" && value.length >= 3 && !secrets.includes(value)) {
            secrets.push(value);
        }
    }
    secrets.sort((a, b) => b.length - a.length);
}

/** Enables path suppression for obfuscated vaults. */
export function setSuppressPaths(value: boolean): void {
    suppressPaths = value;
}

export function setMinLevel(level: number): void {
    minLevel = level;
}

/** Test seam: forget registered secrets. */
export function resetSecrets(): void {
    secrets = [];
    suppressPaths = false;
}

/**
 * Masks registered secrets, URL userinfo, and (when obfuscation is on) any
 * path-looking token. Applied to every stderr line without exception.
 */
export function redact(input: string): string {
    let out = input;
    for (const secret of secrets) {
        // Plain split/join: no regex, so no escaping hazards with characters
        // that are legal in passwords.
        out = out.split(secret).join(MASK);
    }
    // `scheme://user:pass@host` survives even if the password was never
    // registered (e.g. credentials embedded in the URL by the caller).
    out = out.replace(/([a-zA-Z][a-zA-Z0-9+.-]*:\/\/)[^/@\s]+@/g, `$1${MASK}@`);
    if (suppressPaths) {
        // Anything with a slash or a known vault extension is treated as a
        // path. Deliberately blunt: a false positive costs log detail, a false
        // negative leaks vault structure.
        out = out.replace(/\S*\/\S*/g, MASK).replace(/\S+\.(md|canvas|json|png|jpe?g|pdf|webp)\b/gi, MASK);
    }
    return out;
}

function stringify(message: unknown): string {
    if (typeof message === "string") return message;
    if (message instanceof Error) {
        return `${message.name}: ${message.message}`;
    }
    try {
        return JSON.stringify(message) ?? String(message);
    } catch {
        return String(message);
    }
}

/** Writes one redacted line to stderr. Never throws. */
export function logStderr(scope: string, message: unknown): void {
    try {
        const line = redact(`[${scope}] ${stringify(message)}`);
        process.stderr.write(`${line}\n`);
    } catch {
        /* logging must never take the process down */
    }
}

/**
 * Writes one protocol frame to the real stdout. The only sanctioned writer of
 * fd 1 in this process.
 */
export function writeFrame(json: string): void {
    realStdoutWrite(`${json}\n`);
}

/**
 * Routes commonlib's Logger and the console to redacted stderr, and makes
 * `console.log` physically unable to reach the protocol stream.
 */
export function installLogging(): void {
    setGlobalLogFunction((message: unknown, level?: number, key?: string) => {
        if (typeof level === "number" && level < minLevel) return;
        logStderr(key ? `commonlib:${key}` : "commonlib", message);
    });

    const toStderr =
        (scope: string) =>
        (...args: unknown[]): void => {
            logStderr(scope, args.map(stringify).join(" "));
        };

    console.log = toStderr("console");
    console.info = toStderr("console");
    console.debug = toStderr("console");
    console.warn = toStderr("console:warn");
    console.error = toStderr("console:error");
    console.trace = toStderr("console:trace");
    console.dir = toStderr("console");
}
