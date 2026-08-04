/// FNV-1a (64-bit) content hash, rendered as `fnv1a64:<16 lowercase hex digits>`.
///
/// This string is public MCP behaviour: it is the `hash`/`newHash`/`previousHash`
/// field of the read and write tools, the `hash` of a committed upload, and the
/// value clients feed back as `knownHash`/`expectedHash`. It lives in core so the
/// one-shot form used by the tool layer and the incremental form used by the
/// backend's streaming upload commit are provably the same function.
pub fn content_hash(bytes: &[u8]) -> String {
    let mut state = ContentHasher::new();
    state.update(bytes);
    state.finish()
}

/// Incremental form of [`content_hash`], for callers that must hash a byte stream
/// without buffering it. `ContentHasher::new().update(bytes).finish()` is
/// byte-for-byte identical to `content_hash(bytes)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentHasher {
    state: u64,
}

impl ContentHasher {
    pub fn new() -> Self {
        Self {
            state: 0xcbf2_9ce4_8422_2325,
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.state ^= u64::from(*byte);
            self.state = self.state.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    pub fn finish(&self) -> String {
        format!("fnv1a64:{:016x}", self.state)
    }
}

impl Default for ContentHasher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_stable_and_prefixed() {
        assert_eq!(content_hash(b""), "fnv1a64:cbf29ce484222325");
        assert_eq!(
            content_hash(b"binary-payload-bytes"),
            content_hash(b"binary-payload-bytes")
        );
        assert!(content_hash(b"a") != content_hash(b"b"));
    }

    #[test]
    fn incremental_matches_one_shot() {
        let payload = b"# Home\n\nbody with some length to it\n";
        let mut hasher = ContentHasher::new();
        for chunk in payload.chunks(3) {
            hasher.update(chunk);
        }
        assert_eq!(hasher.finish(), content_hash(payload));
    }
}
