//! The boundary contract, exercised against every backend.
//!
//! Each test here runs against *both* [`FilesystemVaultBackend`] and
//! [`InMemoryVaultBackend`], so anything it asserts is specified by the
//! [`VaultBackend`] trait rather than by the filesystem. A new backend added in a
//! later slice becomes conformant by being added to [`backends`].
//!
//! These tests are the boundary's own gate. The server's black-box golden suite
//! remains the gate for public MCP behaviour.

use std::path::PathBuf;

use crate::memory::InMemoryVaultBackend;
use crate::{
    BackendError, BackendKind, BackendRequest, Capability, ContentRequest, MutationRequest,
    RecallRequest, VaultBackend, VaultEntryKind,
};

use super::FilesystemVaultBackend;

/// A named backend under test, seeded with a common fixture.
struct Subject {
    name: &'static str,
    backend: Box<dyn VaultBackend>,
    /// Cleaned up on drop, when the backend owns on-disk state.
    root: Option<PathBuf>,
}

impl Drop for Subject {
    fn drop(&mut self) {
        if let Some(root) = &self.root {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

fn temp_dir(prefix: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time before epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{}-{}", std::process::id(), nanos))
}

/// The shared fixture: two notes, one nested, plus one binary artifact.
const FIXTURE: &[(&str, &[u8])] = &[
    ("Home.md", b"# Home\n\nhome body\n"),
    ("Notes/Nested.md", b"# Nested\n\nnested body\n"),
    ("Assets/blob.bin", b"\x00\x01\x02\x03"),
];

fn filesystem_subject(name: &'static str) -> Subject {
    let root = temp_dir(&format!("contract-fs-{name}"));
    for (path, bytes) in FIXTURE {
        let absolute = root.join(path);
        std::fs::create_dir_all(absolute.parent().expect("fixture parent")).expect("mkdir");
        std::fs::write(&absolute, bytes).expect("seed fixture");
    }
    Subject {
        name: "filesystem",
        backend: Box::new(FilesystemVaultBackend::new(&root)),
        root: Some(root),
    }
}

fn memory_subject() -> Subject {
    let backend = InMemoryVaultBackend::new();
    for (path, bytes) in FIXTURE {
        backend.seed(path, bytes.to_vec());
    }
    Subject {
        name: "in-memory",
        backend: Box::new(backend),
        root: None,
    }
}

/// Every backend under contract. Add new backends here.
fn backends(test_name: &'static str) -> Vec<Subject> {
    vec![filesystem_subject(test_name), memory_subject()]
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_text_returns_exact_bytes() {
    for subject in backends("read-exact") {
        let text = subject
            .backend
            .execute(BackendRequest::read_text("Home.md"))
            .await
            .unwrap_or_else(|error| panic!("[{}] read failed: {error}", subject.name))
            .into_text()
            .expect("text response");
        assert_eq!(text, "# Home\n\nhome body\n", "[{}]", subject.name);

        let nested = subject
            .backend
            .execute(BackendRequest::read_text("Notes/Nested.md"))
            .await
            .expect("nested read")
            .into_text()
            .expect("text response");
        assert_eq!(nested, "# Nested\n\nnested body\n", "[{}]", subject.name);
    }
}

#[tokio::test]
async fn read_bytes_returns_exact_bytes() {
    for subject in backends("read-bytes") {
        let bytes = subject
            .backend
            .execute(BackendRequest::read_bytes("Assets/blob.bin"))
            .await
            .expect("byte read")
            .into_bytes()
            .expect("bytes response");
        assert_eq!(bytes, vec![0, 1, 2, 3], "[{}]", subject.name);
    }
}

#[tokio::test]
async fn stat_reports_size() {
    for subject in backends("stat") {
        let size = subject
            .backend
            .execute(BackendRequest::stat("Assets/blob.bin"))
            .await
            .expect("stat")
            .into_size_bytes()
            .expect("stat response");
        assert_eq!(size, 4, "[{}]", subject.name);
    }
}

#[tokio::test]
async fn missing_file_reads_fail() {
    for subject in backends("missing") {
        let error = subject
            .backend
            .execute(BackendRequest::read_text("Nope.md"))
            .await
            .expect_err("a missing note must fail");
        // Both backends surface NotFound; the *wording* differs by design (the
        // filesystem enriches ReadText with the path), so only the kind is
        // contractual here.
        assert_eq!(
            error.io_kind(),
            Some(std::io::ErrorKind::NotFound),
            "[{}] unexpected error: {error}",
            subject.name
        );

        // ReadBytes/Stat are the bare-IO flavour on every backend.
        let error = subject
            .backend
            .execute(BackendRequest::stat("Nope.bin"))
            .await
            .expect_err("a missing artifact must fail");
        assert_eq!(
            error.io_kind(),
            Some(std::io::ErrorKind::NotFound),
            "[{}] unexpected error: {error}",
            subject.name
        );
    }
}

// ---------------------------------------------------------------------------
// Path normalization and traversal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn traversal_above_the_root_is_rejected_with_core_wording() {
    for subject in backends("traversal") {
        for path in ["../escape.md", "../../escape.md", "/../escape.md"] {
            let error = subject
                .backend
                .execute(BackendRequest::read_text(path))
                .await
                .err()
                .unwrap_or_else(|| panic!("[{}] {path} must not resolve", subject.name));
            // The rejection is reported verbatim with the caller's spelling, not the
            // normalized one — the string is public MCP behaviour.
            assert_eq!(
                error.to_string(),
                format!("invalid vault-relative path: {path}"),
                "[{}] for {path}",
                subject.name
            );
        }
    }
}

#[tokio::test]
async fn inner_parent_segments_normalize_away() {
    // `Notes/../Home.md` resolves to `Home.md`. The stricter `resources/read`
    // pre-guard that rejects it lives in the server, deliberately above this layer.
    for subject in backends("inner-dotdot") {
        let text = subject
            .backend
            .execute(BackendRequest::read_text("Notes/../Home.md"))
            .await
            .unwrap_or_else(|error| panic!("[{}] inner `..` must normalize: {error}", subject.name))
            .into_text()
            .expect("text response");
        assert_eq!(text, "# Home\n\nhome body\n", "[{}]", subject.name);
    }
}

#[tokio::test]
async fn empty_and_root_paths_are_rejected() {
    for subject in backends("empty-path") {
        for path in ["", "/"] {
            let error = subject
                .backend
                .execute(BackendRequest::read_text(path))
                .await
                .expect_err("an empty path must be rejected");
            assert_eq!(
                error.to_string(),
                format!("invalid vault-relative path: {path}"),
                "[{}] for {path:?}",
                subject.name
            );
        }
    }
}

#[tokio::test]
async fn resolve_path_accepts_valid_and_rejects_traversal() {
    for subject in backends("resolve") {
        subject
            .backend
            .execute(BackendRequest::resolve_path("Uploads/new.bin"))
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "[{}] a new in-vault path must resolve: {error}",
                    subject.name
                )
            });

        let error = subject
            .backend
            .execute(BackendRequest::resolve_path("../escape.bin"))
            .await
            .expect_err("traversal must be rejected at resolve time");
        assert_eq!(
            error.to_string(),
            "invalid vault-relative path: ../escape.bin",
            "[{}]",
            subject.name
        );
    }
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_then_read_roundtrips_and_reports_created() {
    for subject in backends("roundtrip") {
        let response = subject
            .backend
            .execute(BackendRequest::write_text(
                "Notes/New.md",
                "# New\n\nfresh\n",
            ))
            .await
            .unwrap_or_else(|error| panic!("[{}] write failed: {error}", subject.name));
        assert!(
            matches!(
                response,
                crate::BackendResponse::Mutation(crate::MutationResponse::Written {
                    created: true
                })
            ),
            "[{}] first write must report created",
            subject.name
        );

        let text = subject
            .backend
            .execute(BackendRequest::read_text("Notes/New.md"))
            .await
            .expect("read back")
            .into_text()
            .expect("text response");
        assert_eq!(text, "# New\n\nfresh\n", "[{}]", subject.name);

        // Overwriting reports `created = false`.
        let response = subject
            .backend
            .execute(BackendRequest::write_text(
                "Notes/New.md",
                "# New\n\nagain\n",
            ))
            .await
            .expect("overwrite");
        assert!(
            matches!(
                response,
                crate::BackendResponse::Mutation(crate::MutationResponse::Written {
                    created: false
                })
            ),
            "[{}] overwrite must report created = false",
            subject.name
        );
    }
}

#[tokio::test]
async fn writes_to_protected_template_folders_are_refused() {
    for subject in backends("protected") {
        for path in ["Templates/T.md", "Notes/Template/T.md", "templates/t.md"] {
            let error = subject
                .backend
                .execute(BackendRequest::write_text(path, "body"))
                .await
                .expect_err("a protected write must be refused");
            assert_eq!(
                error.to_string(),
                format!("writes to protected template folders are forbidden: {path}"),
                "[{}] for {path}",
                subject.name
            );
        }
    }
}

#[tokio::test]
async fn writes_outside_the_root_are_refused() {
    for subject in backends("write-escape") {
        let error = subject
            .backend
            .execute(BackendRequest::write_text("../escape.md", "body"))
            .await
            .expect_err("a traversing write must be refused");
        assert_eq!(
            error.to_string(),
            "invalid vault-relative path: ../escape.md",
            "[{}]",
            subject.name
        );
    }
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_children_orders_directories_before_files() {
    for subject in backends("ordering") {
        let children = subject
            .backend
            .execute(BackendRequest::list_children(None, false, false))
            .await
            .expect("list root")
            .into_children()
            .expect("children response");

        let kinds = children.iter().map(|entry| &entry.kind).collect::<Vec<_>>();
        let first_file = kinds
            .iter()
            .position(|kind| matches!(kind, VaultEntryKind::File));
        let last_dir = kinds
            .iter()
            .rposition(|kind| matches!(kind, VaultEntryKind::Directory));
        if let (Some(first_file), Some(last_dir)) = (first_file, last_dir) {
            assert!(
                last_dir < first_file,
                "[{}] directories must precede files: {:?}",
                subject.name,
                children.iter().map(|e| &e.path).collect::<Vec<_>>()
            );
        }

        // Paths are vault-relative, and each group is sorted.
        let dirs = children
            .iter()
            .filter(|entry| matches!(entry.kind, VaultEntryKind::Directory))
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        let mut sorted = dirs.clone();
        sorted.sort();
        assert_eq!(
            dirs, sorted,
            "[{}] directories must be sorted",
            subject.name
        );
        assert_eq!(
            dirs,
            vec!["Assets".to_string(), "Notes".to_string()],
            "[{}]",
            subject.name
        );

        let files = children
            .iter()
            .filter(|entry| matches!(entry.kind, VaultEntryKind::File))
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        assert_eq!(files, vec!["Home.md".to_string()], "[{}]", subject.name);
    }
}

#[tokio::test]
async fn list_children_marks_markdown_and_sizes_files() {
    for subject in backends("children-meta") {
        let children = subject
            .backend
            .execute(BackendRequest::list_children(
                Some("Notes".to_string()),
                false,
                false,
            ))
            .await
            .expect("list Notes")
            .into_children()
            .expect("children response");
        assert_eq!(children.len(), 1, "[{}]", subject.name);
        let entry = &children[0];
        assert_eq!(entry.path, "Notes/Nested.md", "[{}]", subject.name);
        assert_eq!(entry.name, "Nested.md", "[{}]", subject.name);
        assert!(entry.is_markdown, "[{}]", subject.name);
        assert_eq!(
            entry.size_bytes,
            Some(b"# Nested\n\nnested body\n".len() as u64),
            "[{}]",
            subject.name
        );
    }
}

#[tokio::test]
async fn walk_markdown_finds_every_note_sorted() {
    for subject in backends("walk") {
        let files = subject
            .backend
            .execute(BackendRequest::walk_markdown())
            .await
            .expect("walk")
            .into_markdown_files()
            .expect("markdown response");
        assert_eq!(
            files,
            vec!["Home.md".to_string(), "Notes/Nested.md".to_string()],
            "[{}] the binary artifact must not appear",
            subject.name
        );
    }
}

#[tokio::test]
async fn top_level_folders_are_sorted_and_visible_only() {
    for subject in backends("folders") {
        let folders = subject
            .backend
            .execute(BackendRequest::top_level_folders())
            .await
            .expect("folders")
            .into_folders()
            .expect("folders response");
        assert_eq!(
            folders,
            vec!["Assets".to_string(), "Notes".to_string()],
            "[{}]",
            subject.name
        );
    }
}

// ---------------------------------------------------------------------------
// Capability honesty
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grep_capability_is_honest() {
    for subject in backends("grep-honesty") {
        let descriptor = subject.backend.descriptor();
        let grep = subject
            .backend
            .execute(BackendRequest::Recall(RecallRequest::Grep {
                query: "body".to_string(),
                regex: false,
                case_sensitive: false,
                glob: None,
                context_lines: 0,
                limit: 10,
            }))
            .await;

        if descriptor.supports(Capability::GrepSearch) {
            let matches = grep
                .unwrap_or_else(|error| {
                    panic!(
                        "[{}] a backend advertising grep must serve it: {error}",
                        subject.name
                    )
                })
                .into_grep_matches()
                .expect("grep response");
            assert!(
                !matches.is_empty(),
                "[{}] the fixture contains `body`",
                subject.name
            );
            assert!(
                matches.iter().all(|item| !item.path.starts_with('/')),
                "[{}] match paths must be vault-relative",
                subject.name
            );
        } else {
            // A backend without the capability must refuse clearly, never surface a
            // raw spawn error.
            let error = grep.expect_err("a backend without grep must refuse");
            assert!(
                !error.to_string().contains("os error 2"),
                "[{}] must not leak the raw spawn error: {error}",
                subject.name
            );
        }
    }
}

/// An exhaustive grep says so, and says nothing about candidates.
///
/// The claim is what makes the field useful: a backend that CANNOT be exhaustive reports
/// `exhausted: false` with a candidate count, and the server emits the pair into the
/// `grep_search` payload only in that case. If an exhaustive backend also reported a
/// count, the payload would have to carry it always and every caller would have to
/// interpret it.
#[tokio::test]
async fn an_exhaustive_grep_reports_itself_as_exhaustive_with_no_candidate_count() {
    for subject in backends("grep-exhaustive") {
        if !subject
            .backend
            .descriptor()
            .supports(Capability::GrepSearch)
        {
            continue;
        }
        let outcome = subject
            .backend
            .execute(BackendRequest::Recall(RecallRequest::Grep {
                query: "body".to_string(),
                regex: false,
                case_sensitive: false,
                glob: None,
                context_lines: 0,
                limit: 10,
            }))
            .await
            .expect("grep")
            .into_grep_outcome()
            .expect("grep outcome");
        assert!(
            outcome.exhausted,
            "[{}] a backend that reads every file is exhaustive",
            subject.name
        );
        assert_eq!(
            outcome.candidate_count, None,
            "[{}] an exhaustive search examined no bounded candidate set",
            subject.name
        );
    }
}

/// A listing from a backend whose directories are real directories is complete, and says
/// so. `foldersTruncated` therefore never appears in a filesystem mount's payload.
#[tokio::test]
async fn a_real_directory_listing_is_never_reported_as_truncated() {
    for subject in backends("children-complete") {
        let listing = subject
            .backend
            .execute(BackendRequest::list_children(None, false, false))
            .await
            .expect("listing")
            .into_child_listing()
            .expect("child listing");
        assert!(
            !listing.folders_truncated,
            "[{}] a real directory enumerates every subfolder",
            subject.name
        );
        assert!(!listing.entries.is_empty(), "[{}]", subject.name);
    }
}

/// The three capabilities added for a versioned, index-backed corpus are absent here,
/// and every request they gate is REFUSED rather than approximated.
///
/// The approximations are all tempting and all wrong: a ranked search could fall back to
/// substring matching, a version list could report the one version that exists, and a
/// soft delete could unlink the file. Each would answer the question asked with something
/// else, and the third would hand an agent destructive filesystem access the MCP surface
/// has never granted. So the contract is that they refuse, and that the refusal is a
/// sentence rather than a code.
#[tokio::test]
async fn absent_capabilities_are_refused_rather_than_approximated() {
    for subject in backends("capability-refusals") {
        let descriptor = subject.backend.descriptor();
        for capability in [
            Capability::NativeRecall,
            Capability::VersionHistory,
            Capability::SoftDelete,
        ] {
            assert!(
                !descriptor.supports(capability),
                "[{}] {capability:?} must not be advertised by a last-writer-wins vault",
                subject.name
            );
        }

        let refusals: Vec<(&str, BackendRequest)> = vec![
            ("ranked search", BackendRequest::recall_search("body", 5)),
            ("version list", BackendRequest::note_versions("Home.md")),
            (
                "versioned read",
                BackendRequest::read_text_version("Home.md", "v1"),
            ),
            ("soft delete", BackendRequest::soft_delete("Home.md")),
        ];
        for (what, request) in refusals {
            let error = subject
                .backend
                .execute(request)
                .await
                .err()
                .unwrap_or_else(|| {
                    panic!("[{}] {what} must be refused, not answered", subject.name)
                });
            let message = error.to_string();
            // A sentence, not a code: whoever reads this has to learn WHY, and the
            // shortest honest answer to any of these is longer than a few words.
            assert!(
                message.split_whitespace().count() >= 8,
                "[{}] the {what} refusal must explain itself: {message}",
                subject.name
            );
        }

        // ...and the file the soft delete refused is STILL THERE. This is the assertion
        // that matters: a refusal that had already unlinked the note would pass every
        // check above.
        let text = subject
            .backend
            .execute(BackendRequest::read_text("Home.md"))
            .await
            .expect("the note survives a refused delete")
            .into_text()
            .expect("text");
        assert_eq!(text, "# Home\n\nhome body\n", "[{}]", subject.name);
    }
}

/// An unversioned read is byte-identical to what it was before `version` existed, and
/// mints no version token on storage that has none — which is what keeps
/// [`crate::BaseVersion`] `Unobserved` and the read-then-write window exactly as wide as
/// it always was.
#[tokio::test]
async fn an_unversioned_read_is_unchanged_and_mints_no_version() {
    for subject in backends("unversioned-read") {
        let (text, version) = subject
            .backend
            .execute(BackendRequest::read_text("Home.md"))
            .await
            .expect("read")
            .into_versioned_text()
            .expect("versioned text");
        assert_eq!(text, "# Home\n\nhome body\n", "[{}]", subject.name);
        assert_eq!(
            version, None,
            "[{}] a last-writer-wins vault mints no version token",
            subject.name
        );
    }
}

/// A write that claims to resolve a divergence behaves EXACTLY like one that does not,
/// on a backend with no divergence concept. Nothing here may start reporting a
/// reconciliation it never recorded.
#[tokio::test]
async fn resolve_divergence_is_inert_on_a_backend_that_records_none() {
    for subject in backends("resolve-inert") {
        let plain = subject
            .backend
            .execute(BackendRequest::write_text_full(
                "Notes/Merged.md",
                "# Merged\n",
                crate::BaseVersion::Unobserved,
                false,
            ))
            .await
            .expect("plain write");
        let resolving = subject
            .backend
            .execute(BackendRequest::write_text_full(
                "Notes/Merged.md",
                "# Merged again\n",
                crate::BaseVersion::Unobserved,
                true,
            ))
            .await
            .expect("resolving write");
        assert!(
            matches!(
                plain,
                crate::BackendResponse::Mutation(crate::MutationResponse::Written {
                    created: true
                })
            ),
            "[{}] {plain:?}",
            subject.name
        );
        assert!(
            matches!(
                resolving,
                crate::BackendResponse::Mutation(crate::MutationResponse::Written {
                    created: false
                })
            ),
            "[{}] a resolving write is an ordinary overwrite here: {resolving:?}",
            subject.name
        );
    }
}

#[tokio::test]
async fn descriptor_kind_matches_the_backend() {
    for subject in backends("descriptor") {
        let descriptor = subject.backend.descriptor();
        match subject.name {
            "filesystem" => {
                assert_eq!(descriptor.kind, BackendKind::Filesystem);
                assert!(descriptor.supports(Capability::Watch));
                assert!(descriptor.supports(Capability::Upload));
            }
            "in-memory" => {
                assert_eq!(descriptor.kind, BackendKind::InMemory);
                assert!(!descriptor.supports(Capability::Watch));
                assert!(!descriptor.supports(Capability::Upload));
            }
            other => panic!("unregistered backend {other}"),
        }
        // The descriptor is stable across calls.
        assert_eq!(descriptor, subject.backend.descriptor());
    }
}

#[tokio::test]
async fn health_overview_succeeds_for_a_live_vault() {
    for subject in backends("health") {
        subject
            .backend
            .execute(BackendRequest::health_overview())
            .await
            .unwrap_or_else(|error| {
                panic!("[{}] a live vault must be healthy: {error}", subject.name)
            });
    }
}

// ---------------------------------------------------------------------------
// Change stream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn change_stream_matches_the_watch_capability() {
    for subject in backends("changes") {
        let supports_watch = subject.backend.descriptor().supports(Capability::Watch);
        let mut stream = subject.backend.changes(None);
        if supports_watch {
            // Delivery itself is covered by the filesystem backend's own test; here
            // we only require that a watching backend does not hand back an
            // already-ended stream.
            let ended =
                tokio::time::timeout(std::time::Duration::from_millis(150), stream.recv()).await;
            assert!(
                !matches!(ended, Ok(None)),
                "[{}] a watching backend must not return an ended stream",
                subject.name
            );
        } else {
            assert_eq!(
                stream.recv().await,
                None,
                "[{}] a non-watching backend must return an ended stream, never pend",
                subject.name
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Unsupported operations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn uploads_are_refused_without_the_capability() {
    for subject in backends("upload-capability") {
        if subject.backend.descriptor().supports(Capability::Upload) {
            continue;
        }
        let error = subject
            .backend
            .execute(BackendRequest::Mutation(
                MutationRequest::CommitUploadStream {
                    path: "Uploads/file.bin".to_string(),
                    expected_hash: None,
                    max_bytes: 16,
                    chunks: crate::UploadChunks::new(std::iter::once(Ok(b"data".to_vec()))),
                },
            ))
            .await
            .expect_err("a backend without upload support must refuse");
        assert!(
            matches!(error, BackendError::Unsupported(_)),
            "[{}] unexpected error: {error}",
            subject.name
        );
    }
}

#[tokio::test]
async fn sweeping_staging_files_never_fails() {
    for subject in backends("sweep") {
        subject
            .backend
            .execute(BackendRequest::sweep_orphan_staging_files())
            .await
            .unwrap_or_else(|error| panic!("[{}] the sweep must not fail: {error}", subject.name));
    }
}

// ---------------------------------------------------------------------------
// Request/response family pairing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn responses_mirror_their_request_family() {
    for subject in backends("families") {
        let manifest = subject
            .backend
            .execute(BackendRequest::walk_markdown())
            .await
            .expect("walk");
        assert!(matches!(manifest, crate::BackendResponse::Manifest(_)));

        let content = subject
            .backend
            .execute(BackendRequest::read_text("Home.md"))
            .await
            .expect("read");
        assert!(matches!(content, crate::BackendResponse::Content(_)));

        let health = subject
            .backend
            .execute(BackendRequest::health_overview())
            .await
            .expect("health");
        assert!(matches!(health, crate::BackendResponse::Health(_)));

        // Unwrapping the wrong family is a reported bug, not a panic.
        let mismatch = subject
            .backend
            .execute(BackendRequest::Content(ContentRequest::ReadText {
                path: "Home.md".to_string(),
                version: None,
            }))
            .await
            .expect("read")
            .into_folders()
            .expect_err("unwrapping the wrong family must error");
        assert!(
            mismatch.to_string().contains("content response"),
            "[{}] unexpected message: {mismatch}",
            subject.name
        );
    }
}

#[tokio::test]
async fn walk_markdown_ignores_hidden_and_ignored_directories() {
    // Filesystem-specific setup (dotted dirs, node_modules), asserted through the
    // boundary. The in-memory backend has no such directories to hide.
    let subject = filesystem_subject("ignored-dirs");
    let root = subject.root.clone().expect("filesystem root");
    for (path, bytes) in [
        (".obsidian/Hidden.md", b"hidden" as &[u8]),
        ("node_modules/Ignored.md", b"ignored"),
        (".deep-obsidian-mcp/Cache.md", b"cache"),
    ] {
        let absolute = root.join(path);
        std::fs::create_dir_all(absolute.parent().expect("parent")).expect("mkdir");
        std::fs::write(&absolute, bytes).expect("seed");
    }

    let files = subject
        .backend
        .execute(BackendRequest::walk_markdown())
        .await
        .expect("walk")
        .into_markdown_files()
        .expect("markdown response");
    assert_eq!(
        files,
        vec!["Home.md".to_string(), "Notes/Nested.md".to_string()]
    );

    let folders = subject
        .backend
        .execute(BackendRequest::top_level_folders())
        .await
        .expect("folders")
        .into_folders()
        .expect("folders response");
    assert_eq!(folders, vec!["Assets".to_string(), "Notes".to_string()]);
}

#[tokio::test]
async fn list_children_uses_the_manifest_request_flags() {
    let subject = filesystem_subject("children-flags");
    let root = subject.root.clone().expect("filesystem root");
    std::fs::write(root.join(".hidden.md"), "hidden").expect("seed hidden");

    let visible = subject
        .backend
        .execute(BackendRequest::list_children(None, false, false))
        .await
        .expect("list")
        .into_children()
        .expect("children");
    assert!(
        !visible.iter().any(|entry| entry.name.starts_with('.')),
        "hidden entries must be omitted by default"
    );

    let with_hidden = subject
        .backend
        .execute(BackendRequest::list_children(None, true, false))
        .await
        .expect("list")
        .into_children()
        .expect("children");
    assert!(
        with_hidden.iter().any(|entry| entry.name == ".hidden.md"),
        "include_hidden must surface dotted entries"
    );
}
