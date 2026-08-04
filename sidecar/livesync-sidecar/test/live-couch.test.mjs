/**
 * Optional tests against a REAL CouchDB, gated on `DEEP_OBSIDIAN_COUCHDB_URL`.
 *
 * ```sh
 * docker run -d --name couch -p 5984:5984 \
 *   -e COUCHDB_USER=admin -e COUCHDB_PASSWORD=pw couchdb:3
 * curl -X PUT http://admin:pw@127.0.0.1:5984/livevault
 * DEEP_OBSIDIAN_COUCHDB_URL=http://127.0.0.1:5984 \
 * DEEP_OBSIDIAN_COUCHDB_DB=livevault \
 * DEEP_OBSIDIAN_COUCHDB_USER=admin \
 * DEEP_OBSIDIAN_COUCHDB_PASSWORD=pw npm test
 * ```
 *
 * Skipped, not failed, when the variable is absent: the hermetic suite is the
 * contract and CI must not require a container.
 *
 * Two things only a real server can prove:
 *
 *   1. **Fail-closed.** A database CouchDB accepts but the sidecar cannot classify
 *      -- no `obsydian_livesync_version` document, i.e. not a LiveSync vault --
 *      must be refused for writing even though the host explicitly asked for
 *      `read-write` and the credentials are valid. Getting this wrong would mean
 *      scribbling entry documents into an unrelated database.
 *   2. **That the mock's conflict model is the real one.** `test/write.test.mjs`
 *      trusts `MockCouch` to answer 409 exactly as CouchDB does; the CAS matrix is
 *      replayed here against the real server, with real revision hashes, so the
 *      hermetic suite cannot be passing against a fiction.
 *
 * The write test creates and drops its OWN scratch database
 * (`SCRATCH_DATABASE`), so it never touches the one named in the environment.
 * Point this at a throwaway container regardless.
 */
import test, { after } from "node:test";
import assert from "node:assert/strict";
import { Sidecar, PROTOCOL_VERSION } from "./harness.mjs";
import { SCHEMA_VERSION } from "./fixtures.mjs";

const url = process.env.DEEP_OBSIDIAN_COUCHDB_URL;
const database = process.env.DEEP_OBSIDIAN_COUCHDB_DB ?? "livevault";
const username = process.env.DEEP_OBSIDIAN_COUCHDB_USER ?? "admin";
const password = process.env.DEEP_OBSIDIAN_COUCHDB_PASSWORD ?? "";

/** Created and destroyed by this file; never the configured database. */
const SCRATCH_DATABASE = "deep-obsidian-sidecar-writetest";

const options = { skip: url ? false : "set DEEP_OBSIDIAN_COUCHDB_URL to run the live CouchDB tests" };

function authHeaders() {
    return {
        authorization: `Basic ${Buffer.from(`${username}:${password}`).toString("base64")}`,
        "content-type": "application/json",
    };
}

async function couch(method, requestPath, body) {
    const response = await fetch(`${url}/${requestPath}`, {
        method,
        headers: authHeaders(),
        ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
    });
    return { status: response.status, body: await response.json().catch(() => undefined) };
}

async function withLiveSidecar(mode, body, db = database) {
    const sidecar = new Sidecar();
    try {
        const initializeResult = await sidecar.call("initialize", {
            protocolVersion: PROTOCOL_VERSION,
            ...(mode ? { mode } : {}),
            couchdb: { url, database: db, username, password },
            options: { requestTimeoutMs: 15_000 },
        });
        await body({ sidecar, initializeResult });
    } finally {
        try {
            await sidecar.shutdown();
        } catch {
            await sidecar.kill();
        }
    }
}

/** Every document id currently in a live database. */
async function liveDocIds(db = database) {
    const { body } = await couch("GET", `${db}/_all_docs`);
    return (body?.rows ?? []).map((row) => row.id);
}

/**
 * A minimal but real LiveSync-shaped database: the version document the gate
 * requires, plus a milestone with one accepted node.
 */
async function seedScratchVault() {
    await couch("DELETE", SCRATCH_DATABASE);
    await couch("PUT", SCRATCH_DATABASE);
    await couch("PUT", `${SCRATCH_DATABASE}/obsydian_livesync_version`, {
        type: "versioninfo",
        version: SCHEMA_VERSION,
    });
    await couch("PUT", `${SCRATCH_DATABASE}/_local%2Fobsydian_livesync_milestone`, {
        type: "milestoneinfo",
        created: 1_700_000_000_000,
        locked: false,
        accepted_nodes: ["node-a"],
        node_chunk_info: { "node-a": { min: 0, max: 2, current: 2 } },
        node_info: {},
        tweak_values: {},
    });
}

after(async () => {
    if (url) await couch("DELETE", SCRATCH_DATABASE);
});

test("a real CouchDB with an unknown schema is classified, not crashed on", options, async () => {
    await withLiveSidecar(undefined, async ({ initializeResult }) => {
        assert.equal(initializeResult.compatibility.status, "unknown-schema");
        assert.equal(initializeResult.mode, "read-only");
    });
});

test("read-write mode still refuses to write an unclassifiable real database", options, async () => {
    const before = await liveDocIds();

    await withLiveSidecar("read-write", async ({ sidecar, initializeResult }) => {
        // The host asked for read-write and the credentials are good. The gate is
        // what stops the write, and it must stop it.
        assert.equal(initializeResult.mode, "read-write");
        assert.equal(initializeResult.compatibility.status, "unknown-schema");

        const write = await sidecar.send("write", {
            path: "Notes/ShouldNeverExist.md",
            content: { kind: "text", text: "this must not reach the remote\n" },
            baseRev: null,
        });
        assert.equal(write.error.data.kind, "incompatible-remote");
        assert.equal(write.error.data.status, "unknown-schema");

        const remove = await sidecar.send("delete", { path: "Notes/ShouldNeverExist.md" });
        assert.equal(remove.error.data.kind, "incompatible-remote");

        // Not even the control documents a LiveSync client would normally create.
        const remaining = await liveDocIds();
        assert.deepEqual(remaining, before, "the sidecar modified an unclassifiable database");
    });
});

test("compare-and-swap behaves identically against a real CouchDB", options, async () => {
    await seedScratchVault();
    // Long enough to span many chunks, so the real `_bulk_docs` path runs too.
    const text = Array.from({ length: 300 }, (_, index) => `live line ${index} ${"y".repeat(40)}`).join("\n");

    await withLiveSidecar(
        "read-write",
        async ({ sidecar, initializeResult }) => {
            assert.equal(initializeResult.compatibility.status, "ok");

            const created = await sidecar.call("write", {
                path: "Notes/Live.md",
                content: { kind: "text", text },
                baseRev: null,
            });
            assert.match(created.rev, /^1-[0-9a-f]{32}$/, "a real CouchDB revision hash was expected");

            // The four CAS outcomes, against the real conflict adjudicator.
            const recreate = await sidecar.send("write", {
                path: "Notes/Live.md",
                content: { kind: "text", text: "x" },
                baseRev: null,
            });
            assert.equal(recreate.error.data.kind, "conflict");
            assert.equal(recreate.error.data.conflict.currentRev, created.rev);

            const stale = await sidecar.send("write", {
                path: "Notes/Live.md",
                content: { kind: "text", text: "x" },
                baseRev: "1-00000000000000000000000000000000",
            });
            assert.equal(stale.error.data.conflict.currentRev, created.rev);

            const guarded = await sidecar.call("write", {
                path: "Notes/Live.md",
                content: { kind: "text", text: "updated\n" },
                baseRev: created.rev,
            });
            assert.match(guarded.rev, /^2-/);

            const removed = await sidecar.call("delete", { path: "Notes/Live.md", baseRev: guarded.rev });
            assert.match(removed.rev, /^3-/);

            // Soft, and only ever soft: still there, still readable.
            const stat = await sidecar.call("stat", { path: "Notes/Live.md" });
            assert.equal(stat.deleted, true);
            assert.equal(stat.rev, removed.rev);

            // No conflict branch was created anywhere along the way.
            assert.deepEqual((await sidecar.call("conflicts", { path: "Notes/Live.md" })).conflicts, []);

            // And a guarded write over a GENUINE conflict resolves nothing. The
            // sibling leaf is grafted the way replication would, with
            // `new_edits: false` -- which is the one request shape `MockCouch`
            // refuses to model, so this claim can only be checked here.
            // A SIBLING, not a descendant: same generation as the current leaf
            // (`removed.rev`, generation 3) and sharing its parent (`guarded.rev`,
            // generation 2). Grafting a generation-4 child of the leaf would just
            // extend the branch and resolve nothing.
            const siblingRev = "3-00000000000000000000000000000abc";
            const graft = await couch("POST", `${SCRATCH_DATABASE}/_bulk_docs`, {
                new_edits: false,
                docs: [
                    {
                        _id: "notes/live.md",
                        _rev: siblingRev,
                        _revisions: {
                            start: 3,
                            ids: ["00000000000000000000000000000abc", guarded.rev.split("-")[1]],
                        },
                        path: "Notes/Live.md",
                        children: [],
                        ctime: 1,
                        mtime: 2,
                        size: 3,
                        type: "plain",
                        eden: {},
                    },
                ],
            });
            assert.equal(graft.status, 201, `graft failed: ${JSON.stringify(graft.body)}`);

            const conflicted = await sidecar.call("conflicts", { path: "Notes/Live.md" });
            assert.equal(conflicted.conflicts.length, 1, "the graft did not produce a conflict");
            assert.ok(
                conflicted.winning === siblingRev || conflicted.conflicts[0].rev === siblingRev,
                "the grafted revision is neither the winner nor the loser"
            );
            const losing = conflicted.conflicts[0].rev;

            const overConflict = await sidecar.call("write", {
                path: "Notes/Live.md",
                content: { kind: "text", text: "written over a conflicted entry\n" },
                baseRev: conflicted.winning,
            });
            assert.equal(overConflict.conflicted, true, "the pre-existing conflict must be reported");
            const still = await sidecar.call("conflicts", { path: "Notes/Live.md" });
            assert.equal(still.winning, overConflict.rev);
            assert.deepEqual(
                still.conflicts.map((entry) => entry.rev),
                [losing],
                "a guarded write must extend the winning branch only, leaving the sibling leaf alone"
            );
        },
        SCRATCH_DATABASE
    );

    // A second process reads back what the first wrote, and the control documents
    // are untouched: exactly one entry root plus its chunks were added.
    await withLiveSidecar(
        "read-only",
        async ({ sidecar }) => {
            const read = await sidecar.call("read", { path: "Notes/Live.md" });
            assert.equal(read.text, "written over a conflicted entry\n");
            assert.equal(read.conflicted, true);
        },
        SCRATCH_DATABASE
    );

    const ids = await liveDocIds(SCRATCH_DATABASE);
    assert.ok(ids.includes("notes/live.md"));
    assert.ok(ids.includes("obsydian_livesync_version"));
    for (const id of ids) {
        assert.ok(
            id === "notes/live.md" || id === "obsydian_livesync_version" || id.startsWith("h:"),
            `unexpected document in the live database: ${id}`
        );
    }
    // The sidecar must not have created the sync-parameters document either.
    assert.equal((await couch("GET", `${SCRATCH_DATABASE}/_local%2Fobsidian_livesync_sync_parameters`)).status, 404);
});
