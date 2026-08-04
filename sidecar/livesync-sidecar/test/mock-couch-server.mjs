/**
 * Runs `MockCouch` over a real socket so a NON-Node test can drive the sidecar.
 *
 * The Rust integration tests need the same fixture vault the sidecar's own suite
 * uses. Reimplementing a CouchDB emulator in Rust was the alternative and was
 * rejected: the mock's endpoint set was discovered empirically against upstream's
 * actual request shapes, and a second hand-written copy would drift from it
 * silently — at which point the Rust tests would be testing a fiction. Reusing
 * this one means a fixture change lands in both suites at once.
 *
 * Protocol with the parent, deliberately minimal:
 *
 *   * `--vault <name>`   which fixture to serve (see `VAULTS`)
 *   * stdout line 1      the JSON `{"url": "...", "database": "..."}` handshake
 *   * stdout later       one JSON line per command result
 *   * stdin              newline-delimited JSON commands (see `COMMANDS`)
 *   * stdin closed       exit 0
 *
 * Exiting on stdin EOF is what guarantees no orphan server survives a parent that
 * crashed or was killed: the pipe closes whatever happens to the parent.
 */
import { createInterface } from "node:readline";
import { MockCouch } from "./mock-couch.mjs";
import { largeVault, smallVault } from "./fixtures.mjs";

/** The fixture vaults the Rust suite can ask for, by `--vault`. */
const VAULTS = {
    /** The standard fixture: live notes, a tombstone, a conflict, an attachment. */
    small: () => smallVault({ milestone: {} }),
    /** Milestone `locked`: mid-rebuild, so `initialize` must report `locked`. */
    locked: () => smallVault({ milestone: { locked: true } }),
    /** Milestone `locked` + `cleaned`: chunks purged, clients must resync. */
    cleaned: () => smallVault({ milestone: { locked: true, cleaned: true } }),
    /** No `obsydian_livesync_version` document at all: `unknown-schema`. */
    "unknown-schema": () => {
        const vault = smallVault({ milestone: {} });
        delete vault.docs.obsydian_livesync_version;
        return vault;
    },
    /** Enough entries that manifest pagination is exercised. */
    large: () => largeVault(40),
};

function usage() {
    process.stderr.write(
        `usage: node test/mock-couch-server.mjs [--vault ${Object.keys(VAULTS).join("|")}] [--auth-status <code>]\n`
    );
}

const args = process.argv.slice(2);
let vaultName = "small";
let authStatus;
for (let index = 0; index < args.length; index += 1) {
    if (args[index] === "--vault") {
        vaultName = args[index + 1];
        index += 1;
    } else if (args[index] === "--auth-status") {
        authStatus = Number(args[index + 1]);
        index += 1;
    } else {
        usage();
        process.exit(2);
    }
}

const build = VAULTS[vaultName];
if (!build) {
    usage();
    process.exit(2);
}
const vault = build();

const couch = new MockCouch({
    docs: vault.docs ?? {},
    localDocs: vault.localDocs ?? {},
    conflicts: vault.conflicts ?? {},
    ...(authStatus !== undefined ? { authStatus } : {}),
});
const url = await couch.listen();

/**
 * Commands the parent can issue. Kept to what a Rust test genuinely cannot do
 * from outside: cause a live edit, and read back the write ledger.
 */
const COMMANDS = {
    /**
     * Adds a note and releases any held change feed, as a real edit would. Ids are
     * lower-cased paths, matching upstream's `path2id_base`.
     */
    "push-note": ({ path, text }) => {
        const id = `h:pushed-${Date.now()}`;
        couch.putDoc(id, { type: "leaf", data: text });
        couch.pushChange(path.toLowerCase(), {
            path,
            children: [id],
            size: text.length,
            ctime: Date.now(),
            mtime: Date.now(),
            type: "plain",
            eden: {},
        });
        return { pushed: path };
    },
    /**
     * Every request that would have MUTATED the remote. Must stay empty: the
     * sidecar is structurally read-only and this is the transport-level proof.
     */
    writes: () => ({ writes: couch.writes }),
    /** Requests this mock does not model; a non-empty list means upstream moved. */
    unhandled: () => ({ unhandled: couch.unhandled }),
};

// The handshake line. Written before any command is read so the parent can wait
// for exactly one line and know the server is listening.
process.stdout.write(`${JSON.stringify({ url, database: couch.dbName })}\n`);

const input = createInterface({ input: process.stdin });
input.on("line", (line) => {
    const text = line.trim();
    if (!text) return;
    let response;
    try {
        const message = JSON.parse(text);
        const command = COMMANDS[message.command];
        response = command
            ? { ok: true, ...command(message) }
            : { ok: false, error: `unknown command: ${message.command}` };
    } catch (error) {
        response = { ok: false, error: String(error && error.message ? error.message : error) };
    }
    process.stdout.write(`${JSON.stringify(response)}\n`);
});

// stdin EOF is the stop signal, so the server cannot outlive its parent.
input.on("close", async () => {
    await couch.close();
    process.exit(0);
});
