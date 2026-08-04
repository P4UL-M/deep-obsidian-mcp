/**
 * Drives the built sidecar as a real child process over real stdio.
 *
 * The child-process boundary is the point of these tests: it exercises the
 * newline framing, the stdout/stderr split, and the shutdown path, none of
 * which an in-process call could check. It also means nothing can be injected
 * into the sidecar -- hence `MockCouch` rather than a fake `fetch`.
 */
import { spawn } from "node:child_process";
import { once } from "node:events";
import { existsSync } from "node:fs";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import * as path from "node:path";
import { MockCouch } from "./mock-couch.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
export const BUNDLE = path.resolve(here, "..", "dist", "sidecar.mjs");

export const PROTOCOL_VERSION = 1;

function requireBundle() {
    if (!existsSync(BUNDLE)) {
        throw new Error(
            `dist/sidecar.mjs is missing. These tests run the built artifact on purpose; run \`npm run build\` first.`
        );
    }
}

export class Sidecar {
    constructor() {
        requireBundle();
        this.nextId = 1;
        this.pending = new Map();
        this.notifications = [];
        this.notificationWaiters = [];
        /** Everything the child wrote to stderr, for redaction assertions. */
        this.stderr = "";
        /** Lines on stdout that were not valid JSON-RPC: must stay empty. */
        this.junk = [];

        this.child = spawn(process.execPath, [BUNDLE], {
            stdio: ["pipe", "pipe", "pipe"],
            // Deliberately no secrets in the environment: the protocol carries
            // them, and a test that passed them here would not prove that.
            env: { ...process.env, NODE_ENV: "test" },
        });

        this.exited = once(this.child, "exit");

        this.child.stderr.setEncoding("utf8");
        this.child.stderr.on("data", (chunk) => {
            this.stderr += chunk;
        });

        const stdout = createInterface({ input: this.child.stdout, crlfDelay: Infinity });
        stdout.on("line", (line) => this.#onLine(line));
    }

    #onLine(line) {
        const trimmed = line.trim();
        if (trimmed === "") return;
        let message;
        try {
            message = JSON.parse(trimmed);
        } catch {
            // A non-JSON line on stdout means something leaked into the
            // protocol stream. Recorded rather than thrown so the failing test
            // can show it.
            this.junk.push(trimmed);
            return;
        }
        if (message.jsonrpc !== "2.0") {
            this.junk.push(trimmed);
            return;
        }
        if (message.id === undefined || message.id === null) {
            this.notifications.push(message);
            const waiters = this.notificationWaiters;
            this.notificationWaiters = [];
            for (const resolve of waiters) resolve();
            return;
        }
        const entry = this.pending.get(message.id);
        if (!entry) {
            this.junk.push(`unmatched response id ${String(message.id)}`);
            return;
        }
        this.pending.delete(message.id);
        entry(message);
    }

    /** Sends a request and resolves with the raw JSON-RPC envelope. */
    send(method, params, { raw } = {}) {
        const id = this.nextId++;
        const payload = raw ?? { jsonrpc: "2.0", id, method, params };
        const promise = new Promise((resolve, reject) => {
            this.pending.set(id, resolve);
            const timer = setTimeout(() => {
                this.pending.delete(id);
                reject(new Error(`timed out waiting for ${method}\n--- stderr ---\n${this.stderr}`));
            }, 20_000);
            timer.unref();
        });
        this.child.stdin.write(`${JSON.stringify(payload)}\n`);
        return promise;
    }

    /** Sends a request and returns `result`, throwing on a JSON-RPC error. */
    async call(method, params) {
        const response = await this.send(method, params);
        if (response.error) {
            const error = new Error(`${method} failed: ${response.error.message}`);
            error.rpc = response.error;
            throw error;
        }
        return response.result;
    }

    /** Sends a raw line, bypassing the envelope, to test malformed input. */
    writeRaw(text) {
        this.child.stdin.write(`${text}\n`);
    }

    async waitForNotification(method, timeoutMs = 10_000) {
        const deadline = Date.now() + timeoutMs;
        for (;;) {
            const found = this.notifications.find((n) => n.method === method);
            if (found) return found;
            if (Date.now() > deadline) {
                throw new Error(`no ${method} notification within ${timeoutMs}ms\n--- stderr ---\n${this.stderr}`);
            }
            await new Promise((resolve) => {
                const timer = setTimeout(resolve, 100);
                timer.unref();
                this.notificationWaiters.push(resolve);
            });
        }
    }

    async shutdown() {
        if (this.child.exitCode !== null || this.child.signalCode !== null) return this.child.exitCode;
        try {
            await this.call("shutdown", {});
        } catch {
            /* the child may exit before the reply is read */
        }
        const settled = await Promise.race([
            this.exited,
            new Promise((resolve) => {
                const timer = setTimeout(() => resolve(null), 8_000);
                timer.unref();
            }),
        ]);
        if (settled === null) {
            this.child.kill("SIGKILL");
            await this.exited;
            throw new Error(`sidecar did not exit after shutdown\n--- stderr ---\n${this.stderr}`);
        }
        return settled[0];
    }

    async kill() {
        if (this.child.exitCode === null && this.child.signalCode === null) {
            this.child.kill("SIGKILL");
            await this.exited;
        }
    }
}

/**
 * Boots a mock CouchDB plus a sidecar, runs `body`, and tears both down even if
 * the body throws.
 *
 * @param {object} options
 * @param {object} options.vault fixture bundle: {docs, localDocs, conflicts}
 * @param {number} [options.authStatus]
 * @param {object} [options.e2ee]
 * @param {object} [options.initOptions]
 * @param {boolean} [options.skipInitialize]
 * @param {string} [options.password]
 */
export async function withSidecar(options, body) {
    const couch = new MockCouch({
        docs: options.vault?.docs ?? {},
        localDocs: options.vault?.localDocs ?? {},
        conflicts: options.vault?.conflicts ?? {},
        ...(options.authStatus !== undefined ? { authStatus: options.authStatus } : {}),
    });
    const url = await couch.listen();
    const sidecar = new Sidecar();
    let initializeResult;
    try {
        if (!options.skipInitialize) {
            initializeResult = await sidecar.call("initialize", {
                protocolVersion: PROTOCOL_VERSION,
                couchdb: {
                    url,
                    database: "vault",
                    username: "vaultuser",
                    password: options.password ?? "s3cr3t-password-value",
                },
                ...(options.e2ee ? { e2ee: options.e2ee } : {}),
                ...(options.initOptions ? { options: options.initOptions } : {}),
            });
        }
        await body({ sidecar, couch, url, initializeResult });
    } finally {
        try {
            await sidecar.shutdown();
        } catch {
            await sidecar.kill();
        }
        await couch.close();
    }
}
