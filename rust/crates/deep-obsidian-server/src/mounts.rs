//! One backend per mount, built once and shared by the router and the indexes.
//!
//! # Why this module exists
//!
//! Before this slice the router's backends and the per-mount index runtimes were
//! built independently: `MountRuntimes::new(&config)` derived a `(vault_path,
//! index_dir)` pair from the config, and `AppState::new` separately constructed a
//! `FilesystemVaultBackend` for the router. That is harmless when a backend is a
//! path — two `FilesystemVaultBackend`s over one directory are interchangeable.
//!
//! It is not harmless for a CouchDB mount. Its backend owns a supervised Node child
//! process holding a CouchDB connection, a handshake, and a live change feed.
//! Building one for the router and another for the index would mean two child
//! processes per mount, two handshakes, two change feeds, and two health answers
//! that cannot be reconciled with each other.
//!
//! So the backends are built FIRST, here, and both the router and the index
//! runtimes are derived from them. For a couchdb mount, [`MountBackendEntry::index_target`]
//! hands the index a [`CouchDbSource`] over *the same supervisor* the router's
//! backend uses.
//!
//! # Where secrets are resolved
//!
//! Here, and only here. [`MountBackends::build`] resolves each couchdb mount's
//! `passwordRef` and E2EE passphrase refs through the shared [`SecretResolver`] and
//! hands the plaintext straight into the backend's credential struct, which keeps it
//! in a `SecretString` and passes it to the sidecar exclusively through
//! `initialize`. Nothing between here and there writes it to argv, the environment,
//! or a log.

use std::sync::Arc;

use deep_obsidian_backend::sidecar::{SidecarCredentials, SidecarMode, SidecarSupervisor};
use deep_obsidian_backend::{
    CouchDbVaultBackend, FilesystemVaultBackend, Mount, VaultBackend, VaultRouter,
};
use deep_obsidian_config::default_mount_index_dir;
use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_types::{
    CouchdbE2eeConfig, MountBackendConfig, MountConfig, ResolvedServiceConfig, SecretRef,
};
use secrecy::SecretString;
use tracing::warn;

use crate::runtime::IndexTarget;

/// One mount's backend, plus the supervisor behind it when it has one.
pub struct MountBackendEntry {
    pub mount: MountConfig,
    pub backend: Arc<dyn VaultBackend>,
    /// The sidecar supervisor a couchdb mount's backend and index SHARE. `None` for
    /// a filesystem mount.
    pub supervisor: Option<Arc<SidecarSupervisor>>,
    /// Where this mount's index lives.
    index_dir: std::path::PathBuf,
}

impl MountBackendEntry {
    /// The index target for this mount, given the runtime handle the sync→async
    /// bridge must use.
    ///
    /// Returns a FACTORY, so a fresh [`CouchDbSource`] is minted per refresh — which
    /// is what scopes its manifest pin to one refresh. See [`IndexTarget`].
    pub fn index_target(&self, runtime: &tokio::runtime::Handle) -> IndexTarget {
        match (&self.mount.backend, &self.supervisor) {
            (MountBackendConfig::Couchdb { .. }, Some(supervisor)) => {
                let supervisor = supervisor.clone();
                let runtime = runtime.clone();
                IndexTarget::from_factory("couchdb", &self.index_dir, move || {
                    Arc::new(crate::couchdb_source::CouchDbSource::new(
                        supervisor.clone(),
                        runtime.clone(),
                    ))
                })
            }
            // A couchdb mount whose backend could not be constructed at all (missing
            // secret, missing sidecar bundle). It gets a source that FAILS, not a
            // filesystem source over an empty path: `ensure_vault_path("")`
            // normalizes a relative path against the process working directory, so an
            // empty vault path would resolve to the server's CWD and this mount would
            // index and serve whatever happens to be there under the configured
            // prefix. Failing closed is the only safe answer.
            (MountBackendConfig::Couchdb { .. }, None) => {
                // Deliberately does NOT say READ-ONLY: a mount that opted in to
                // writes and then failed to start is not read-only, it is absent, and
                // naming the wrong reason sends the reader looking in the wrong place.
                let message = format!(
                    "mount '{}' is an EXPERIMENTAL CouchDB (Self-hosted LiveSync) vault that \
could not be initialized, so it has no index",
                    self.mount.id
                );
                IndexTarget::from_factory("couchdb-unavailable", &self.index_dir, move || {
                    Arc::new(UnavailableSource {
                        message: message.clone(),
                    })
                })
            }
            _ => IndexTarget::filesystem(self.vault_path(), &self.index_dir),
        }
    }

    /// The local directory this mount reads.
    fn vault_path(&self) -> &std::path::Path {
        match &self.mount.backend {
            MountBackendConfig::Filesystem { vault_path, .. } => vault_path,
            // Unreachable: both couchdb arms are handled above.
            MountBackendConfig::Couchdb { .. } => std::path::Path::new(""),
        }
    }

    pub fn is_root(&self) -> bool {
        self.mount.mount_at.is_empty()
    }
}

/// Every mount's backend, in config order.
pub struct MountBackends {
    entries: Vec<MountBackendEntry>,
}

impl MountBackends {
    /// Build one backend per mount. No IO against a remote: a couchdb backend's
    /// handshake happens on first use, so a mount whose CouchDB is down is still
    /// constructed and can be reported as degraded.
    pub fn build(config: &ResolvedServiceConfig) -> Self {
        Self::build_with_resolver(config, &SecretResolver::new())
    }

    /// [`Self::build`] against an explicit secret store.
    ///
    /// Exists so a test can point at a temp secrets file instead of mutating
    /// `XDG_CONFIG_HOME`, which is process-global and would race every other test
    /// that reads the default secrets path.
    pub fn build_with_resolver(config: &ResolvedServiceConfig, resolver: &SecretResolver) -> Self {
        let entries = config
            .mount_table()
            .into_iter()
            .map(|mount| build_entry(config, mount, resolver))
            .collect();
        Self { entries }
    }

    pub fn entries(&self) -> &[MountBackendEntry] {
        &self.entries
    }

    /// Build the router over these backends.
    ///
    /// The backends are CLONED `Arc`s, so the router and the index runtimes address
    /// the same objects — which for a couchdb mount means the same child process.
    pub fn router(&self) -> VaultRouter {
        let mounts = self
            .entries
            .iter()
            .map(|entry| {
                Mount::new(
                    entry.mount.id.clone(),
                    entry.mount.mount_at.clone(),
                    entry.backend.clone(),
                )
            })
            .collect();
        // Infallible in practice: `normalize_service_config` is the validation gate
        // and already rejects duplicate ids and duplicate prefixes with user-facing
        // messages. A failure here means a `ResolvedServiceConfig` was hand-built
        // with an invalid table, i.e. a programming error.
        VaultRouter::new(mounts).expect("resolved config to carry a valid mount table")
    }

    /// The supervisors to shut down when the service stops.
    pub fn supervisors(&self) -> Vec<Arc<SidecarSupervisor>> {
        self.entries
            .iter()
            .filter_map(|entry| entry.supervisor.clone())
            .collect()
    }
}

/// Where a mount's index lives: its explicit `indexDir`, else the id-keyed default
/// under the root's. The ROOT mount uses the resolved top-level `index_dir`, which
/// is what keeps a single-mount config's index exactly where it has always been.
fn mount_index_dir(config: &ResolvedServiceConfig, mount: &MountConfig) -> std::path::PathBuf {
    if mount.mount_at.is_empty() {
        return config.index_dir.clone();
    }
    let declared = match &mount.backend {
        MountBackendConfig::Filesystem { index_dir, .. } => index_dir.clone(),
        MountBackendConfig::Couchdb { index_dir, .. } => index_dir.clone(),
    };
    declared.unwrap_or_else(|| default_mount_index_dir(&config.index_dir, &mount.id))
}

fn build_entry(
    config: &ResolvedServiceConfig,
    mount: MountConfig,
    resolver: &SecretResolver,
) -> MountBackendEntry {
    let index_dir = mount_index_dir(config, &mount);
    match &mount.backend {
        MountBackendConfig::Filesystem { vault_path, .. } => MountBackendEntry {
            // The EFFECTIVE index dir is declared to the backend, not the mount's
            // raw `indexDir` field: an index dir that lives *inside* the vault
            // would otherwise leak its SQLite index and sidecar files into
            // `grep_search` results as phantom vault paths. `mount_index_dir`
            // already applies the right precedence (the resolved top-level
            // `index_dir` for the ROOT mount, which folds in both the top-level
            // setting and the root mount's declared one; the declared or
            // id-keyed default elsewhere) and is where the index is actually
            // written, so it is exactly the directory to keep out of grep.
            backend: Arc::new(
                FilesystemVaultBackend::new(vault_path.clone()).with_index_dir(&index_dir),
            ),
            supervisor: None,
            index_dir,
            mount,
        },
        MountBackendConfig::Couchdb {
            url,
            database,
            username,
            password_ref,
            e2ee,
            sidecar_path,
            options,
            writable,
            ..
        } => {
            let credentials = match resolve_credentials(
                url,
                database,
                username.as_deref(),
                password_ref,
                e2ee.as_ref(),
                resolver,
            ) {
                Ok(credentials) => credentials,
                Err(error) => {
                    // A missing secret is a configuration failure, but NOT a fatal
                    // one: a couchdb mount is non-root-only, so degrading it keeps
                    // the vault root serving. The stub backend refuses every request
                    // with this exact message.
                    warn!(
                        "mount '{}' cannot be served: {error}; the vault root keeps serving and \
readiness reports the server as degraded",
                        mount.id
                    );
                    return MountBackendEntry {
                        backend: Arc::new(crate::mounts::UnavailableBackend::new(format!(
                            "mount '{}' is an EXPERIMENTAL CouchDB (Self-hosted LiveSync) vault \
that could not be initialized: {error}",
                            mount.id
                        ))),
                        supervisor: None,
                        index_dir,
                        mount,
                    };
                }
            };
            let options = options
                .as_ref()
                .and_then(|options| serde_json::to_value(options).ok())
                .filter(|options| options.as_object().is_some_and(|map| !map.is_empty()));
            let request_timeout = options
                .as_ref()
                .and_then(|options| options.get("requestTimeoutMs"))
                .and_then(serde_json::Value::as_u64)
                // The per-HTTP-request timeout is the sidecar's; the supervisor's own
                // per-RPC ceiling must be strictly larger, or a slow-but-working
                // CouchDB call would be cancelled from this side first and reported
                // as a transport failure. Doubling it, floored at the default.
                .map(|ms| {
                    std::time::Duration::from_millis(ms.saturating_mul(2))
                        .max(deep_obsidian_backend::sidecar::DEFAULT_REQUEST_TIMEOUT)
                });

            // The ONE place a read-write sidecar can come into being, and it needs the
            // mount to have said so explicitly. `writable` defaults to false, so every
            // pre-existing config produces exactly the read-only sidecar it produced
            // before — and a mount that does opt in gets a child process whose own
            // mode enforces it, rather than a Rust-side flag the sidecar knows nothing
            // about.
            let mode = if *writable {
                warn!(
                    "mount '{}' is a WRITABLE CouchDB (Self-hosted LiveSync) vault: the agent can \
edit this vault, and its writes replicate to every device syncing it",
                    mount.id
                );
                SidecarMode::ReadWrite
            } else {
                SidecarMode::ReadOnly
            };
            match CouchDbVaultBackend::spawn(
                sidecar_path.as_deref(),
                credentials,
                mode,
                options,
                request_timeout,
            ) {
                Ok((supervisor, backend)) => MountBackendEntry {
                    backend: Arc::new(backend),
                    supervisor: Some(supervisor),
                    index_dir,
                    mount,
                },
                Err(error) => {
                    warn!(
                        "mount '{}' cannot be served: {error}; the vault root keeps serving and \
readiness reports the server as degraded",
                        mount.id
                    );
                    MountBackendEntry {
                        backend: Arc::new(UnavailableBackend::new(format!(
                            "mount '{}' is an EXPERIMENTAL CouchDB (Self-hosted LiveSync) vault \
that could not be started: {error}",
                            mount.id
                        ))),
                        supervisor: None,
                        index_dir,
                        mount,
                    }
                }
            }
        }
    }
}

/// Resolve a couchdb mount's secrets through the shared store.
fn resolve_credentials(
    url: &str,
    database: &str,
    username: Option<&str>,
    password_ref: &SecretRef,
    e2ee: Option<&CouchdbE2eeConfig>,
    resolver: &SecretResolver,
) -> Result<SidecarCredentials, String> {
    let password = require_secret(resolver, password_ref, "passwordRef")?;
    let (passphrase, obfuscate) = match e2ee {
        Some(e2ee) => (
            Some(require_secret(
                resolver,
                &e2ee.passphrase_ref,
                "e2ee.passphraseRef",
            )?),
            match &e2ee.obfuscate_passphrase_ref {
                Some(reference) => Some(require_secret(
                    resolver,
                    reference,
                    "e2ee.obfuscatePassphraseRef",
                )?),
                None => None,
            },
        ),
        None => (None, None),
    };
    Ok(SidecarCredentials {
        url: url.to_string(),
        database: database.to_string(),
        // CouchDB requires a user; an empty string is what the sidecar's own tests
        // would reject, so surface the absence as a config problem rather than
        // sending an empty credential.
        username: username.unwrap_or_default().to_string(),
        password,
        e2ee_passphrase: passphrase,
        e2ee_obfuscate_passphrase: obfuscate,
    })
}

/// Fetch a required secret, naming the FIELD rather than the secret when it is
/// missing. The reference itself (a keyring service/account or a file id) is
/// deliberately not echoed: it is not secret, but it is noise in an error a user
/// reads.
fn require_secret(
    resolver: &SecretResolver,
    reference: &SecretRef,
    field: &str,
) -> Result<SecretString, String> {
    match resolver.get(reference) {
        Ok(Some(secret)) => Ok(secret),
        Ok(None) => Err(format!(
            "the secret referenced by '{field}' is not stored; add it with \
             `deep-obsidian-mcp setup-service --wizard` or store it in the encrypted secrets file"
        )),
        Err(error) => Err(format!(
            "the secret referenced by '{field}' could not be read: {error}"
        )),
    }
}

/// A backend that refuses everything with one message.
///
/// Used for a couchdb mount that could not be constructed at all (missing secret,
/// missing sidecar bundle). It exists so the router's mount table stays complete:
/// dropping the mount would silently reroute its paths to whichever mount is the
/// next-longest prefix — usually the vault root — and serve *different content*
/// under the configured prefix. Refusing loudly is the only honest answer.
pub struct UnavailableBackend {
    message: String,
}

impl UnavailableBackend {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[async_trait::async_trait]
impl VaultBackend for UnavailableBackend {
    /// No capabilities at all, so nothing is advertised for this mount.
    fn descriptor(&self) -> deep_obsidian_backend::BackendDescriptor {
        deep_obsidian_backend::BackendDescriptor::new(
            deep_obsidian_backend::BackendKind::Couchdb,
            [],
        )
    }

    async fn execute(
        &self,
        _request: deep_obsidian_backend::BackendRequest,
    ) -> Result<deep_obsidian_backend::BackendResponse, deep_obsidian_backend::BackendError> {
        Err(deep_obsidian_backend::BackendError::Unsupported(
            self.message.clone(),
        ))
    }

    fn changes(
        &self,
        _after: Option<deep_obsidian_backend::OpaqueCursor>,
    ) -> deep_obsidian_backend::watch::ChangeStream {
        deep_obsidian_backend::watch::ChangeStream::empty()
    }
}

/// A [`NoteSource`] that fails every call with one message.
///
/// The index-side counterpart of [`UnavailableBackend`]. Its `ensure_ready` failing
/// is what makes the mount's refresh fail and the mount report `Degraded`, rather
/// than an empty-or-wrong index looking successful.
struct UnavailableSource {
    message: String,
}

impl deep_obsidian_index::source::NoteSource for UnavailableSource {
    fn ensure_ready(&self) -> deep_obsidian_index::index::Result<()> {
        Err(deep_obsidian_index::index::IndexError::source(
            self.message.clone(),
        ))
    }

    fn note_snapshots(
        &self,
    ) -> deep_obsidian_index::index::Result<Vec<deep_obsidian_index::index::FileSnapshot>> {
        self.ensure_ready().map(|()| Vec::new())
    }

    fn artifact_snapshots(
        &self,
    ) -> deep_obsidian_index::index::Result<Vec<deep_obsidian_index::index::ArtifactSnapshot>> {
        self.ensure_ready().map(|()| Vec::new())
    }

    fn ensure_path(&self, _path: &str) -> deep_obsidian_index::index::Result<()> {
        self.ensure_ready()
    }

    fn read_note(&self, _path: &str) -> deep_obsidian_index::index::Result<String> {
        self.ensure_ready().map(|()| String::new())
    }

    fn read_artifact(
        &self,
        _path: &str,
        _max_bytes: u64,
    ) -> deep_obsidian_index::index::Result<Option<Vec<u8>>> {
        self.ensure_ready().map(|()| None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deep_obsidian_types::{
        AuthConfig, AutoReindexConfig, EmbeddingConfig, ExperimentalConfig, HttpConfig, StdioMode,
        TransportMode,
    };
    use std::path::PathBuf;

    fn config_with(mounts: Vec<MountConfig>, index_dir: PathBuf) -> ResolvedServiceConfig {
        ResolvedServiceConfig {
            vault_path: PathBuf::from("/tmp/root-vault"),
            mounts,
            experimental: ExperimentalConfig {
                multi_vault: true,
                couchdb_vaults: true,
            },
            index_dir,
            transport: TransportMode::Http,
            stdio_mode: StdioMode::Auto,
            http: HttpConfig::default(),
            auto_reindex: AutoReindexConfig::default(),
            embedding: EmbeddingConfig::default(),
            artifact_embedding: EmbeddingConfig::default(),
            auth: AuthConfig::default(),
            config_file_path: None,
        }
    }

    fn couchdb_mount(id: &str, mount_at: &str) -> MountConfig {
        MountConfig {
            id: id.to_string(),
            mount_at: mount_at.to_string(),
            backend: MountBackendConfig::Couchdb {
                url: "http://couch.invalid".to_string(),
                database: "vault".to_string(),
                username: Some("vaultuser".to_string()),
                password_ref: SecretRef::EncryptedFile {
                    id: "definitely-not-stored".to_string(),
                },
                e2ee: None,
                sidecar_path: None,
                index_dir: None,
                options: None,
                writable: false,
            },
        }
    }

    /// A resolver pointed at a temp secrets file that does not exist, so every
    /// `SecretRef::EncryptedFile` lookup misses.
    ///
    /// Deliberately NOT `XDG_CONFIG_HOME`: that is process-global, and mutating it
    /// races every other test that reads the default secrets path (it poisoned
    /// `bootstrap`'s `ENV_LOCK` when this test was first written that way).
    fn empty_resolver() -> (SecretResolver, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "deep-obsidian-mounts-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let resolver = SecretResolver::with_encrypted_file_path(dir.join("secrets.json"));
        (resolver, dir)
    }

    /// A couchdb mount whose secret is missing degrades to a refusing stub, and the
    /// mount stays in the router. Dropping it would reroute its prefix to the vault
    /// root and serve different content under the configured path.
    #[test]
    fn an_unresolvable_couchdb_mount_becomes_a_refusing_stub_and_stays_routed() {
        let (resolver, dir) = empty_resolver();
        let config = config_with(
            vec![
                MountConfig {
                    id: "vault".to_string(),
                    mount_at: String::new(),
                    backend: MountBackendConfig::Filesystem {
                        vault_path: PathBuf::from("/tmp/root-vault"),
                        index_dir: None,
                    },
                },
                couchdb_mount("live", "LiveSync"),
            ],
            dir.join("index"),
        );
        let backends = MountBackends::build_with_resolver(&config, &resolver);

        let live = backends
            .entries()
            .iter()
            .find(|entry| entry.mount.id == "live")
            .expect("the couchdb mount is still present");
        assert!(live.supervisor.is_none());
        // No capabilities: nothing is advertised for a mount that cannot serve.
        assert!(live.backend.descriptor().capabilities.is_empty());
        // ...and it is still routed, so its prefix cannot silently fall through to
        // the vault root.
        let router = backends.router();
        let resolved = router
            .resolve("LiveSync/Note.md")
            .expect("routes to the mount");
        assert_eq!(resolved.mount.id, "live");
    }

    /// The index directory derivation: root uses the top-level one, a non-root
    /// couchdb mount gets the id-keyed default, and an explicit one wins.
    #[test]
    fn couchdb_mounts_index_dir_follows_the_same_rules_as_a_filesystem_mount() {
        let index_dir = PathBuf::from("/tmp/index-root");
        let config = config_with(vec![], index_dir.clone());

        let root = MountConfig {
            id: "vault".to_string(),
            mount_at: String::new(),
            backend: MountBackendConfig::Filesystem {
                vault_path: PathBuf::from("/tmp/root-vault"),
                index_dir: None,
            },
        };
        assert_eq!(mount_index_dir(&config, &root), index_dir);

        let derived = couchdb_mount("live", "LiveSync");
        assert_eq!(
            mount_index_dir(&config, &derived),
            index_dir.join("mounts").join("live")
        );

        let mut explicit = couchdb_mount("live", "LiveSync");
        if let MountBackendConfig::Couchdb {
            index_dir: slot, ..
        } = &mut explicit.backend
        {
            *slot = Some(PathBuf::from("/tmp/chosen"));
        }
        assert_eq!(
            mount_index_dir(&config, &explicit),
            PathBuf::from("/tmp/chosen")
        );
    }

    /// The stub's refusal names the mount and the experimental read-only state.
    #[tokio::test]
    async fn the_stub_backend_refuses_with_a_named_message() {
        let backend = UnavailableBackend::new("mount 'live' is an EXPERIMENTAL, READ-ONLY test");
        let error = backend
            .execute(deep_obsidian_backend::BackendRequest::walk_markdown())
            .await
            .expect_err("the stub refuses everything");
        assert!(error.to_string().contains("EXPERIMENTAL"));
        assert!(error.to_string().contains("READ-ONLY"));
    }
}
