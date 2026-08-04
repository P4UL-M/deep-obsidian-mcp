/**
 * End-to-end encryption and path obfuscation, finally on the SUCCESS path.
 *
 * Until this slice the suite could only prove classification: `e2ee-required`
 * and `e2ee-invalid` against synthetic ciphertext. Writing closes that gap
 * hermetically -- the ciphertext, chunk ids, encrypted metadata envelope and
 * obfuscated document ids are produced by `octagonal-wheels` and
 * `livesync-commonlib` through the real sidecar, and only the *database* is a
 * mock. CouchDB does not participate in the codec; it stores what it is given.
 *
 * Two shapes of proof, and both are needed:
 *
 *   * a **round trip** (write here, read back through another process) proves the
 *     encrypt and decrypt halves agree and that a wrong passphrase fails;
 *   * a **committed fixture** (`test/fixtures/e2ee-written-vault.json`, produced
 *     by `npm run fixtures:e2ee`) proves the codec has not *drifted*. A round
 *     trip alone would happily write and read a new, mutually consistent, wrong
 *     format after a commonlib bump.
 */
import test from "node:test";
import assert from "node:assert/strict";
import { withCouch } from "./harness.mjs";
import { PBKDF2_SALT, writableVault } from "./fixtures.mjs";
import {
    E2EE_BINARY_BYTES,
    E2EE_BINARY_PATH,
    E2EE_OBFUSCATED_FIXTURE_PATH,
    E2EE_OBFUSCATED_PATH,
    E2EE_OBFUSCATE_PASSPHRASE,
    E2EE_PASSPHRASE,
    E2EE_PLAIN_PATH,
    E2EE_TEXT,
    E2EE_WRONG_PASSPHRASE,
    loadE2EEFixture,
} from "./e2ee-fixture.mjs";

const encrypting = (e2ee) => ({
    vault: writableVault(),
    writable: true,
    mode: "read-write",
    e2ee,
});

/* -------------------------------------------------------------------------- */
/* Round trip                                                                  */
/* -------------------------------------------------------------------------- */

test("an encrypted write really encrypts, and a second sidecar reads it back", async () => {
    await withCouch(encrypting({ passphrase: E2EE_PASSPHRASE }), async ({ couch, open }) => {
        const writer = await open();
        // The vault holds only plaintext chunks so far, so the gate reports it as
        // unencrypted -- encryption is a per-chunk property, not a vault flag.
        assert.equal(writer.initializeResult.remote.encrypted, false);

        const written = await writer.sidecar.call("write", {
            path: E2EE_PLAIN_PATH,
            content: { kind: "text", text: E2EE_TEXT },
            baseRev: null,
        });

        const stored = couch.docs.get(E2EE_PLAIN_PATH.toLowerCase());
        assert.ok(stored.children.length > 1, "the fixture text should span several chunks");
        for (const child of stored.children) {
            // `h:+` is the id prefix upstream's hash manager produces when a
            // passphrase is configured, and it is what selects the chunk for the
            // decryption transform on the way back.
            assert.ok(child.startsWith("h:+"), `chunk ${child} is not an encrypted chunk id`);
            const chunk = couch.docs.get(child);
            assert.equal(chunk.e_, true, "the encrypted marker is missing");
            assert.ok(chunk.data.startsWith("%="), "the HKDF header is missing");
        }
        // The whole point: no plaintext reached the remote.
        assert.equal(JSON.stringify([...couch.docs.values()]).includes("secret line 0"), false);

        // A second process: no shared chunk cache, no shared PouchDB handle, so
        // this is a genuine decrypt of what is on the wire.
        const reader = await open();
        assert.equal(reader.initializeResult.remote.encrypted, true);
        assert.equal(reader.initializeResult.compatibility.status, "ok");
        const read = await reader.sidecar.call("read", { path: E2EE_PLAIN_PATH });
        assert.equal(read.text, E2EE_TEXT);
        assert.equal(read.rev, written.rev);
    });
});

test("encrypted binary content round-trips byte for byte", async () => {
    await withCouch(encrypting({ passphrase: E2EE_PASSPHRASE }), async ({ couch, open }) => {
        const writer = await open();
        await writer.sidecar.call("write", {
            path: E2EE_BINARY_PATH,
            content: { kind: "binary", base64: E2EE_BINARY_BYTES.toString("base64") },
            baseRev: null,
        });
        assert.equal(couch.docs.get(E2EE_BINARY_PATH).type, "newnote");

        const read = await (await open()).sidecar.call("read", { path: E2EE_BINARY_PATH });
        assert.equal(read.kind, "binary");
        assert.ok(Buffer.from(read.base64, "base64").equals(E2EE_BINARY_BYTES));
    });
});

test("a wrong passphrase cannot read what the right one wrote", async () => {
    await withCouch(encrypting({ passphrase: E2EE_PASSPHRASE }), async ({ open }) => {
        const writer = await open();
        await writer.sidecar.call("write", {
            path: E2EE_PLAIN_PATH,
            content: { kind: "text", text: E2EE_TEXT },
            baseRev: null,
        });

        const wrong = await open({ e2ee: { passphrase: E2EE_WRONG_PASSPHRASE } });
        // The failure surfaces at the compatibility gate rather than at `read`:
        // the gate decrypts one real chunk on purpose, so a bad passphrase is
        // caught before any data method runs. The AEAD tag is what fails -- this is
        // a real authentication failure, not a heuristic.
        assert.equal(wrong.initializeResult.compatibility.status, "e2ee-invalid");
        assert.match(wrong.initializeResult.compatibility.detail, /could not be decrypted/);

        const read = await wrong.sidecar.send("read", { path: E2EE_PLAIN_PATH });
        assert.equal(read.error.data.kind, "incompatible-remote");
        assert.equal(read.error.data.status, "e2ee-invalid");
    });
});

test("an encrypting writer is refused when the remote has no replication salt", async () => {
    // The salt lives in `_local/obsidian_livesync_sync_parameters` and the sidecar
    // refuses to create it in ANY mode. Without this up-front check, the first
    // chunk encryption would fail deep inside the write.
    const vault = writableVault();
    delete vault.localDocs["_local/obsidian_livesync_sync_parameters"];
    await withCouch(
        { vault, writable: true, mode: "read-write", e2ee: { passphrase: E2EE_PASSPHRASE } },
        async ({ couch, open }) => {
            const { initializeResult } = await open();
            assert.equal(initializeResult.compatibility.status, "e2ee-invalid");
            assert.match(initializeResult.compatibility.detail, /an encrypted write would need/);
            assert.deepEqual(couch.writes, [], "the salt must never be written");
        }
    );
});

test("the same vault read-only is still serveable without a salt", async () => {
    // The writer-only precondition must not degrade a reader: a vault with a
    // passphrase but no encrypted chunks reads fine.
    const vault = writableVault();
    delete vault.localDocs["_local/obsidian_livesync_sync_parameters"];
    await withCouch({ vault, e2ee: { passphrase: E2EE_PASSPHRASE } }, async ({ open }) => {
        const { initializeResult } = await open();
        assert.equal(initializeResult.compatibility.status, "ok");
    });
});

/* -------------------------------------------------------------------------- */
/* Path obfuscation                                                            */
/* -------------------------------------------------------------------------- */

test("an obfuscated write hides the path and metadata, and reads back intact", async () => {
    await withCouch(
        encrypting({ passphrase: E2EE_PASSPHRASE, obfuscatePassphrase: E2EE_OBFUSCATE_PASSPHRASE }),
        async ({ couch, open }) => {
            const writer = await open();
            const written = await writer.sidecar.call("write", {
                path: E2EE_OBFUSCATED_PATH,
                content: { kind: "text", text: E2EE_TEXT },
                baseRev: null,
            });
            assert.equal(written.path, E2EE_OBFUSCATED_PATH);

            const ids = [...couch.docs.keys()].filter((id) => id.startsWith("f:"));
            assert.equal(ids.length, 1, "the entry should be stored under a single obfuscated id");
            const stored = couch.docs.get(ids[0]);

            // What obfuscation actually does to the stored document: the path
            // becomes an encrypted envelope and the metadata that would leak file
            // size and edit times is zeroed, with the real values inside the
            // envelope. `children` is emptied for the same reason.
            assert.ok(stored.path.startsWith("/\\:"), `unexpected stored path: ${stored.path}`);
            assert.equal(stored.path.includes("Obfuscated"), false);
            assert.equal(stored.mtime, 0);
            assert.equal(stored.ctime, 0);
            assert.equal(stored.size, 0);
            assert.deepEqual(stored.children, []);
            assert.equal(JSON.stringify([...couch.docs.values()]).includes("secret line 0"), false);

            const reader = await open();
            assert.equal(reader.initializeResult.remote.pathObfuscation, true);
            const read = await reader.sidecar.call("read", { path: E2EE_OBFUSCATED_PATH });
            assert.equal(read.text, E2EE_TEXT);
            assert.equal(read.size, written.size);
            assert.equal(read.mtimeMs, written.mtimeMs);

            // And the plaintext path is what the manifest advertises.
            const manifest = await reader.sidecar.call("manifest", { metaOnly: true });
            assert.ok(manifest.entries.some((entry) => entry.path === E2EE_OBFUSCATED_PATH));
        }
    );
});

test("guarded update and delete work on an obfuscated entry", async () => {
    // `delete` is a read-modify-write, and for an obfuscated entry that means the
    // document goes out through the decryption transform and back in through the
    // encryption one. If the round trip lost the encrypted metadata envelope, the
    // entry would become unreadable -- so this is the case most likely to break.
    await withCouch(
        encrypting({ passphrase: E2EE_PASSPHRASE, obfuscatePassphrase: E2EE_OBFUSCATE_PASSPHRASE }),
        async ({ couch, open }) => {
            const writer = await open();
            const created = await writer.sidecar.call("write", {
                path: E2EE_OBFUSCATED_PATH,
                content: { kind: "text", text: E2EE_TEXT },
                baseRev: null,
            });

            const stale = await writer.sidecar.send("write", {
                path: E2EE_OBFUSCATED_PATH,
                content: { kind: "text", text: "x" },
                baseRev: "1-00000000000000000000000000000000",
            });
            assert.equal(stale.error.data.conflict.currentRev, created.rev);
            // The conflict detail reports the REAL mtime/size, not the zeros the
            // remote physically stores for an obfuscated entry.
            assert.equal(stale.error.data.conflict.mtimeMs, created.mtimeMs);
            assert.equal(stale.error.data.conflict.size, created.size);

            const updated = await writer.sidecar.call("write", {
                path: E2EE_OBFUSCATED_PATH,
                content: { kind: "text", text: `${E2EE_TEXT}\nappended` },
                baseRev: created.rev,
            });

            const removed = await writer.sidecar.call("delete", {
                path: E2EE_OBFUSCATED_PATH,
                baseRev: updated.rev,
            });
            assert.equal(removed.deleted, true);

            // Still obfuscated on the wire, and still readable through a fresh
            // process: the envelope survived the read-modify-write.
            const id = [...couch.docs.keys()].find((candidate) => candidate.startsWith("f:"));
            const stored = couch.docs.get(id);
            assert.ok(stored.path.startsWith("/\\:"));
            assert.equal(stored.deleted, true);
            assert.equal(stored._deleted, undefined);

            const reader = await open();
            const read = await reader.sidecar.call("read", { path: E2EE_OBFUSCATED_PATH });
            assert.equal(read.deleted, true);
            assert.equal(read.text, `${E2EE_TEXT}\nappended`);
        }
    );
});

test("an obfuscated vault refuses a client without the obfuscation passphrase", async () => {
    await withCouch(
        encrypting({ passphrase: E2EE_PASSPHRASE, obfuscatePassphrase: E2EE_OBFUSCATE_PASSPHRASE }),
        async ({ open }) => {
            const writer = await open();
            await writer.sidecar.call("write", {
                path: E2EE_OBFUSCATED_PATH,
                content: { kind: "text", text: E2EE_TEXT },
                baseRev: null,
            });

            const partial = await open({ e2ee: { passphrase: E2EE_PASSPHRASE } });
            assert.equal(partial.initializeResult.compatibility.status, "e2ee-required");
            assert.match(partial.initializeResult.compatibility.detail, /obfuscated document ids/);
        }
    );
});

/* -------------------------------------------------------------------------- */
/* Committed fixture: the drift detector                                       */
/* -------------------------------------------------------------------------- */

test("the committed E2EE fixture is still readable: no codec drift", async () => {
    const { meta, vault } = loadE2EEFixture();
    // The fixture is only meaningful for the codec it was generated against, and
    // only decryptable with the exact salt and passphrase it was written with.
    assert.equal(meta.commonlibVersion, "0.1.2");
    assert.equal(meta.pbkdf2salt, PBKDF2_SALT);
    assert.equal(meta.passphrase, E2EE_PASSPHRASE);

    // Deliberately NOT writable: this asserts a pure read of bytes an earlier run
    // of the library produced.
    await withCouch({ vault, e2ee: { passphrase: E2EE_PASSPHRASE } }, async ({ couch, open }) => {
        const { sidecar, initializeResult } = await open();
        assert.equal(initializeResult.compatibility.status, "ok");
        assert.equal(initializeResult.remote.encrypted, true);
        assert.equal(initializeResult.remote.pathObfuscation, false);

        assert.equal((await sidecar.call("read", { path: E2EE_PLAIN_PATH })).text, E2EE_TEXT);
        const binary = await sidecar.call("read", { path: E2EE_BINARY_PATH });
        assert.ok(Buffer.from(binary.base64, "base64").equals(E2EE_BINARY_BYTES));

        // The plaintext chunks the fixture inherited from `smallVault` still read,
        // so a mixed encrypted/unencrypted vault is covered.
        assert.equal((await sidecar.call("read", { path: "Beta.md" })).kind, "text");

        assert.deepEqual(couch.writes, [], "reading a fixture must not write");
        assert.deepEqual(couch.unhandled, []);
    });
});

test("the committed obfuscated fixture resolves its obfuscated id back to a path", async () => {
    const { meta, vault } = loadE2EEFixture(E2EE_OBFUSCATED_FIXTURE_PATH);
    assert.equal(meta.commonlibVersion, "0.1.2");
    assert.equal(meta.obfuscatePassphrase, E2EE_OBFUSCATE_PASSPHRASE);
    assert.ok(
        Object.keys(vault.docs).some((id) => id.startsWith("f:")),
        "the obfuscated fixture has no f: id"
    );

    await withCouch(
        { vault, e2ee: { passphrase: E2EE_PASSPHRASE, obfuscatePassphrase: E2EE_OBFUSCATE_PASSPHRASE } },
        async ({ couch, open }) => {
            const { sidecar, initializeResult } = await open();
            assert.equal(initializeResult.compatibility.status, "ok");
            assert.equal(initializeResult.remote.pathObfuscation, true);

            // Two independent things: resolving the plaintext path forward to the
            // obfuscated id (`read`), and resolving the stored id back to a path
            // (`manifest`, via `id2path` on the decrypted envelope).
            assert.equal((await sidecar.call("read", { path: E2EE_OBFUSCATED_PATH })).text, E2EE_TEXT);
            const manifest = await sidecar.call("manifest", { metaOnly: true });
            assert.ok(manifest.entries.some((entry) => entry.path === E2EE_OBFUSCATED_PATH));

            assert.deepEqual(couch.writes, []);
        }
    );
});

test("the committed E2EE fixture is not readable with the wrong passphrase", async () => {
    const { vault } = loadE2EEFixture();
    await withCouch({ vault, e2ee: { passphrase: E2EE_WRONG_PASSPHRASE } }, async ({ open }) => {
        const { initializeResult } = await open();
        assert.equal(initializeResult.compatibility.status, "e2ee-invalid");
    });
});
