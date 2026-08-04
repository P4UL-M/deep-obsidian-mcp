/**
 * The constants shared by the E2EE fixture generator and the tests that read it.
 *
 * Kept in one place because the passphrases are part of the *data*: HKDF derives
 * the content key from passphrase + salt, so changing either here without
 * regenerating the committed dump makes it unreadable.
 */
import { readFileSync } from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));

/**
 * Two fixtures, not one, because path obfuscation is not additive.
 *
 * `path2id` obfuscates unconditionally once an obfuscation passphrase is
 * configured, so a client with obfuscation on can only address entries stored
 * under `f:` ids -- and one with it off can only address plaintext ids. A vault is
 * therefore all-or-nothing in practice (the plugin requires a rebuild to switch,
 * which is what the `locked`/`cleaned` milestone states are for), and a mixed
 * fixture would be a state no real client can fully read.
 */
export const E2EE_FIXTURE_PATH = "test/fixtures/e2ee-written-vault.json";
export const E2EE_OBFUSCATED_FIXTURE_PATH = "test/fixtures/e2ee-obfuscated-vault.json";

export const E2EE_PASSPHRASE = "correct-horse-battery-staple";
export const E2EE_OBFUSCATE_PASSPHRASE = "obfuscate-the-paths-please";
export const E2EE_WRONG_PASSPHRASE = "definitely-not-the-passphrase";

export const E2EE_PLAIN_PATH = "Notes/Encrypted.md";
export const E2EE_OBFUSCATED_PATH = "Notes/Obfuscated.md";
export const E2EE_BINARY_PATH = "assets/encrypted.bin";

/** Long enough to span several chunks, so chunk assembly is exercised too. */
export const E2EE_TEXT = Array.from(
    { length: 300 },
    (_, index) => `secret line ${index}: ${"the quick brown fox jumps over the lazy dog ".repeat(2)}`
).join("\n");

export const E2EE_BINARY_BYTES = (() => {
    const bytes = Buffer.alloc(4096);
    for (let index = 0; index < bytes.length; index += 1) bytes[index] = (index * 101) % 256;
    return bytes;
})();

/** A committed dump, as a `MockCouch` fixture bundle. */
export function loadE2EEFixture(fixturePath = E2EE_FIXTURE_PATH) {
    const raw = JSON.parse(readFileSync(path.resolve(here, "..", fixturePath), "utf8"));
    return {
        meta: raw,
        vault: { docs: raw.docs, localDocs: raw.localDocs, conflicts: {} },
    };
}
