/**
 * The compatibility gate.
 *
 * A remote-side problem is never a JSON-RPC error on `initialize`: the call
 * succeeds and reports a `CompatibilityStatus`, so the supervisor gets one
 * precise reason to show a user. Data methods then refuse with
 * `incompatible-remote`, carrying that same status.
 */
import test from "node:test";
import assert from "node:assert/strict";
import { Sidecar, withSidecar, PROTOCOL_VERSION } from "./harness.mjs";
import { encryptedVault, smallVault } from "./fixtures.mjs";

/** Asserts initialize reported `status`, and that reads are then refused. */
async function expectStatus(options, status, detailPattern) {
    await withSidecar(options, async ({ sidecar, initializeResult }) => {
        assert.equal(
            initializeResult.compatibility.status,
            status,
            `expected ${status}, got ${JSON.stringify(initializeResult.compatibility)}`
        );
        if (detailPattern) {
            assert.match(initializeResult.compatibility.detail ?? "", detailPattern);
        }

        const read = await sidecar.send("read", { path: "Beta.md" });
        assert.ok(read.error, "read must be refused while the remote is not serveable");
        assert.equal(read.error.code, -32003);
        assert.equal(read.error.data.kind, "incompatible-remote");
        assert.equal(read.error.data.status, status);

        const manifest = await sidecar.send("manifest", { metaOnly: true });
        assert.equal(manifest.error.data.kind, "incompatible-remote");

        const health = await sidecar.call("health", {});
        assert.equal(health.status, "degraded");
        assert.equal(health.compatibility.status, status);
    });
}

test("a newer schema version is refused as unknown-schema", async () => {
    await expectStatus(
        { vault: smallVault({ schemaVersion: 13, milestone: {} }) },
        "unknown-schema",
        /13 is newer than the supported maximum 12/
    );
});

test("an older schema version is still readable", async () => {
    await withSidecar({ vault: smallVault({ schemaVersion: 11, milestone: {} }) }, async ({ sidecar, initializeResult }) => {
        assert.equal(initializeResult.compatibility.status, "ok");
        assert.equal(initializeResult.remote.schemaVersion, 11);
        const read = await sidecar.call("read", { path: "Beta.md" });
        assert.equal(read.kind, "text");
    });
});

test("a database with no version document is not treated as a vault", async () => {
    const vault = smallVault({ milestone: {} });
    delete vault.docs["obsydian_livesync_version"];
    await expectStatus({ vault }, "unknown-schema", /has no obsydian_livesync_version document/);
});

test("a malformed version document is refused", async () => {
    const vault = smallVault({ milestone: {} });
    vault.docs["obsydian_livesync_version"] = { type: "versioninfo" };
    await expectStatus({ vault }, "unknown-schema", /malformed/);
});

test("a locked milestone stops reads", async () => {
    await expectStatus(
        { vault: smallVault({ milestone: { locked: true } }) },
        "locked",
        /rebuild or cleanup is in progress/
    );
});

test("a locked and cleaned milestone reports cleaned, not locked", async () => {
    await expectStatus(
        { vault: smallVault({ milestone: { locked: true, cleaned: true } }) },
        "cleaned",
        /chunks were purged/
    );
});

test("accepted nodes with no common chunk version report incompatible", async () => {
    await expectStatus(
        {
            vault: smallVault({
                milestone: {
                    accepted_nodes: ["node-a", "node-b"],
                    node_chunk_info: {
                        "node-a": { min: 3, max: 4, current: 3 },
                        "node-b": { min: 0, max: 1, current: 1 },
                    },
                },
            }),
        },
        "incompatible",
        /no common chunk format version/
    );
});

test("an unknown accepted node is treated as version 0..0, like upstream", async () => {
    await expectStatus(
        {
            vault: smallVault({
                milestone: {
                    accepted_nodes: ["node-a", "ghost-node"],
                    node_chunk_info: { "node-a": { min: 2, max: 3, current: 2 } },
                },
            }),
        },
        "incompatible"
    );
});

test("a vault with no milestone document at all is readable", async () => {
    // A brand-new remote may legitimately have no milestone yet; refusing would
    // be stricter than the plugin itself.
    await withSidecar({ vault: smallVault() }, async ({ sidecar, initializeResult }) => {
        assert.equal(initializeResult.compatibility.status, "ok");
        const read = await sidecar.call("read", { path: "Beta.md" });
        assert.equal(read.kind, "text");
    });
});

test("preferred tweak values that require a passphrase we lack report mismatched", async () => {
    await expectStatus(
        {
            vault: smallVault({
                milestone: { tweak_values: { PREFERRED: { encrypt: true, usePathObfuscation: false } } },
            }),
        },
        "mismatched",
        /prefers end-to-end encryption/
    );
});

test("preferred tweak values that only differ on writer knobs are accepted", async () => {
    // Upstream compares its whole should-match template, including chunk sizes
    // and splitter versions. A reader that disagrees about those still reads
    // correctly, so flagging them would refuse healthy vaults.
    await withSidecar(
        {
            vault: smallVault({
                milestone: {
                    tweak_values: {
                        PREFERRED: { customChunkSize: 99, chunkSplitterVersion: 3, minimumChunkSize: 40 },
                    },
                },
            }),
        },
        async ({ initializeResult }) => {
            assert.equal(initializeResult.compatibility.status, "ok");
        }
    );
});

test("rejected credentials report auth-failed", async () => {
    await expectStatus({ vault: smallVault({ milestone: {} }), authStatus: 401 }, "auth-failed", /HTTP 401/);
});

test("a forbidden response also reports auth-failed", async () => {
    await expectStatus({ vault: smallVault({ milestone: {} }), authStatus: 403 }, "auth-failed", /HTTP 403/);
});

test("an unreachable server reports unreachable", async () => {
    // Port 1 on loopback: nothing listens, so the connection is refused
    // immediately rather than hanging on a timeout.
    const sidecar = new Sidecar();
    try {
        const result = await sidecar.call("initialize", {
            protocolVersion: PROTOCOL_VERSION,
            couchdb: { url: "http://127.0.0.1:1", database: "vault", username: "u", password: "pw" },
        });
        assert.equal(result.compatibility.status, "unreachable");
        const read = await sidecar.send("read", { path: "Beta.md" });
        assert.equal(read.error.data.status, "unreachable");
    } finally {
        await sidecar.shutdown();
    }
});

test("encrypted chunks without a passphrase report e2ee-required", async () => {
    await expectStatus({ vault: encryptedVault() }, "e2ee-required", /no passphrase was supplied/);
});

test("a passphrase without a replication salt reports e2ee-invalid instead of writing one", async () => {
    // Upstream's salt handler *creates* `_local/obsidian_livesync_sync_parameters`
    // when it is missing. A read-only client must refuse rather than write, so
    // the absence of the salt is a hard stop.
    await withSidecar(
        { vault: encryptedVault({ withSalt: false }), e2ee: { passphrase: "correct horse battery staple" } },
        async ({ sidecar, couch, initializeResult }) => {
            assert.equal(initializeResult.compatibility.status, "e2ee-invalid");
            assert.match(initializeResult.compatibility.detail, /sync_parameters/);
            assert.deepEqual(couch.writes, [], "the sidecar must not create the sync-parameters document");
            const read = await sidecar.send("read", { path: "Beta.md" });
            assert.equal(read.error.data.status, "e2ee-invalid");
        }
    );
});

test("a passphrase that cannot decrypt a chunk reports e2ee-invalid", async () => {
    // The salt is present, so the write-on-missing-salt path is not what stops
    // this: the sidecar actually attempts a chunk read and the placeholder
    // ciphertext fails to decrypt.
    await withSidecar(
        { vault: encryptedVault({ withSalt: true }), e2ee: { passphrase: "wrong passphrase entirely" } },
        async ({ sidecar, couch, initializeResult }) => {
            assert.equal(initializeResult.compatibility.status, "e2ee-invalid");
            assert.deepEqual(couch.writes, []);
            const read = await sidecar.send("read", { path: "Beta.md" });
            assert.equal(read.error.data.status, "e2ee-invalid");
        }
    );
});

test("initialize reports the remote's encryption and obfuscation shape", async () => {
    await withSidecar({ vault: encryptedVault() }, async ({ initializeResult }) => {
        assert.equal(initializeResult.remote.encrypted, true);
        assert.equal(initializeResult.remote.pathObfuscation, false);
    });
});

test("obfuscated ids without an obfuscation passphrase are refused", async () => {
    const vault = smallVault({ milestone: {} });
    // An `f:`-prefixed entry is what path obfuscation produces.
    vault.docs["f:0123456789abcdef"] = {
        path: "f:0123456789abcdef",
        children: ["h:beta1"],
        size: 1,
        ctime: 1,
        mtime: 1,
        type: "plain",
        eden: {},
    };
    await expectStatus({ vault }, "e2ee-required", /obfuscated document ids/);
});
