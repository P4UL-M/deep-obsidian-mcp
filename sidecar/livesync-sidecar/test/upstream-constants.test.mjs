/**
 * Pins the sidecar to the upstream constants it restates.
 *
 * `manipulator.ts` hardcodes the control-document ids, the id prefixes, and the
 * maximum schema version rather than importing them, so that a silent upstream
 * rename cannot make the sidecar follow along and quietly address the wrong
 * document. This test is the other half of that decision: it reads the values
 * out of the installed package and fails when they diverge, turning a silent
 * drift into a build failure with an obvious fix.
 *
 * It also pins the resolved commonlib version to what `SUPPORTED` advertises --
 * the Rust supervisor enforces that triple, so it must be true.
 */
import test from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import * as types from "@vrtmrz/livesync-commonlib/compat/common/types";

const require = createRequire(import.meta.url);

test("control document ids match upstream", () => {
    assert.equal(types.VERSIONING_DOCID, "obsydian_livesync_version");
    assert.equal(types.MILESTONE_DOCID, "_local/obsydian_livesync_milestone");
    assert.equal(types.DOCID_SYNC_PARAMETERS, "_local/obsidian_livesync_sync_parameters");
});

test("id prefixes match upstream", () => {
    assert.equal(types.IDPrefixes.Chunk, "h:");
    assert.equal(types.IDPrefixes.EncryptedChunk, "h:+");
    assert.equal(types.IDPrefixes.Obfuscated, "f:");
});

test("the supported schema version is upstream's VER", () => {
    // A bump here is a real event: it means the remote format changed and the
    // sidecar needs review against the new plugin release, not just a new number.
    assert.equal(types.VER, 12);
});

test("the resolved commonlib version matches the advertised pin", () => {
    const installed = require("@vrtmrz/livesync-commonlib/package.json").version;
    assert.equal(installed, "0.1.2", "the lockfile drifted from the version SUPPORTED advertises");
});

test("entry type names match upstream", () => {
    assert.equal(types.EntryTypes.NOTE_PLAIN, "plain");
    assert.equal(types.EntryTypes.NOTE_BINARY, "newnote");
    assert.equal(types.EntryTypes.NOTE_LEGACY, "notes");
    assert.equal(types.EntryTypes.CHUNK, "leaf");
    assert.equal(types.EntryTypes.VERSION_INFO, "versioninfo");
});
