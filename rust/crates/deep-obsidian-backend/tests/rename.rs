//! Deterministic versioned-backend races at the copy/remove boundary.
use deep_obsidian_backend::*;
use std::{collections::BTreeMap, sync::Mutex};

struct Backend {
    files: Mutex<BTreeMap<String, (String, String)>>,
    change_after_copy: bool,
}
impl Backend {
    fn new(change_after_copy: bool) -> Self {
        Self {
            files: Mutex::new(BTreeMap::from([(
                "Old.md".into(),
                ("original".into(), "v1".into()),
            )])),
            change_after_copy,
        }
    }
}
#[async_trait::async_trait]
impl VaultBackend for Backend {
    fn descriptor(&self) -> BackendDescriptor {
        BackendDescriptor::new(BackendKind::Couchdb, [Capability::Rename])
    }
    fn changes(&self, _: Option<OpaqueCursor>) -> ChangeStream {
        ChangeStream::empty()
    }
    async fn execute(&self, request: BackendRequest) -> Result<BackendResponse, BackendError> {
        let mut files = self.files.lock().unwrap();
        match request {
            BackendRequest::Content(ContentRequest::ReadText { path, .. }) => {
                let (text, version) = files
                    .get(&path)
                    .cloned()
                    .ok_or_else(|| BackendError::io(std::io::ErrorKind::NotFound.into()))?;
                Ok(BackendResponse::Content(ContentResponse::Text {
                    text,
                    version: Some(version),
                }))
            }
            BackendRequest::Mutation(MutationRequest::WriteText {
                path,
                content,
                base_version,
                ..
            }) => {
                assert_eq!(base_version, BaseVersion::Absent);
                assert!(!files.contains_key(&path));
                files.insert(path, (content, "copy-v1".into()));
                if self.change_after_copy {
                    files.insert("Old.md".into(), ("concurrent source".into(), "v2".into()));
                }
                Ok(BackendResponse::Mutation(MutationResponse::Written {
                    created: true,
                }))
            }
            BackendRequest::Mutation(MutationRequest::SoftDelete { path, base_version }) => {
                let found = files.get(&path).unwrap().1.clone();
                if base_version != BaseVersion::Version(found.clone()) {
                    return Err(BackendError::VersionConflict {
                        path,
                        expected: format!("{base_version:?}"),
                        found,
                    });
                }
                files.remove(&path);
                Ok(BackendResponse::Mutation(MutationResponse::SoftDeleted {
                    version_id: "deleted".into(),
                    already_deleted: false,
                    recoverable_from: None,
                }))
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_self_rename_cannot_remove_the_source() {
    let backend = Backend::new(false);
    assert!(rename_by_write_then_remove(
        &backend,
        "Old.md",
        "Old.md",
        BaseVersion::Version("v1".into())
    )
    .await
    .is_err());
    assert_eq!(backend.files.lock().unwrap()["Old.md"].0, "original");
}

#[tokio::test]
async fn a_stale_source_revision_is_refused_before_the_copy() {
    let backend = Backend::new(false);
    assert!(rename_by_write_then_remove(
        &backend,
        "Old.md",
        "New.md",
        BaseVersion::Version("stale".into())
    )
    .await
    .is_err());
    assert!(!backend.files.lock().unwrap().contains_key("New.md"));
}

#[tokio::test]
async fn a_source_changed_after_copy_is_preserved_and_partial_progress_is_reported() {
    let backend = Backend::new(true);
    let error = rename_by_write_then_remove(
        &backend,
        "Old.md",
        "New.md",
        BaseVersion::Version("v1".into()),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("Both paths may remain"));
    let files = backend.files.lock().unwrap();
    assert_eq!(files["Old.md"].0, "concurrent source");
    assert_eq!(files["New.md"].0, "original");
}

#[tokio::test]
async fn an_identical_destination_can_finish_an_interrupted_copy() {
    let backend = Backend::new(false);
    backend
        .files
        .lock()
        .unwrap()
        .insert("New.md".into(), ("original".into(), "copy-v1".into()));
    rename_by_write_then_remove(
        &backend,
        "Old.md",
        "New.md",
        BaseVersion::Version("v1".into()),
    )
    .await
    .unwrap();
    let files = backend.files.lock().unwrap();
    assert!(!files.contains_key("Old.md"));
    assert_eq!(files["New.md"].0, "original");
}
