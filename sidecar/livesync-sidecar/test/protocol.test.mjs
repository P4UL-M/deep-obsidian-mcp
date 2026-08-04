/**
 * Protocol-level behaviour: the handshake, the fail-closed gate, framing, and
 * the shutdown contract. These are the guarantees the Rust supervisor relies on
 * before it ever asks for data.
 */
import test from "node:test";
import assert from "node:assert/strict";
import { Sidecar, withSidecar, PROTOCOL_VERSION } from "./harness.mjs";
import { MockCouch } from "./mock-couch.mjs";
import { smallVault } from "./fixtures.mjs";

test("initialize reports the pinning triple and an ok remote", async () => {
    await withSidecar({ vault: smallVault({ milestone: {} }) }, async ({ initializeResult }) => {
        assert.equal(initializeResult.protocolVersion, PROTOCOL_VERSION);
        assert.equal(initializeResult.commonlibVersion, "0.1.2");
        assert.equal(initializeResult.supportedSchemaVersion, 12);
        assert.deepEqual(initializeResult.supported, {
            protocolVersion: 1,
            commonlibVersion: "0.1.2",
            maxSchemaVersion: 12,
            pluginVersionTested: "1.0.3",
        });
        assert.deepEqual(initializeResult.compatibility, { status: "ok" });
        assert.deepEqual(initializeResult.remote, {
            schemaVersion: 12,
            encrypted: false,
            pathObfuscation: false,
        });
    });
});

test("an unsupported protocolVersion is refused and the sidecar stays alive", async () => {
    const sidecar = new Sidecar();
    try {
        const response = await sidecar.send("initialize", {
            protocolVersion: 99,
            couchdb: { url: "http://127.0.0.1:1", database: "vault", username: "u", password: "p" },
        });
        assert.equal(response.error.code, -32001);
        assert.equal(response.error.data.kind, "unsupported-protocol-version");

        // Still responsive: the supervisor must be able to read a clean answer
        // rather than discover an EOF.
        const health = await sidecar.call("health", {});
        assert.equal(health.status, "uninitialized");
        assert.equal(sidecar.child.exitCode, null);
    } finally {
        await sidecar.shutdown();
    }
});

test("a second initialize is refused", async () => {
    await withSidecar({ vault: smallVault({ milestone: {} }) }, async ({ sidecar, url }) => {
        const response = await sidecar.send("initialize", {
            protocolVersion: PROTOCOL_VERSION,
            couchdb: { url, database: "vault", username: "vaultuser", password: "pw" },
        });
        assert.equal(response.error.code, -32002);
        assert.equal(response.error.data.kind, "already-initialized");
    });
});

test("every data method fails typed before initialize", async () => {
    const sidecar = new Sidecar();
    try {
        for (const [method, params] of [
            ["manifest", { metaOnly: true }],
            ["read", { path: "Beta.md" }],
            ["stat", { path: "Beta.md" }],
            ["changesSince", {}],
            ["watch", {}],
            ["unwatch", {}],
        ]) {
            const response = await sidecar.send(method, params);
            assert.ok(response.error, `${method} should have failed`);
            assert.equal(response.error.code, -32000, `${method} code`);
            assert.equal(response.error.data.kind, "not-initialized", `${method} kind`);
        }
        // health and shutdown are always available -- the supervisor needs them
        // precisely when initialize has not happened.
        const health = await sidecar.call("health", {});
        assert.equal(health.status, "uninitialized");
    } finally {
        await sidecar.shutdown();
    }
});

test("malformed frames and unknown methods get standard JSON-RPC errors", async () => {
    const sidecar = new Sidecar();
    try {
        // Parse error: answered with id null, and the loop survives.
        sidecar.writeRaw("{not json");
        // Invalid request: valid JSON, wrong envelope.
        const invalid = await sidecar.send("ignored", undefined, {
            raw: { jsonrpc: "1.0", id: 1, method: "health" },
        });
        assert.equal(invalid.error.code, -32600);

        const unknown = await sidecar.send("noSuchMethod", {});
        assert.equal(unknown.error.code, -32601);
        assert.equal(unknown.error.data.kind, "method-not-found");

        // Blank lines are ignored rather than treated as frames.
        sidecar.writeRaw("");
        const health = await sidecar.call("health", {});
        assert.equal(health.status, "uninitialized");
    } finally {
        await sidecar.shutdown();
    }
});

test("invalid params are rejected per method", async () => {
    await withSidecar({ vault: smallVault({ milestone: {} }) }, async ({ sidecar }) => {
        const cases = [
            ["manifest", { metaOnly: false }],
            ["manifest", { limit: 0 }],
            ["manifest", { cursor: "not-base64-json" }],
            ["read", {}],
            ["stat", { path: 42 }],
        ];
        for (const [method, params] of cases) {
            const response = await sidecar.send(method, params);
            assert.ok(response.error, `${method} ${JSON.stringify(params)} should have failed`);
            assert.equal(response.error.code, -32602, `${method} ${JSON.stringify(params)}`);
        }
    });
});

test("shutdown replies then exits 0", async () => {
    const couch = new MockCouch(smallVault({ milestone: {} }));
    const url = await couch.listen();
    const sidecar = new Sidecar();
    try {
        await sidecar.call("initialize", {
            protocolVersion: PROTOCOL_VERSION,
            couchdb: { url, database: "vault", username: "u", password: "pw" },
        });
        const result = await sidecar.call("shutdown", {});
        assert.deepEqual(result, { ok: true });
        const [code] = await sidecar.exited;
        assert.equal(code, 0);
    } finally {
        await sidecar.kill();
        await couch.close();
    }
});

test("closing stdin shuts the sidecar down: it never outlives its supervisor", async () => {
    const couch = new MockCouch(smallVault({ milestone: {} }));
    const url = await couch.listen();
    const sidecar = new Sidecar();
    try {
        await sidecar.call("initialize", {
            protocolVersion: PROTOCOL_VERSION,
            couchdb: { url, database: "vault", username: "u", password: "pw" },
        });
        sidecar.child.stdin.end();
        const [code] = await sidecar.exited;
        assert.equal(code, 0);
    } finally {
        await sidecar.kill();
        await couch.close();
    }
});

test("stdout carries protocol frames only", async () => {
    // commonlib's logger defaults to console.log, i.e. stdout. Opening a
    // database emits several lines; any of them reaching fd 1 would desync the
    // stream, so the harness records unparseable stdout lines as junk.
    await withSidecar({ vault: smallVault({ milestone: {} }) }, async ({ sidecar }) => {
        await sidecar.call("manifest", { metaOnly: true });
        await sidecar.call("read", { path: "Beta.md" });
        assert.deepEqual(sidecar.junk, []);
        // And the log lines really did happen -- otherwise this test proves nothing.
        assert.match(sidecar.stderr, /\[commonlib\].*Opening Database/);
    });
});
