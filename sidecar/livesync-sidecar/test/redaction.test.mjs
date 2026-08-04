/**
 * Secrets must not reach stderr, which is where the Rust supervisor's log goes.
 *
 * The values below are chosen to be unmistakable in a grep: if any appears in
 * the child's captured stderr, the redaction failed.
 */
import test from "node:test";
import assert from "node:assert/strict";
import { Sidecar, withSidecar, PROTOCOL_VERSION } from "./harness.mjs";
import { encryptedVault, smallVault } from "./fixtures.mjs";

const PASSWORD = "UNIQUE-PASSWORD-2f8a91";
const PASSPHRASE = "UNIQUE-PASSPHRASE-77c3de";
const OBFUSCATE = "UNIQUE-OBFUSCATE-b41e05";

test("the CouchDB password never appears on stderr", async () => {
    await withSidecar({ vault: smallVault({ milestone: {} }), password: PASSWORD }, async ({ sidecar }) => {
        await sidecar.call("manifest", { metaOnly: true });
        await sidecar.call("read", { path: "Notes/Alpha.md" });
        await sidecar.call("changesSince", {});
        // Force a failure path too: error messages are the classic leak.
        await sidecar.send("read", { path: "Missing.md" });
        await sidecar.call("health", {});
        assert.ok(!sidecar.stderr.includes(PASSWORD), `password leaked:\n${sidecar.stderr}`);
    });
});

test("the E2EE passphrases never appear on stderr, even on the failure path", async () => {
    await withSidecar(
        {
            vault: encryptedVault({ withSalt: false }),
            password: PASSWORD,
            e2ee: { passphrase: PASSPHRASE, obfuscatePassphrase: OBFUSCATE },
        },
        async ({ sidecar }) => {
            await sidecar.call("health", {});
            for (const secret of [PASSWORD, PASSPHRASE, OBFUSCATE]) {
                assert.ok(!sidecar.stderr.includes(secret), `${secret} leaked:\n${sidecar.stderr}`);
            }
        }
    );
});

test("a URL carrying userinfo is masked even when the password was never registered", async () => {
    const sidecar = new Sidecar();
    try {
        // Unreachable on purpose: the failure detail is what gets logged.
        await sidecar.call("initialize", {
            protocolVersion: PROTOCOL_VERSION,
            couchdb: {
                url: "http://embeddeduser:EMBEDDED-SECRET-99@127.0.0.1:1",
                database: "vault",
                username: "u",
                password: "pw",
            },
        });
        await sidecar.call("health", {});
        assert.ok(!sidecar.stderr.includes("EMBEDDED-SECRET-99"), `userinfo leaked:\n${sidecar.stderr}`);
    } finally {
        await sidecar.shutdown();
    }
});

test("error messages returned over the protocol are redacted too", async () => {
    // The host may surface `error.message` verbatim in a user-facing log.
    const sidecar = new Sidecar();
    try {
        const response = await sidecar.send("initialize", {
            protocolVersion: 99,
            couchdb: { url: "http://127.0.0.1:1", database: "vault", username: "u", password: PASSWORD },
        });
        assert.ok(!JSON.stringify(response).includes(PASSWORD));
    } finally {
        await sidecar.shutdown();
    }
});

test("with path obfuscation on, vault paths are suppressed from stderr", async () => {
    // When ids are obfuscated the plaintext paths are themselves sensitive: the
    // whole point of the mode is that the server never sees them, so the log
    // must not either.
    await withSidecar(
        {
            vault: smallVault({ milestone: {} }),
            e2ee: { passphrase: PASSPHRASE, obfuscatePassphrase: OBFUSCATE },
        },
        async ({ sidecar }) => {
            await sidecar.send("read", { path: "Notes/Alpha.md" });
            await sidecar.call("health", {});
            assert.ok(
                !sidecar.stderr.includes("Notes/Alpha.md"),
                `path leaked while obfuscation is on:\n${sidecar.stderr}`
            );
        }
    );
});

test("without obfuscation, stderr still carries useful diagnostics", async () => {
    // Redaction must not be so aggressive that the log becomes useless.
    await withSidecar({ vault: smallVault({ milestone: {} }), password: PASSWORD }, async ({ sidecar }) => {
        await sidecar.call("manifest", { metaOnly: true });
        assert.match(sidecar.stderr, /livesync-sidecar 0\.1\.0 \(protocol 1\) ready/);
        assert.match(sidecar.stderr, /\[commonlib\]/);
    });
});
