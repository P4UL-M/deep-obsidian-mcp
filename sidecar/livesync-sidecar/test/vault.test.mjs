/**
 * Data-plane behaviour against the mock vault: enumeration, chunk assembly,
 * conflicts, soft deletes, change feeds, and the read-only guarantee.
 */
import test from "node:test";
import assert from "node:assert/strict";
import { withSidecar } from "./harness.mjs";
import {
    ALPHA_TEXT,
    BETA_TEXT,
    BINARY_BYTES,
    CONFLICTED_TEXT,
    DELETED_TEXT,
    LEGACY_TEXT,
    largeVault,
    smallVault,
} from "./fixtures.mjs";

/** Every path the fixture vault should expose, sorted. */
const ALL_PATHS = [
    "Beta.md",
    "Conflicted.md",
    "Legacy.md",
    "Notes/Alpha.md",
    "Removed.md",
    "assets/logo.png",
];

const vault = () => smallVault({ milestone: {} });

test("manifest lists vault entries and excludes internal and plugin-settings docs", async () => {
    await withSidecar({ vault: vault() }, async ({ sidecar }) => {
        const result = await sidecar.call("manifest", { metaOnly: true });
        assert.equal(result.exhausted, true);
        assert.equal(result.nextCursor, undefined);

        const paths = result.entries.map((entry) => entry.path).sort();
        assert.deepEqual(paths, ALL_PATHS);
        // The `i:` hidden-file entry and the `ps:` plugin-settings entry both
        // exist in the fixture and must not appear: they fall in the gaps
        // between the id ranges upstream scans.
        assert.ok(!paths.some((path) => path.startsWith("i:") || path.startsWith("ps:")));

        const byPath = Object.fromEntries(result.entries.map((entry) => [entry.path, entry]));
        assert.deepEqual(byPath["Notes/Alpha.md"], {
            path: "Notes/Alpha.md",
            size: ALPHA_TEXT.length,
            mtimeMs: 1_700_000_100_000,
            ctimeMs: 1_700_000_000_000,
            deleted: false,
            conflicted: false,
            kind: "markdown",
        });
        assert.equal(byPath["Removed.md"].deleted, true, "soft-deleted entries stay listed, flagged");
        assert.equal(byPath["Conflicted.md"].conflicted, true, "_conflicts must surface");
        assert.equal(byPath["assets/logo.png"].kind, "binary");
    });
});

test("manifest paginates and the cursor is a resumable opaque token", async () => {
    await withSidecar({ vault: vault() }, async ({ sidecar }) => {
        const seen = [];
        let cursor = undefined;
        let pages = 0;
        for (;;) {
            const page = await sidecar.call("manifest", { metaOnly: true, limit: 2, cursor });
            pages += 1;
            seen.push(...page.entries.map((entry) => entry.path));
            if (page.exhausted) {
                assert.equal(page.nextCursor, undefined, "an exhausted page carries no cursor");
                break;
            }
            assert.equal(typeof page.nextCursor, "string");
            cursor = page.nextCursor;
            assert.ok(pages < 20, "pagination must terminate");
        }
        assert.ok(pages > 1, "limit=2 over six entries must span several pages");
        assert.deepEqual(seen.slice().sort(), ALL_PATHS);
        assert.equal(new Set(seen).size, seen.length, "no entry may be emitted twice");
    });
});

test("read assembles text from multiple chunks", async () => {
    await withSidecar({ vault: vault() }, async ({ sidecar }) => {
        const alpha = await sidecar.call("read", { path: "Notes/Alpha.md" });
        assert.equal(alpha.kind, "text");
        assert.equal(alpha.text, ALPHA_TEXT);
        assert.equal(alpha.conflicted, false);
        assert.equal(alpha.deleted, false);
        assert.equal(typeof alpha.rev, "string");

        const beta = await sidecar.call("read", { path: "Beta.md" });
        assert.equal(beta.text, BETA_TEXT);
    });
});

test("read decodes binary entries after joining base64 fragments", async () => {
    await withSidecar({ vault: vault() }, async ({ sidecar }) => {
        const result = await sidecar.call("read", { path: "assets/logo.png" });
        assert.equal(result.kind, "binary");
        // The fixture splits base64 mid-quantum, so per-fragment decoding would
        // corrupt this.
        assert.deepEqual(Buffer.from(result.base64, "base64"), BINARY_BYTES);
    });
});

test("read serves a legacy inline note whose content is not chunked", async () => {
    // Upstream cannot read these through its own metadata path: `getDBEntryMeta`
    // rewrites a `notes` entry's type to `plain` and drops `data`, after which
    // the chunk path finds no children and yields empty content. The sidecar
    // falls back to the raw document.
    await withSidecar({ vault: vault() }, async ({ sidecar }) => {
        const result = await sidecar.call("read", { path: "Legacy.md" });
        assert.equal(result.kind, "text");
        assert.equal(result.text, LEGACY_TEXT);
    });
});

test("read serves the winning revision of a conflicted entry and flags it", async () => {
    await withSidecar({ vault: vault() }, async ({ sidecar }) => {
        const result = await sidecar.call("read", { path: "Conflicted.md" });
        assert.equal(result.conflicted, true);
        assert.equal(result.rev, "3-winningrevision");
        assert.equal(result.text, CONFLICTED_TEXT);
    });
});

test("soft-deleted entries remain readable and flagged", async () => {
    await withSidecar({ vault: vault() }, async ({ sidecar }) => {
        const stat = await sidecar.call("stat", { path: "Removed.md" });
        assert.equal(stat.deleted, true);
        const read = await sidecar.call("read", { path: "Removed.md" });
        assert.equal(read.deleted, true);
        assert.equal(read.text, DELETED_TEXT);
    });
});

test("stat returns metadata without fetching chunks", async () => {
    await withSidecar({ vault: vault() }, async ({ sidecar, couch }) => {
        const before = couch.requests.length;
        const stat = await sidecar.call("stat", { path: "Notes/Alpha.md" });
        assert.equal(stat.kind, "markdown");
        assert.equal(stat.size, ALPHA_TEXT.length);
        const issued = couch.requests.slice(before);
        assert.ok(
            !issued.some((request) => request.startsWith("POST /vault/_all_docs")),
            `stat must not bulk-fetch chunks, got:\n${issued.join("\n")}`
        );
    });
});

test("missing and non-visible paths fail with not-found", async () => {
    await withSidecar({ vault: vault() }, async ({ sidecar }) => {
        for (const path of ["Nope.md", "i:.obsidian/app.json", "ps:some-device"]) {
            const response = await sidecar.send("read", { path });
            assert.ok(response.error, `${path} should not be readable`);
            assert.equal(response.error.code, -32004, path);
            assert.equal(response.error.data.kind, "not-found", path);
        }
    });
});

test("an entry whose chunks are missing reports corrupted-document, not empty content", async () => {
    const broken = vault();
    delete broken.docs["h:beta1"];
    await withSidecar({ vault: broken }, async ({ sidecar }) => {
        const response = await sidecar.send("read", { path: "Beta.md" });
        assert.ok(response.error);
        assert.equal(response.error.data.kind, "corrupted-document");
    });
});

test("changesSince drains the feed and resumes from its cursor", async () => {
    await withSidecar({ vault: vault() }, async ({ sidecar, couch }) => {
        const first = await sidecar.call("changesSince", {});
        assert.equal(first.exhausted, true);
        assert.equal(typeof first.nextCursor, "string");
        assert.deepEqual(first.changes.map((change) => change.path).sort(), ALL_PATHS);

        // Resuming from the cursor yields nothing until the vault moves.
        const empty = await sidecar.call("changesSince", { cursor: first.nextCursor });
        assert.deepEqual(empty.changes, []);

        couch.pushChange("gamma.md", {
            path: "Gamma.md",
            children: ["h:gamma1"],
            size: 6,
            ctime: 1,
            mtime: 2,
            type: "plain",
            eden: {},
        });
        couch.putDoc("h:gamma1", { type: "leaf", data: "gamma\n" });

        const next = await sidecar.call("changesSince", { cursor: empty.nextCursor });
        assert.deepEqual(next.changes, [{ path: "Gamma.md", deleted: false, kind: "markdown" }]);
    });
});

test("changesSince paginates a truncated feed without dropping changes", async () => {
    // The one that matters for incremental sync. Two ways this silently loses
    // data: the sidecar mis-threading `nextCursor`, or the server reporting the
    // database's max seq instead of the last row it returned -- which makes the
    // client resume past everything it did not receive. A real vault is mostly
    // chunk documents, so the feed is always truncated in practice.
    const vault = largeVault(40);
    await withSidecar({ vault }, async ({ sidecar }) => {
        const seen = [];
        let cursor = undefined;
        let pages = 0;
        for (;;) {
            const page = await sidecar.call("changesSince", { cursor, limit: 3 });
            pages += 1;
            // A page may legitimately be empty: the limit can be spent entirely
            // on `leaf` chunk documents, which are filtered out.
            seen.push(...page.changes.map((change) => change.path));
            cursor = page.nextCursor;
            if (page.exhausted) break;
            assert.ok(pages < 500, "pagination must terminate");
        }
        assert.ok(pages > 1, "limit=3 must truncate a 40-note feed");

        const expected = [...vault.bulkPaths, ...ALL_PATHS].sort();
        assert.deepEqual(seen.slice().sort(), expected, "no change may be skipped");
        assert.equal(new Set(seen).size, seen.length, "no change may be reported twice");
    });
});

test("changesSince crosses PouchDB's internal batch size in one call", async () => {
    // Even with a generous caller limit, PouchDB pages the underlying request
    // into batches (25 rows) and resumes from each batch's `last_seq`. A
    // 40-note vault produces ~86 raw changes, so this call spans several
    // batches inside a single `changesSince`.
    const vault = largeVault(40);
    await withSidecar({ vault }, async ({ sidecar, couch }) => {
        const page = await sidecar.call("changesSince", {});
        const expected = [...vault.bulkPaths, ...ALL_PATHS].sort();
        assert.deepEqual(page.changes.map((change) => change.path).sort(), expected);
        assert.equal(page.exhausted, true);
        const feedRequests = couch.requests.filter(
            (request) => request.includes("_changes") && !request.includes("feed=")
        );
        assert.ok(feedRequests.length > 1, `expected several batches, saw:\n${feedRequests.join("\n")}`);
    });
});

test("changesSince does not use the replicate/pull filter", async () => {
    // Upstream's `followUpdates()` asks for `filter=replicate/pull`, a design
    // document no LiveSync client ever creates. If the sidecar regressed to it,
    // the mock would answer 404 for the design doc and this would catch it.
    await withSidecar({ vault: vault() }, async ({ sidecar, couch }) => {
        await sidecar.call("changesSince", {});
        assert.ok(
            !couch.requests.some((request) => request.includes("replicate%2Fpull") || request.includes("replicate/pull")),
            `unexpected filter request:\n${couch.requests.join("\n")}`
        );
    });
});

test("watch emits change notifications and unwatch stops them", async () => {
    await withSidecar({ vault: vault() }, async ({ sidecar, couch }) => {
        const started = await sidecar.call("watch", {});
        assert.equal(started.watching, true);
        assert.equal(typeof started.cursor, "string");

        couch.putDoc("h:delta1", { type: "leaf", data: "delta\n" });
        couch.pushChange("delta.md", {
            path: "Delta.md",
            children: ["h:delta1"],
            size: 6,
            ctime: 1,
            mtime: 2,
            type: "plain",
            eden: {},
        });

        const notification = await sidecar.waitForNotification("change");
        assert.equal(notification.params.path, "Delta.md");
        assert.equal(notification.params.deleted, false);
        assert.equal(notification.params.kind, "markdown");
        assert.equal(typeof notification.params.cursor, "string");

        const health = await sidecar.call("health", {});
        assert.equal(health.watching, true);
        assert.ok(health.pendingChanges >= 1);

        const stopped = await sidecar.call("unwatch", {});
        assert.deepEqual(stopped, { watching: false });
        const after = await sidecar.call("health", {});
        assert.equal(after.watching, false);
    });
});

test("re-watching after unwatch really resubscribes", async () => {
    // A supervisor reconnect loop does exactly this. Upstream's `beginWatch`
    // refuses while its own flag is still set, and that flag clears only in the
    // feed's asynchronous completion handler -- so a naive implementation
    // reports `watching: true` with nothing subscribed.
    await withSidecar({ vault: vault() }, async ({ sidecar, couch }) => {
        await sidecar.call("watch", {});
        await sidecar.call("unwatch", {});
        const again = await sidecar.call("watch", {});
        assert.equal(again.watching, true);

        couch.putDoc("h:epsilon1", { type: "leaf", data: "epsilon\n" });
        couch.pushChange("epsilon.md", {
            path: "Epsilon.md",
            children: ["h:epsilon1"],
            size: 8,
            ctime: 1,
            mtime: 2,
            type: "plain",
            eden: {},
        });

        const notification = await sidecar.waitForNotification("change");
        assert.equal(notification.params.path, "Epsilon.md");
    });
});

test("health status tracks serveability, not the last caller mistake", async () => {
    // `lastError` is informational. If it drove `status`, one read of a mistyped
    // path would mark a healthy vault degraded for the rest of the session --
    // and the supervisor branches on `status`.
    await withSidecar({ vault: vault() }, async ({ sidecar }) => {
        assert.equal((await sidecar.call("health", {})).status, "ok");

        const missing = await sidecar.send("read", { path: "DoesNotExist.md" });
        assert.equal(missing.error.data.kind, "not-found");
        const bad = await sidecar.send("manifest", { limit: -1 });
        assert.equal(bad.error.code, -32602);

        const health = await sidecar.call("health", {});
        assert.equal(health.status, "ok", "a caller-fault error must not degrade health");
        assert.equal(health.lastError, undefined, "caller-fault errors are not recorded");

        // And a real read still works afterwards.
        assert.equal((await sidecar.call("read", { path: "Beta.md" })).kind, "text");
    });
});

test("the sidecar never writes to the remote", async () => {
    // The read-only posture is the whole safety story for pointing this at
    // someone's real vault, so it is asserted at the transport: the mock records
    // every PUT/DELETE and every mutating POST and answers 403.
    await withSidecar({ vault: vault() }, async ({ sidecar, couch }) => {
        await sidecar.call("manifest", { metaOnly: true });
        await sidecar.call("read", { path: "Notes/Alpha.md" });
        await sidecar.call("read", { path: "assets/logo.png" });
        await sidecar.call("stat", { path: "Beta.md" });
        await sidecar.call("changesSince", {});
        await sidecar.call("watch", {});
        await sidecar.call("unwatch", {});
        assert.deepEqual(couch.writes, [], "the sidecar attempted a write");
        assert.deepEqual(couch.unhandled, [], "the sidecar issued a request the mock does not model");
    });
});
