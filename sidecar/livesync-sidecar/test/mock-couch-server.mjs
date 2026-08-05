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
import { loadE2EEFixture } from "./e2ee-fixture.mjs";

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
    /**
     * The committed dump of a REAL E2EE vault: `Notes/Encrypted.md` and
     * `assets/encrypted.bin` are stored as `h:+` chunks of genuine ciphertext, produced
     * by upstream's own key schedule (`npm run fixtures:e2ee`).
     *
     * Exposed to the Rust suite so a test can prove a feature composes with DECRYPTION
     * rather than only with plaintext reads. A caller must supply
     * `E2EE_PASSPHRASE` — without it the handshake reports `e2ee-required`, which is
     * exactly the fixture's other use in the sidecar's own suite.
     */
    e2ee: () => loadE2EEFixture().vault,
};

function usage() {
    process.stderr.write(
        `usage: node test/mock-couch-server.mjs [--vault ${Object.keys(VAULTS).join("|")}] [--auth-status <code>] [--writable]\n`
    );
}

const args = process.argv.slice(2);
let vaultName = "small";
let authStatus;
/**
 * Opt-in, exactly as it is on `MockCouch` itself. The default stays non-writable so
 * the read-only proofs keep asserting against a fixture that would refuse a write
 * even if the sidecar tried one.
 */
let writable = false;
for (let index = 0; index < args.length; index += 1) {
    if (args[index] === "--vault") {
        vaultName = args[index + 1];
        index += 1;
    } else if (args[index] === "--auth-status") {
        authStatus = Number(args[index + 1]);
        index += 1;
    } else if (args[index] === "--writable") {
        writable = true;
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
    ...(writable ? { writable: true } : {}),
});
const url = await couch.listen();

/**
 * Monotonic suffix for pushed chunk ids.
 *
 * `Date.now()` alone is NOT unique: a parent seeding several notes back to back over
 * the pipe lands them inside one millisecond, every push then computes the SAME chunk
 * id, `putDoc` overwrites the previous body, and each of those entries'
 * `children: [id]` aliases the last writer's text. A reader gets the wrong content for
 * every note but the last — silently, and looking exactly like a bug in whatever is
 * reading. The counter makes the id unique per push regardless of timing.
 */
let pushSeq = 0;

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
        pushSeq += 1;
        const id = `h:pushed-${Date.now()}-${pushSeq}`;
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
     * Every request that would have MUTATED the remote. On a non-writable fixture
     * this must stay EMPTY: that is the transport-level proof that a read-only mount
     * never even attempts a write. On a writable one it is the write ledger.
     */
    writes: () => ({ writes: couch.writes }),
    /** Requests this mock does not model; a non-empty list means upstream moved. */
    unhandled: () => ({ unhandled: couch.unhandled }),
    /** Every document write actually APPLIED, as `{method, id, type}` rows. */
    mutations: () => ({ mutations: couch.mutations }),
    /**
     * Answer the next N mutating requests 500 WITHOUT applying them, so the Rust
     * side can drive the supervisor's retry-on-`remote-error` path.
     */
    "fail-next-writes": ({ count }) => {
        couch.failNextWrites = Number(count ?? 1);
        return { failNextWrites: couch.failNextWrites };
    },
    /**
     * Apply the next N entry-root PUTs and then answer 500 — the LOST RESPONSE case.
     * The write lands, the client never hears, and its retry meets a revision that
     * is its own. This is the only way to reach the ambiguity carve-out in the Rust
     * conflict resolver from outside.
     */
    "drop-next-entry-put-responses": ({ count }) => {
        couch.dropNextEntryPutResponses = Number(count ?? 1);
        return { dropNextEntryPutResponses: couch.dropNextEntryPutResponses };
    },
    /**
     * Answer the next N requests of ANY kind 500: a remote OUTAGE, which is the only
     * injection that breaks reads and therefore the only one a resilience test can
     * observe a recovery from.
     *
     * The window is a request COUNT rather than a duration: a caller either bounds it
     * (fail the next 3) or opens it wide and clears it with `count: 0`. Either way the
     * fixture keeps its port for the whole test, so a recovery is observed by re-issuing
     * the operation. Unbinding and rebinding a listener would be the alternative and
     * would race `TIME_WAIT`.
     */
    "fail-next-requests": ({ count }) => {
        couch.failNextRequests = Number(count ?? 1);
        return { failNextRequests: couch.failNextRequests };
    },
    /** Destroy the socket for the next N requests: a connection drop, not a 500. */
    "destroy-next-requests": ({ count }) => {
        couch.destroyNextRequests = Number(count ?? 1);
        return { destroyNextRequests: couch.destroyNextRequests };
    },
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
