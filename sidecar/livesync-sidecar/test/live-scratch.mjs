/**
 * Creates and drops a throwaway, REAL LiveSync-shaped database on a real CouchDB, so a
 * non-Node test can get a serveable vault to write against.
 *
 * Exists because a bare CouchDB database handshakes as `unknown-schema` and writes are
 * refused however the host asks — correctly, that is the fail-closed gate. Proving
 * anything about writes against a real server therefore needs a database with the
 * version document and the milestone the gate looks for, and that seeding already exists
 * in `live-couch.test.mjs`. Reimplementing it in Rust would be a second copy of a shape
 * that has no spec.
 *
 * Never touches the database named in the environment: it creates its own, named on the
 * command line, and `drop` removes exactly that one.
 *
 * ```sh
 * node test/live-scratch.mjs --url http://127.0.0.1:5984 --user admin --password pw \
 *   --database scratch
 * ```
 *
 * Protocol with the parent: same shape as `mock-couch-server.mjs` — one JSON handshake
 * line, then one JSON line per newline-delimited command, and stdin EOF drops the
 * database and exits, so no scratch database can outlive a parent that crashed.
 */
import { createInterface } from "node:readline";
import { SCHEMA_VERSION } from "./fixtures.mjs";

const args = process.argv.slice(2);
const option = (name, fallback) => {
    const index = args.indexOf(`--${name}`);
    return index === -1 ? fallback : args[index + 1];
};
const url = option("url", process.env.DEEP_OBSIDIAN_COUCHDB_URL);
const username = option("user", process.env.DEEP_OBSIDIAN_COUCHDB_USER ?? "admin");
const password = option("password", process.env.DEEP_OBSIDIAN_COUCHDB_PASSWORD ?? "");
const database = option("database", "deep-obsidian-live-scratch");

if (!url) {
    process.stderr.write("usage: node test/live-scratch.mjs --url <url> [--user u] [--password p] [--database d]\n");
    process.exit(2);
}

async function couch(method, requestPath, body) {
    const response = await fetch(`${url}/${requestPath}`, {
        method,
        headers: {
            authorization: `Basic ${Buffer.from(`${username}:${password}`).toString("base64")}`,
            "content-type": "application/json",
        },
        ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
    });
    return { status: response.status, body: await response.json().catch(() => undefined) };
}

/** A minimal but REAL LiveSync vault: the version document plus a milestone. */
async function seed() {
    await couch("DELETE", database);
    const created = await couch("PUT", database);
    await couch("PUT", `${database}/obsydian_livesync_version`, {
        type: "versioninfo",
        version: SCHEMA_VERSION,
    });
    await couch("PUT", `${database}/_local%2Fobsydian_livesync_milestone`, {
        type: "milestoneinfo",
        created: 1_700_000_000_000,
        locked: false,
        accepted_nodes: ["node-a"],
        node_chunk_info: { "node-a": { min: 0, max: 2, current: 2 } },
        node_info: {},
        tweak_values: {},
    });
    return created.status;
}

const status = await seed();
process.stdout.write(`${JSON.stringify({ url, database, status })}\n`);

const input = createInterface({ input: process.stdin });
input.on("line", async (line) => {
    const text = line.trim();
    if (!text) return;
    let response;
    try {
        const message = JSON.parse(text);
        if (message.command === "reseed") {
            response = { ok: true, status: await seed() };
        } else if (message.command === "doc-ids") {
            const { body } = await couch("GET", `${database}/_all_docs`);
            response = { ok: true, ids: (body?.rows ?? []).map((row) => row.id) };
        } else {
            response = { ok: false, error: `unknown command: ${message.command}` };
        }
    } catch (error) {
        response = { ok: false, error: String(error?.message ?? error) };
    }
    process.stdout.write(`${JSON.stringify(response)}\n`);
});

// stdin EOF is the stop signal, and it DROPS the scratch database: a crashed parent must
// not leave one behind on someone's server.
input.on("close", async () => {
    await couch("DELETE", database);
    process.exit(0);
});
