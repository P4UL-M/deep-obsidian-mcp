/**
 * The write plane: compare-and-swap, soft delete, conflict enumeration, and the
 * guarantees that make a retry safe.
 *
 * Everything here runs against a *writable* mock CouchDB, which models real
 * update conflicts (`PUT` compares `_rev` and answers 409). That is what makes
 * the CAS assertions worth anything: a mock that accepted every write would let
 * the sidecar revert to upstream's force-write semantics and still pass.
 */
import test from "node:test";
import assert from "node:assert/strict";
import { withCouch, withSidecar } from "./harness.mjs";
import { BETA_TEXT, DELETED_TEXT, writableVault } from "./fixtures.mjs";

/** Long enough that content-defined chunking produces many leaves. */
const MULTI_CHUNK_TEXT = Array.from(
    { length: 400 },
    (index) => `line ${index} ${"lorem ipsum dolor sit amet ".repeat(2)}`
).join("\n");

const rw = (extra = {}) => ({
    vault: writableVault(),
    writable: true,
    mode: "read-write",
    ...extra,
});

/** Sends `write` and returns the raw envelope, so errors can be inspected. */
function write(sidecar, params) {
    return sidecar.send("write", params);
}

function conflictOf(response) {
    assert.ok(response.error, `expected a conflict, got ${JSON.stringify(response.result)}`);
    assert.equal(response.error.code, -32008);
    assert.equal(response.error.data.kind, "conflict");
    return response.error.data.conflict;
}

/* -------------------------------------------------------------------------- */
/* CAS matrix                                                                  */
/* -------------------------------------------------------------------------- */

test("create-only writes a new entry and reports it as created", async () => {
    await withSidecar(rw(), async ({ sidecar, couch }) => {
        const result = await sidecar.call("write", {
            path: "Notes/Fresh.md",
            content: { kind: "text", text: "brand new\n" },
            baseRev: null,
        });
        assert.match(result.rev, /^1-/);
        assert.equal(result.created, true);
        assert.equal(result.resurrected, false);
        assert.equal(result.conflicted, false);
        assert.equal(result.kind, "markdown");
        assert.equal(result.size, Buffer.byteLength("brand new\n"));

        // Upstream lower-cases entry ids; the real case survives in `path`.
        const stored = couch.docs.get("notes/fresh.md");
        assert.equal(stored.path, "Notes/Fresh.md");
        assert.equal(stored.type, "plain");
        assert.equal(stored.deleted, undefined);
    });
});

test("create-only over an existing entry is a conflict carrying the current rev", async () => {
    await withSidecar(rw(), async ({ sidecar, couch }) => {
        const detail = conflictOf(
            await write(sidecar, {
                path: "Beta.md",
                content: { kind: "text", text: "clobbered\n" },
                baseRev: null,
            })
        );
        assert.equal(detail.currentRev, couch.docs.get("beta.md")._rev);
        assert.equal(detail.expected, null);
        assert.equal(detail.deleted, false);
        // Nothing was rooted: the entry still reads as it did.
        const read = await sidecar.call("read", { path: "Beta.md" });
        assert.equal(read.text, BETA_TEXT);
    });
});

test("create-only over a soft-deleted entry is a conflict that says so", async () => {
    await withSidecar(rw(), async ({ sidecar }) => {
        const detail = conflictOf(
            await write(sidecar, {
                path: "Removed.md",
                content: { kind: "text", text: "recreated\n" },
                baseRev: null,
            })
        );
        // The distinction a host needs: the path looks free but the document is
        // still there, so the right move is a resurrect, not a create.
        assert.equal(detail.deleted, true);
        assert.match(detail.currentRev, /^1-/);
    });
});

test("a guarded update on the current revision succeeds", async () => {
    await withSidecar(rw(), async ({ sidecar, couch }) => {
        const before = couch.docs.get("beta.md")._rev;
        const result = await sidecar.call("write", {
            path: "Beta.md",
            content: { kind: "text", text: "second draft\n" },
            baseRev: before,
        });
        assert.notEqual(result.rev, before);
        assert.equal(result.created, false);
        const read = await sidecar.call("read", { path: "Beta.md" });
        assert.equal(read.text, "second draft\n");
        assert.equal(read.rev, result.rev);
    });
});

test("a guarded update on a stale revision is refused, and changes nothing", async () => {
    await withSidecar(rw(), async ({ sidecar, couch }) => {
        const detail = conflictOf(
            await write(sidecar, {
                path: "Beta.md",
                content: { kind: "text", text: "stale writer\n" },
                baseRev: "9-notarealrevision",
            })
        );
        assert.equal(detail.currentRev, couch.docs.get("beta.md")._rev);
        assert.equal(detail.expected, "9-notarealrevision");
        assert.equal((await sidecar.call("read", { path: "Beta.md" })).text, BETA_TEXT);
    });
});

test("a guarded update on a path with no entry is a conflict without a current rev", async () => {
    await withSidecar(rw(), async ({ sidecar }) => {
        const detail = conflictOf(
            await write(sidecar, {
                path: "Notes/NeverExisted.md",
                content: { kind: "text", text: "x\n" },
                baseRev: "1-something",
            })
        );
        assert.equal(detail.currentRev, undefined);
        assert.equal(detail.expected, "1-something");
    });
});

test("an unguarded upsert overwrites whatever is there", async () => {
    await withSidecar(rw(), async ({ sidecar }) => {
        const result = await sidecar.call("write", {
            path: "Beta.md",
            content: { kind: "text", text: "no questions asked\n" },
        });
        assert.match(result.rev, /^2-/);
        assert.equal((await sidecar.call("read", { path: "Beta.md" })).text, "no questions asked\n");
    });
});

test("writing over a soft-deleted entry resurrects it", async () => {
    await withSidecar(rw(), async ({ sidecar, couch }) => {
        const stat = await sidecar.call("stat", { path: "Removed.md" });
        assert.equal(stat.deleted, true);

        const result = await sidecar.call("write", {
            path: "Removed.md",
            content: { kind: "text", text: "back from the dead\n" },
            baseRev: stat.rev,
        });
        assert.equal(result.resurrected, true);
        assert.equal(result.created, false);

        // Upstream's `putDBEntry` builds the root document from scratch, so the
        // `deleted` flag is simply not carried over -- resurrection is structural,
        // not a special case.
        assert.equal(couch.docs.get("removed.md").deleted, undefined);
        const read = await sidecar.call("read", { path: "Removed.md" });
        assert.equal(read.deleted, false);
        assert.equal(read.text, "back from the dead\n");
        assert.notEqual(read.text, DELETED_TEXT);
    });
});

/* -------------------------------------------------------------------------- */
/* delete                                                                      */
/* -------------------------------------------------------------------------- */

test("delete soft-deletes: the entry stays listed, readable and chunked", async () => {
    await withSidecar(rw(), async ({ sidecar, couch }) => {
        const before = couch.docs.get("beta.md");
        const result = await sidecar.call("delete", { path: "Beta.md", baseRev: before._rev });
        assert.equal(result.deleted, true);
        assert.notEqual(result.rev, before._rev);

        const stored = couch.docs.get("beta.md");
        assert.equal(stored.deleted, true);
        // NOT a CouchDB tombstone, and the chunks are still referenced.
        assert.equal(stored._deleted, undefined);
        assert.deepEqual(stored.children, before.children);
        assert.ok(stored.mtime >= before.mtime);

        const read = await sidecar.call("read", { path: "Beta.md" });
        assert.equal(read.deleted, true);
        assert.equal(read.text, BETA_TEXT);

        const manifest = await sidecar.call("manifest", { metaOnly: true });
        const entry = manifest.entries.find((candidate) => candidate.path === "Beta.md");
        assert.equal(entry.deleted, true);
    });
});

test("an unguarded delete needs no revision", async () => {
    await withSidecar(rw(), async ({ sidecar, couch }) => {
        await sidecar.call("delete", { path: "Beta.md" });
        assert.equal(couch.docs.get("beta.md").deleted, true);
    });
});

test("a guarded delete on a stale revision is refused", async () => {
    await withSidecar(rw(), async ({ sidecar, couch }) => {
        const detail = conflictOf(await sidecar.send("delete", { path: "Beta.md", baseRev: "7-stale" }));
        assert.equal(detail.currentRev, couch.docs.get("beta.md")._rev);
        assert.equal(couch.docs.get("beta.md").deleted, undefined);
    });
});

test("deleting a path with no entry is not-found, not a silent success", async () => {
    await withSidecar(rw(), async ({ sidecar }) => {
        const response = await sidecar.send("delete", { path: "Notes/Ghost.md" });
        assert.equal(response.error.code, -32004);
        assert.equal(response.error.data.kind, "not-found");
    });
});

/* -------------------------------------------------------------------------- */
/* Read-your-write                                                             */
/* -------------------------------------------------------------------------- */

test("a written entry reads back through a second sidecar process", async () => {
    await withCouch(rw(), async ({ open }) => {
        const writer = (await open()).sidecar;
        const written = await writer.call("write", {
            path: "Notes/Shared.md",
            content: { kind: "text", text: MULTI_CHUNK_TEXT },
            baseRev: null,
        });

        // A second process shares no chunk cache and no PouchDB handle, so this
        // proves the bytes really round-tripped through the remote.
        const reader = (await open()).sidecar;
        const read = await reader.call("read", { path: "Notes/Shared.md" });
        assert.equal(read.text, MULTI_CHUNK_TEXT);
        assert.equal(read.rev, written.rev);
        assert.equal(read.size, written.size);

        const stat = await reader.call("stat", { path: "Notes/Shared.md" });
        assert.equal(stat.kind, "markdown");
        assert.equal(stat.deleted, false);
    });
});

/* -------------------------------------------------------------------------- */
/* Publication order and binary content                                        */
/* -------------------------------------------------------------------------- */

test("every chunk is published before the entry root", async () => {
    await withCouch(rw(), async ({ couch, open }) => {
        const { sidecar } = await open();
        await sidecar.call("write", {
            path: "Notes/Ordered.md",
            content: { kind: "text", text: MULTI_CHUNK_TEXT },
            baseRev: null,
        });

        const stored = couch.docs.get("notes/ordered.md");
        assert.ok(stored.children.length > 1, `expected several chunks, got ${stored.children.length}`);

        // The invariant: an interrupted write can leave orphan chunks but never a
        // root pointing at chunks that were never stored.
        const rootIndex = couch.mutations.findIndex((mutation) => mutation.id === "notes/ordered.md");
        assert.notEqual(rootIndex, -1, "the entry root was never written");
        for (const mutation of couch.mutations.slice(0, rootIndex)) {
            assert.equal(mutation.type, "leaf", `${mutation.id} was written before the chunks`);
        }
        assert.equal(couch.mutations.at(-1).id, "notes/ordered.md");
        // Every chunk the root references really exists.
        for (const child of stored.children) {
            assert.ok(couch.docs.has(child), `chunk ${child} is missing`);
        }
    });
});

test("binary content round-trips as a newnote entry", async () => {
    // Deliberately not valid UTF-8. It also has to be *big*: binary content is
    // base64'd before splitting and the default splitter only breaks it at around
    // 100 KiB of base64, so a small attachment is legitimately a single chunk.
    const bytes = Buffer.alloc(256 * 1024);
    for (let index = 0; index < bytes.length; index += 1) bytes[index] = (index * 37) % 256;
    const base64 = bytes.toString("base64");

    await withCouch(rw(), async ({ couch, open }) => {
        const writer = (await open()).sidecar;
        const written = await writer.call("write", {
            path: "assets/blob.bin",
            content: { kind: "binary", base64 },
            baseRev: null,
        });
        assert.equal(written.kind, "binary");
        assert.equal(written.size, bytes.length);
        assert.equal(couch.docs.get("assets/blob.bin").type, "newnote");
        assert.ok(couch.docs.get("assets/blob.bin").children.length > 1);

        const read = await (await open()).sidecar.call("read", { path: "assets/blob.bin" });
        assert.equal(read.kind, "binary");
        assert.ok(Buffer.from(read.base64, "base64").equals(bytes));
    });
});

test("invalid base64 is rejected at the boundary, not stored truncated", async () => {
    await withSidecar(rw(), async ({ sidecar, couch }) => {
        const response = await sidecar.send("write", {
            path: "assets/bad.bin",
            content: { kind: "binary", base64: "not base64 at all!!" },
            baseRev: null,
        });
        assert.equal(response.error.data.kind, "invalid-params");
        assert.deepEqual(couch.mutations, []);
    });
});

/* -------------------------------------------------------------------------- */
/* Retry safety                                                                */
/* -------------------------------------------------------------------------- */

test("a write whose chunks failed leaves no root, and the retry succeeds", async () => {
    await withCouch(rw(), async ({ couch, open }) => {
        const { sidecar } = await open();
        couch.failNextWrites = 1;

        const failed = await write(sidecar, {
            path: "Notes/Retried.md",
            content: { kind: "text", text: MULTI_CHUNK_TEXT },
            baseRev: null,
        });
        assert.ok(failed.error, "the injected 500 should surface");
        assert.equal(couch.docs.has("notes/retried.md"), false, "no root may exist without its chunks");

        // Same content, same precondition: content-addressed chunk ids make the
        // retry idempotent at the chunk layer.
        const retried = await sidecar.call("write", {
            path: "Notes/Retried.md",
            content: { kind: "text", text: MULTI_CHUNK_TEXT },
            baseRev: null,
        });
        assert.match(retried.rev, /^1-/);
        assert.equal((await sidecar.call("read", { path: "Notes/Retried.md" })).text, MULTI_CHUNK_TEXT);
    });
});

test("a dropped root response makes the retry fail with a conflict, never a double write", async () => {
    await withCouch(rw(), async ({ couch, open }) => {
        const { sidecar } = await open();
        const baseRev = couch.docs.get("beta.md")._rev;
        couch.dropNextEntryPutResponses = 1;

        const lost = await write(sidecar, {
            path: "Beta.md",
            content: { kind: "text", text: "landed but unacknowledged\n" },
            baseRev,
        });
        assert.ok(lost.error, "the dropped response should surface as an error");
        // The write DID land; the client just never heard about it.
        const landedRev = couch.docs.get("beta.md")._rev;
        assert.notEqual(landedRev, baseRev);

        // A naive retry with the same precondition is safe: it loses the CAS and
        // is told exactly what the remote now holds.
        const detail = conflictOf(
            await write(sidecar, {
                path: "Beta.md",
                content: { kind: "text", text: "landed but unacknowledged\n" },
                baseRev,
            })
        );
        assert.equal(detail.currentRev, landedRev);

        // Re-reading and retrying against the observed revision then works.
        const recovered = await sidecar.call("write", {
            path: "Beta.md",
            content: { kind: "text", text: "landed but unacknowledged\n" },
            baseRev: landedRev,
        });
        assert.notEqual(recovered.rev, landedRev);
    });
});

/* -------------------------------------------------------------------------- */
/* Concurrent writers                                                          */
/* -------------------------------------------------------------------------- */

test("two sidecars writing from the same base revision: exactly one wins", async () => {
    await withCouch(rw(), async ({ couch, open }) => {
        const first = (await open()).sidecar;
        const second = (await open()).sidecar;

        // Both read the same starting point, as two independent hosts would.
        const baseRev = (await first.call("stat", { path: "Beta.md" })).rev;
        assert.equal((await second.call("stat", { path: "Beta.md" })).rev, baseRev);

        const winner = await first.call("write", {
            path: "Beta.md",
            content: { kind: "text", text: "written by the first writer\n" },
            baseRev,
        });

        const loser = await write(second, {
            path: "Beta.md",
            content: { kind: "text", text: "written by the second writer\n" },
            baseRev,
        });
        const detail = conflictOf(loser);
        assert.equal(detail.currentRev, winner.rev);

        // One winner, and crucially NO conflict branch was created: this is what
        // upstream's force-write would have done instead.
        assert.equal((await first.call("read", { path: "Beta.md" })).text, "written by the first writer\n");
        assert.deepEqual((await first.call("conflicts", { path: "Beta.md" })).conflicts, []);
        assert.deepEqual(couch.unhandled, []);
    });
});

/* -------------------------------------------------------------------------- */
/* conflicts                                                                   */
/* -------------------------------------------------------------------------- */

test("conflicts lists a replication-style sibling revision with its metadata", async () => {
    await withCouch(rw(), async ({ couch, open }) => {
        const { sidecar } = await open();
        // A real conflict cannot be produced through the write API -- that is the
        // point of CAS -- so it is grafted on the way replication would.
        const losingRev = couch.injectConflict("beta.md", {
            path: "Beta.md",
            children: ["h:beta1"],
            size: 11,
            ctime: 1_700_000_200_000,
            mtime: 1_900_000_000_000,
            type: "plain",
            eden: {},
        });

        const result = await sidecar.call("conflicts", { path: "Beta.md" });
        assert.equal(result.winning, couch.docs.get("beta.md")._rev);
        assert.equal(result.conflicts.length, 1);
        assert.deepEqual(result.conflicts[0], {
            rev: losingRev,
            mtimeMs: 1_900_000_000_000,
            size: 11,
            deleted: false,
        });

        // The winning revision is still what `read` serves, and it is flagged.
        const read = await sidecar.call("read", { path: "Beta.md" });
        assert.equal(read.conflicted, true);
        assert.equal(read.text, BETA_TEXT);

        // A guarded write extends the WINNING branch only, so it neither creates
        // nor resolves the conflict -- the sibling leaf survives. `write` reports
        // that from its pre-read rather than spending a round trip re-checking.
        const updated = await sidecar.call("write", {
            path: "Beta.md",
            content: { kind: "text", text: "written over a conflicted entry\n" },
            baseRev: read.rev,
        });
        assert.equal(updated.conflicted, true, "the pre-existing conflict must be reported, not hidden");
        const after = await sidecar.call("conflicts", { path: "Beta.md" });
        assert.equal(after.winning, updated.rev);
        assert.deepEqual(
            after.conflicts.map((entry) => entry.rev),
            [losingRev],
            "the losing revision must survive a guarded write"
        );
    });
});

test("conflicts is empty for a healthy entry and not-found for a missing one", async () => {
    await withSidecar(rw(), async ({ sidecar }) => {
        assert.deepEqual((await sidecar.call("conflicts", { path: "Beta.md" })).conflicts, []);
        const response = await sidecar.send("conflicts", { path: "Notes/Ghost.md" });
        assert.equal(response.error.data.kind, "not-found");
    });
});

test("a conflict revision CouchDB no longer holds is reported, not dropped", async () => {
    // The stock fixture lists a conflict rev with no stored body, which is what a
    // compacted database looks like.
    await withSidecar(rw(), async ({ sidecar }) => {
        const result = await sidecar.call("conflicts", { path: "Conflicted.md" });
        assert.equal(result.conflicts.length, 1);
        assert.equal(result.conflicts[0].unavailable, true);
    });
});

/* -------------------------------------------------------------------------- */
/* Mode gating                                                                 */
/* -------------------------------------------------------------------------- */

test("read-only is the default and refuses write and delete", async () => {
    await withSidecar({ vault: writableVault(), writable: true }, async ({ sidecar, couch, initializeResult }) => {
        assert.equal(initializeResult.mode, "read-only");
        assert.equal((await sidecar.call("health", {})).mode, "read-only");

        for (const [method, params] of [
            ["write", { path: "Notes/Nope.md", content: { kind: "text", text: "x" }, baseRev: null }],
            ["delete", { path: "Beta.md" }],
        ]) {
            const response = await sidecar.send(method, params);
            assert.equal(response.error.code, -32009, `${method} should be refused`);
            assert.equal(response.error.data.kind, "read-only");
        }

        // Refused before any request reached the remote.
        assert.deepEqual(couch.writes, []);
        assert.deepEqual(couch.mutations, []);

        // A read-only refusal is a caller/configuration fact, not a sick remote.
        const health = await sidecar.call("health", {});
        assert.equal(health.status, "ok");
        assert.equal(health.lastError, undefined);
    });
});

test("conflicts is available in read-only mode: it never writes", async () => {
    await withSidecar({ vault: writableVault(), writable: true }, async ({ sidecar, couch }) => {
        const result = await sidecar.call("conflicts", { path: "Conflicted.md" });
        assert.equal(result.conflicts.length, 1);
        assert.deepEqual(couch.writes, []);
    });
});

test("an explicit read-write mode is echoed by initialize and health", async () => {
    await withSidecar(rw(), async ({ sidecar, initializeResult }) => {
        assert.equal(initializeResult.mode, "read-write");
        assert.equal((await sidecar.call("health", {})).mode, "read-write");
        // The pinning triple must not move: the Rust supervisor asserts it field
        // by field, so a capability may never be smuggled into it.
        assert.deepEqual(initializeResult.supported, {
            protocolVersion: 1,
            commonlibVersion: "0.1.2",
            maxSchemaVersion: 12,
            pluginVersionTested: "1.0.3",
        });
    });
});

test("an unknown mode is rejected rather than silently treated as read-only", async () => {
    await withCouch({ vault: writableVault(), writable: true, skipInitialize: true }, async ({ url, open }) => {
        const { sidecar } = await open();
        const response = await sidecar.send("initialize", {
            protocolVersion: 1,
            mode: "write-only",
            couchdb: { url, database: "vault", username: "vaultuser", password: "s3cr3t-password-value" },
        });
        assert.equal(response.error.data.kind, "invalid-params");
    });
});

test("write methods still fail closed before initialize", async () => {
    await withCouch({ vault: writableVault(), writable: true, skipInitialize: true }, async ({ open }) => {
        const { sidecar } = await open();
        for (const method of ["write", "delete", "conflicts"]) {
            const response = await sidecar.send(method, { path: "Beta.md", content: { kind: "text", text: "x" } });
            assert.equal(response.error.data.kind, "not-initialized", `${method} must fail closed`);
        }
    });
});

/* -------------------------------------------------------------------------- */
/* What a writer must still never touch                                        */
/* -------------------------------------------------------------------------- */

test("in read-write mode only entry and chunk documents are ever written", async () => {
    await withCouch(rw(), async ({ couch, open }) => {
        const { sidecar } = await open();
        await sidecar.call("write", {
            path: "Notes/Audited.md",
            content: { kind: "text", text: MULTI_CHUNK_TEXT },
            baseRev: null,
        });
        await sidecar.call("write", {
            path: "assets/audited.bin",
            content: { kind: "binary", base64: Buffer.alloc(4096, 9).toString("base64") },
            baseRev: null,
        });
        await sidecar.call("delete", { path: "Notes/Audited.md" });
        await sidecar.call("manifest", { metaOnly: true });
        await sidecar.call("changesSince", {});

        assert.ok(couch.mutations.length > 0, "the test wrote nothing, so it proves nothing");
        for (const mutation of couch.mutations) {
            assert.ok(
                mutation.type === "leaf" || mutation.type === "plain" || mutation.type === "newnote",
                `unexpected document type written: ${JSON.stringify(mutation)}`
            );
            // The three control documents a LiveSync client is tempted to write.
            assert.equal(mutation.id.startsWith("_local/"), false, `control document written: ${mutation.id}`);
            assert.notEqual(mutation.id, "obsydian_livesync_version");
        }
        // And nothing destructive: no compaction, no purge, no CouchDB tombstone.
        for (const request of couch.writes) {
            assert.ok(
                request.startsWith("POST /vault/_bulk_docs") || request.startsWith("PUT /vault/"),
                `unexpected mutating request: ${request}`
            );
        }
        assert.deepEqual(couch.unhandled, [], "a new upstream request shape appeared");
    });
});

test("writes are refused while the remote is not serveable, even in read-write mode", async () => {
    // Fail-closed is the whole point: a vault we cannot classify must not be
    // written to just because the host asked for read-write.
    const vault = writableVault();
    delete vault.docs.obsydian_livesync_version;
    await withSidecar(
        { vault, writable: true, mode: "read-write" },
        async ({ sidecar, couch, initializeResult }) => {
            assert.equal(initializeResult.compatibility.status, "unknown-schema");
            const response = await sidecar.send("write", {
                path: "Notes/Nope.md",
                content: { kind: "text", text: "x" },
                baseRev: null,
            });
            assert.equal(response.error.data.kind, "incompatible-remote");
            assert.equal(response.error.data.status, "unknown-schema");
            assert.deepEqual(couch.mutations, []);
        }
    );
});
