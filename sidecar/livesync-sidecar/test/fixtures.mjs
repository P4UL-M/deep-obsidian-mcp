/**
 * Fixture documents in the real LiveSync CouchDB layout.
 *
 * Every shape here mirrors something the plugin actually writes:
 *
 *   * Entry `_id` is the *lower-cased* path. Upstream's `path2id_base`
 *     lower-cases unless `handleFilenameCaseSensitive` is set, so `Notes/Alpha.md`
 *     is stored under `notes/alpha.md` while the `path` field keeps the real
 *     case. Getting this wrong is the classic way a hand-built fixture reads as
 *     "not found".
 *   * Content lives in separate `leaf` documents with `h:`-prefixed ids, listed
 *     in the entry's `children`, in order. Encrypted vaults use `h:+`.
 *   * Deletion is soft: `deleted: true` on the entry, NOT a CouchDB tombstone.
 *     A tombstone would not even appear in `_all_docs`.
 *   * `eden` is present but empty; upstream reads it for legacy inline chunks.
 *
 * Text-only for this slice. E2EE and path-obfuscation fixtures need the real
 * plugin's key derivation to be trustworthy and are deferred to 3c (see README).
 */

/** Upstream's current schema version (`VER` in shared.const.behabiour.ts). */
export const SCHEMA_VERSION = 12;

function chunk(id, data) {
    return [id, { type: "leaf", data }];
}

function entry({ path, children, size, ctime, mtime, type = "plain", deleted, rev }) {
    return {
        path,
        children,
        size,
        ctime,
        mtime,
        type,
        eden: {},
        ...(deleted ? { deleted: true } : {}),
        ...(rev ? { _rev: rev } : {}),
    };
}

export const ALPHA_TEXT = "# Alpha\n\nFirst note body.\n";
const ALPHA_CHUNK_A = "# Alpha\n\n";
const ALPHA_CHUNK_B = "First note body.\n";

export const BETA_TEXT = "Beta note, single chunk.\n";
export const DELETED_TEXT = "This note was removed.\n";
export const CONFLICTED_TEXT = "Winning revision content.\n";
/** Raw bytes behind the binary fixture, base64-chunked below. */
export const BINARY_BYTES = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x01, 0x02, 0x03]);

// Upstream's splitter cuts binary content at an arbitrary BYTE offset and
// base64-encodes each piece independently (`splitPiecesRabinKarp` calls
// `arrayBufferToBase64Single` on each `subarray`). So a fragment is always valid
// base64 on its own -- and, when its byte length is not a multiple of three, it
// carries `=` padding in the middle of the stream.
//
// 5 + 7 bytes is chosen for exactly that: both fragments are padded. A reader
// that concatenates the base64 and decodes once truncates at the first interior
// `=` and silently returns 5 of the 12 bytes.
const BINARY_CHUNK_A = BINARY_BYTES.subarray(0, 5).toString("base64");
const BINARY_CHUNK_B = BINARY_BYTES.subarray(5).toString("base64");

export const CONFLICT_REV = "2-conflictingrevision";

/** Pre-chunking format: whole content inline in `data`, no children. */
export const LEGACY_TEXT = "Legacy inline note body.\n";

/**
 * A small vault: two live notes, one soft-deleted note, one conflicted note, one
 * binary attachment, plus a hidden-file entry and a plugin-settings entry that
 * must NOT show up in the manifest.
 */
export function smallVault({ schemaVersion = SCHEMA_VERSION, milestone, syncParameters } = {}) {
    const docs = Object.fromEntries([
        // -- entries ------------------------------------------------------------
        [
            "notes/alpha.md",
            entry({
                path: "Notes/Alpha.md",
                children: ["h:alpha1", "h:alpha2"],
                size: ALPHA_TEXT.length,
                ctime: 1_700_000_000_000,
                mtime: 1_700_000_100_000,
            }),
        ],
        [
            "beta.md",
            entry({
                path: "Beta.md",
                children: ["h:beta1"],
                size: BETA_TEXT.length,
                ctime: 1_700_000_200_000,
                mtime: 1_700_000_300_000,
            }),
        ],
        [
            "removed.md",
            entry({
                path: "Removed.md",
                children: ["h:removed1"],
                size: DELETED_TEXT.length,
                ctime: 1_700_000_400_000,
                mtime: 1_700_000_500_000,
                deleted: true,
            }),
        ],
        [
            "conflicted.md",
            entry({
                path: "Conflicted.md",
                children: ["h:conflicted1"],
                size: CONFLICTED_TEXT.length,
                ctime: 1_700_000_600_000,
                mtime: 1_700_000_700_000,
                rev: "3-winningrevision",
            }),
        ],
        [
            "assets/logo.png",
            entry({
                path: "assets/logo.png",
                children: ["h:bin1", "h:bin2"],
                size: BINARY_BYTES.length,
                ctime: 1_700_000_800_000,
                mtime: 1_700_000_900_000,
                type: "newnote",
            }),
        ],
        // A hidden-file entry. `i:`-prefixed ids fall in one of the gaps between
        // the ranges upstream scans, so the manifest must not list it.
        [
            "i:.obsidian/app.json",
            entry({
                path: "i:.obsidian/app.json",
                children: ["h:hidden1"],
                size: 2,
                ctime: 1_700_001_000_000,
                mtime: 1_700_001_000_000,
                type: "newnote",
            }),
        ],
        // Plugin-sync settings, likewise excluded by the `ps:` gap.
        [
            "ps:some-device",
            entry({
                path: "ps:some-device",
                children: ["h:ps1"],
                size: 2,
                ctime: 1_700_001_100_000,
                mtime: 1_700_001_100_000,
            }),
        ],

        // A pre-chunking `notes` entry: content inline, `children` absent.
        // Upstream's metadata path rewrites its type and drops the data, so this
        // fixture guards the sidecar's inline fallback.
        [
            "legacy.md",
            {
                path: "Legacy.md",
                data: LEGACY_TEXT,
                size: LEGACY_TEXT.length,
                ctime: 1_700_001_200_000,
                mtime: 1_700_001_200_000,
                type: "notes",
                eden: {},
            },
        ],

        // -- chunks -------------------------------------------------------------
        chunk("h:alpha1", ALPHA_CHUNK_A),
        chunk("h:alpha2", ALPHA_CHUNK_B),
        chunk("h:beta1", BETA_TEXT),
        chunk("h:removed1", DELETED_TEXT),
        chunk("h:conflicted1", CONFLICTED_TEXT),
        chunk("h:bin1", BINARY_CHUNK_A),
        chunk("h:bin2", BINARY_CHUNK_B),
        chunk("h:hidden1", "{}"),
        chunk("h:ps1", "{}"),

        // -- schema document ----------------------------------------------------
        ["obsydian_livesync_version", { type: "versioninfo", version: schemaVersion }],
    ]);

    const localDocs = {};
    if (milestone !== undefined) {
        localDocs["_local/obsydian_livesync_milestone"] = {
            type: "milestoneinfo",
            created: 1_700_000_000_000,
            locked: false,
            accepted_nodes: ["node-a"],
            node_chunk_info: { "node-a": { min: 0, max: 2, current: 2 } },
            node_info: {},
            tweak_values: {},
            ...milestone,
        };
    }
    if (syncParameters !== undefined) {
        localDocs["_local/obsidian_livesync_sync_parameters"] = {
            type: "sync-parameters",
            protocolVersion: 3,
            ...syncParameters,
        };
    }

    return {
        docs,
        localDocs,
        conflicts: { "conflicted.md": [CONFLICT_REV] },
    };
}

/**
 * A vault whose chunks are `h:+`-prefixed, i.e. end-to-end encrypted.
 *
 * Two markers matter, and they do different jobs in commonlib:
 *
 *   * the `h:+` id prefix selects the document for the decryption transform
 *     (`isEncryptedChunkEntry`), and its mere presence is what the sidecar reads
 *     as "this vault is configured for E2EE";
 *   * `e_: true` plus a `%=` (HKDF) or `%` (V1) header on `data` is what makes
 *     the transform actually *attempt* a decrypt. Without `e_`, upstream
 *     classifies the chunk as UNENCRYPTED and passes it through untouched --
 *     which is why an `h:+` id alone is not enough to exercise the failure path.
 *
 * The ciphertext is still not real: producing it needs the plugin's HKDF key
 * schedule, which is 3c work. It is only well-formed enough that decryption is
 * attempted and fails, which is what `e2ee-invalid` is about.
 */
export function encryptedVault({ withSalt = false } = {}) {
    const vault = smallVault({ milestone: {} });
    const docs = { ...vault.docs };
    // Re-point one entry at an encrypted chunk so an `h:+` id exists.
    docs["beta.md"] = { ...docs["beta.md"], children: ["h:+encryptedchunk"] };
    delete docs["h:beta1"];
    docs["h:+encryptedchunk"] = {
        type: "leaf",
        e_: true,
        data: `%=${Buffer.from("not-real-ciphertext").toString("base64")}`,
    };
    const localDocs = { ...vault.localDocs };
    if (withSalt) {
        localDocs["_local/obsidian_livesync_sync_parameters"] = {
            type: "sync-parameters",
            protocolVersion: 3,
            pbkdf2salt: Buffer.alloc(32, 7).toString("base64"),
        };
    }
    return { ...vault, docs, localDocs };
}

/**
 * The replication salt every E2EE fixture and every E2EE write shares.
 *
 * It has to be a *fixed* value, not a random one: the HKDF key schedule derives
 * the content key from passphrase + salt, so a sidecar that reads back what
 * another wrote must see the same salt. `_local/obsidian_livesync_sync_parameters`
 * is where LiveSync keeps it, and the sidecar refuses to create it in either
 * mode -- so a writable E2EE fixture must ship it.
 */
export const PBKDF2_SALT = Buffer.alloc(32, 7).toString("base64");

/**
 * The standard fixture, plus everything a *writer* needs: a milestone (so the
 * compatibility gate has tweak values to check) and a replication salt (so an
 * encrypting write can derive its key without writing the sync-parameters doc).
 *
 * The existing notes are plaintext on purpose. A vault can hold both -- upstream
 * classifies a chunk by its `h:+` prefix and `e_` marker, not by a vault-wide
 * flag -- so this doubles as the "first encrypted chunk appears mid-life" case.
 */
export function writableVault(overrides = {}) {
    return smallVault({ milestone: {}, syncParameters: { pbkdf2salt: PBKDF2_SALT }, ...overrides });
}

/**
 * A vault with `count` chunked notes, to push the change feed past PouchDB's
 * internal batch size (25) and past any caller-supplied `limit`.
 *
 * Chunk documents dominate the feed of a real vault, which matters: a page of
 * changes can be entirely consumed by `leaf` documents and come back with zero
 * entries while the feed is not yet drained.
 */
export function largeVault(count = 40) {
    const base = smallVault({ milestone: {} });
    const docs = { ...base.docs };
    const paths = [];
    for (let index = 0; index < count; index += 1) {
        const name = `bulk/note-${String(index).padStart(3, "0")}.md`;
        const text = `Bulk note ${index}\n`;
        docs[`h:bulk${index}`] = { type: "leaf", data: text };
        docs[name] = {
            path: name,
            children: [`h:bulk${index}`],
            size: text.length,
            ctime: 1_800_000_000_000 + index,
            mtime: 1_800_000_000_000 + index,
            type: "plain",
            eden: {},
        };
        paths.push(name);
    }
    return { ...base, docs, bulkPaths: paths };
}
