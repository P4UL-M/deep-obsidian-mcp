//! A one-shot pause at the real backend boundary, independent of the transport.
use deep_obsidian_backend::{
    BackendDescriptor, BackendError, BackendRequest, BackendResponse, ChangeStream, ContentRequest,
    MutationRequest, OpaqueCursor, VaultBackend,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

struct Pause {
    path: String,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

pub struct MutationGate {
    pub inner: Arc<dyn VaultBackend>,
    pause: Mutex<Option<Pause>>,
    reads: Mutex<HashMap<String, usize>>,
    mutations: Mutex<Vec<(String, &'static str)>>,
}

impl MutationGate {
    pub fn new(inner: Arc<dyn VaultBackend>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            pause: Mutex::new(None),
            reads: Mutex::new(HashMap::new()),
            mutations: Mutex::new(Vec::new()),
        })
    }

    pub fn arm(&self, path: &str) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, seen) = oneshot::channel();
        let (release, resume) = oneshot::channel();
        assert!(self
            .pause
            .lock()
            .unwrap()
            .replace(Pause {
                path: path.into(),
                entered,
                release: resume
            })
            .is_none());
        (seen, release)
    }

    pub fn reads(&self, path: &str) -> usize {
        *self.reads.lock().unwrap().get(path).unwrap_or(&0)
    }
    pub fn mutations(&self, path: &str) -> Vec<&'static str> {
        self.mutations
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, kind)| *kind)
            .collect()
    }
}

#[async_trait::async_trait]
impl VaultBackend for MutationGate {
    fn descriptor(&self) -> BackendDescriptor {
        self.inner.descriptor()
    }
    fn storage_id(&self) -> String {
        self.inner.storage_id()
    }
    fn mutation_key(&self, path: &str) -> Result<String, BackendError> {
        self.inner.mutation_key(path)
    }
    fn changes(&self, after: Option<OpaqueCursor>) -> ChangeStream {
        self.inner.changes(after)
    }
    fn as_couchdb(&self) -> Option<&deep_obsidian_backend::CouchDbVaultBackend> {
        self.inner.as_couchdb()
    }

    async fn execute(&self, request: BackendRequest) -> Result<BackendResponse, BackendError> {
        if let BackendRequest::Content(ContentRequest::ReadText { path, .. }) = &request {
            *self.reads.lock().unwrap().entry(path.clone()).or_default() += 1;
        }
        let mutation = match &request {
            BackendRequest::Mutation(MutationRequest::WriteText { path, .. }) => {
                Some((path, "write"))
            }
            BackendRequest::Mutation(MutationRequest::SoftDelete { path, .. }) => {
                Some((path, "delete"))
            }
            BackendRequest::Mutation(MutationRequest::Rename { from, .. }) => {
                Some((from, "rename"))
            }
            BackendRequest::Mutation(MutationRequest::CommitStagedUpload { path, .. }) => {
                Some((path, "upload"))
            }
            _ => None,
        };
        if let Some((path, kind)) = mutation {
            self.mutations.lock().unwrap().push((path.clone(), kind));
            let pause = {
                let mut slot = self.pause.lock().unwrap();
                if slot.as_ref().is_some_and(|p| p.path == *path) {
                    slot.take()
                } else {
                    None
                }
            };
            if let Some(pause) = pause {
                let _ = pause.entered.send(());
                pause.release.await.map_err(|_| {
                    BackendError::Message("test released a paused request by cancellation".into())
                })?;
            }
        }
        self.inner.execute(request).await
    }
}
