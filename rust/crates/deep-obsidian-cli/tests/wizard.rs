//! `setup-service --wizard` and the per-kind question sequences it shares with `mounts add`.
//!
//! # Why this file can exist at all
//!
//! The previous wizard read `io::stdin()` from six call sites and had exactly one test,
//! against the one function that took no input. Every question now goes through
//! `AnswerReader` and every credential through `SecretReader`, both of which have a
//! `from_lines` constructor, so a whole six-screen run is a list of strings. That is what
//! makes the properties below testable as PROPERTIES OF THE FLOW rather than of its pieces:
//! that a first write leaves no `.bak`, that a failed probe writes nothing, that an
//! interruption at question nine leaves no credential behind.
//!
//! # Reading an answer list
//!
//! Each entry answers one question, in order, and `""` means "press Enter" — take the
//! default. The lists below are commented question by question, because an off-by-one in an
//! answer list produces a test that passes for the wrong reason.
//!
//! # What is NOT tested here, and why
//!
//! The OS keyring: every run uses `prefer_os_keyring = false` and a temp secrets file, so a
//! test never writes into the developer's login keychain (a `SecretResolver` routes by
//! reference SHAPE, so the temp file alone would not prevent it). The masked prompt is not
//! driven either — `rpassword` needs a tty — but the code path behind it is, through
//! `SecretReader::from_lines`. The `--mcp` and `--skills` installers are always answered
//! "no": they write into `~/.codex` and `~/.claude`, which a test must not touch.

use std::path::PathBuf;

use deep_obsidian_algolia::mock::spawn_mock;
use deep_obsidian_cli::cli::{MountsAddCommon, MountsAddKind, ServiceOptions};
use deep_obsidian_cli::commands::InstallChoices;
use deep_obsidian_cli::mounts_cmd::{MountSpec, SecretReader};
use deep_obsidian_cli::wizard::{
    resolve_mount_spec, run_with_io, suggested_mount_id, AnswerReader, WizardIo, WizardRequest,
};
use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_types::SecretRef;

const API_KEY: &str = "test-wizard-api-key";
/// A port nothing listens on, so a probe fails by connection refusal rather than by timing
/// out. Deterministic and instant, which a hostname that has to fail DNS resolution is not.
const UNREACHABLE: &str = "http://127.0.0.1:1";

fn temp_dir(prefix: &str) -> PathBuf {
    static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let unique = UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "dob-wizard-{prefix}-{}-{nanos}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// One wizard run's world: a config path that does not exist yet, an index directory, and a
/// temp secret store.
struct Fixture {
    base: PathBuf,
    config_path: PathBuf,
    index_dir: PathBuf,
    resolver: SecretResolver,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let base = temp_dir(name);
        Self {
            config_path: base.join("config.json"),
            index_dir: base.join("index"),
            resolver: SecretResolver::with_encrypted_file_path(base.join("secrets.json")),
            base,
        }
    }

    fn options(&self) -> ServiceOptions {
        service_options(Some(self.config_path.clone()), Some(self.index_dir.clone()))
    }

    fn io(&self, answers: Vec<&str>, secrets: Vec<&str>) -> WizardIo {
        WizardIo {
            answers: AnswerReader::from_lines(answers.into_iter().map(str::to_string).collect()),
            secrets: SecretReader::from_lines(secrets.into_iter().map(str::to_string).collect()),
            resolver: self.resolver.clone(),
            // See the module docs: keeps every `osKeyring`-shaped reference out of the
            // developer's login keychain.
            prefer_os_keyring: false,
        }
    }

    fn backup_path(&self) -> PathBuf {
        self.config_path.with_extension("json.bak")
    }

    fn config_json(&self) -> serde_json::Value {
        let text = std::fs::read_to_string(&self.config_path).expect("read config");
        serde_json::from_str(&text).expect("config is json")
    }

    fn secret_exists(&self, id: &str) -> bool {
        self.resolver
            .get(&SecretRef::EncryptedFile { id: id.to_string() })
            .expect("read the secret store")
            .is_some()
    }
}

fn service_options(config: Option<PathBuf>, index_dir: Option<PathBuf>) -> ServiceOptions {
    ServiceOptions {
        config,
        dry_run: false,
        no_dry_run: false,
        json: false,
        no_json: false,
        vault_path: None,
        index_dir,
        packaged: false,
        insecure_no_auth: false,
        transport: None,
        stdio_mode: None,
        host: None,
        port: None,
        mcp_path: None,
        health_path: None,
        auto_reindex: false,
        no_auto_reindex: false,
        reindex_debounce_ms: None,
        reindex_interval_ms: None,
        embedding_provider: None,
        embedding_model: None,
        embedding_base_url: None,
    }
}

fn request<'a>(options: &'a ServiceOptions, dry_run: bool) -> WizardRequest<'a> {
    WizardRequest {
        options,
        dry_run,
        overwrite: false,
        installs: InstallChoices::default(),
    }
}

// ---------------------------------------------------------------------------
// The local-root happy path
// ---------------------------------------------------------------------------

/// A first install, answered entirely with defaults: a local folder the wizard creates, no
/// extra mounts, no embeddings, stdio, no extras.
///
/// The properties, all of which are properties of the WHOLE run:
///
/// * The config is written, and holds `vaultPath` rather than a one-mount table — a local
///   single-vault install must produce the same shape it always has.
/// * **No `.bak` on a first write.** A backup next to a file that did not exist would
///   overwrite nothing and mean nothing; the guard is that the wizard only backs up a
///   DIFFERING previous file.
/// * The vault folder the operator confirmed was created.
/// * Nothing landed in the secret store: no embedding key, no auth token on stdio.
#[tokio::test]
async fn a_first_local_install_writes_a_legacy_shaped_config_and_no_backup() {
    let fixture = Fixture::new("local-happy");
    let vault = fixture.base.join("Vault");
    let options = fixture.options();
    let mut io = fixture.io(
        vec![
            "1",                                 // where do your notes live -> local folder
            vault.to_str().expect("utf-8 path"), // vault folder
            "",                                  // it does not exist: create it? -> yes
            "",                                  // add another vault? -> no
            "",                                  // enable embeddings? -> no
            "",                                  // transport -> stdio
            "",                                  // install packaged skills? -> no
            "",                                  // install Obsidian snippets? -> no
            "",                                  // write it? -> yes
        ],
        Vec::new(),
    );

    let report = run_with_io(&request(&options, false), &mut io)
        .await
        .expect("the wizard completes");

    assert!(report.written, "{:?}", report.messages);
    assert!(vault.is_dir(), "the vault folder was not created");
    assert!(
        !fixture.backup_path().exists(),
        "a first write must not leave a .bak"
    );

    let json = fixture.config_json();
    assert_eq!(json["vaultPath"], vault.display().to_string());
    assert!(
        json.get("mounts").is_none(),
        "a single local vault must stay a legacy `vaultPath` config: {json}"
    );
    assert_eq!(json["transport"], "stdio");
    // stdio has no HTTP surface, so no token was generated and auth stays off.
    assert!(
        json["auth"].is_null() || json["auth"]["enabled"] == serde_json::json!(false),
        "auth must not be enabled on stdio: {json}"
    );
    assert!(
        !fixture.secret_exists("openai-embedding"),
        "no embedding key was given, so none may be stored"
    );

    let _ = std::fs::remove_dir_all(&fixture.base);
}

/// Declining the folder creation aborts without writing anything.
///
/// The wizard OFFERS to create a missing folder rather than doing it silently or refusing
/// outright, because a typo and a genuinely new vault look identical from inside the
/// command. This pins the "it was a typo" answer.
#[tokio::test]
async fn declining_to_create_the_vault_folder_writes_nothing() {
    let fixture = Fixture::new("local-decline-create");
    let vault = fixture.base.join("Typo");
    let options = fixture.options();
    let mut io = fixture.io(
        vec![
            "1",                            // local folder
            vault.to_str().expect("utf-8"), // vault folder
            "n",                            // create it? -> no
        ],
        Vec::new(),
    );

    let error = run_with_io(&request(&options, false), &mut io)
        .await
        .expect_err("a missing vault that was not created is an abort");
    let message = error.to_string();
    assert!(message.contains("does not exist"), "{message}");
    assert!(message.contains("Nothing was written"), "{message}");
    assert!(!vault.exists(), "the folder must not have been created");
    assert!(!fixture.config_path.exists(), "nothing may be written");

    let _ = std::fs::remove_dir_all(&fixture.base);
}

/// Embedding answers are PREFILLED from the existing config, so a re-run that presses Enter
/// keeps the endpoint it already had instead of replacing it with the Ollama preset.
///
/// The regression this guards is the one the whole "prefill" idea exists for: a wizard whose
/// defaults come from nowhere turns every re-run into a silent reset of the settings the
/// operator is not currently thinking about.
#[tokio::test]
async fn embedding_answers_are_prefilled_from_the_existing_config() {
    let fixture = Fixture::new("embedding-prefill");
    let vault = fixture.base.join("Vault");
    std::fs::create_dir_all(&vault).expect("vault dir");
    std::fs::write(
        &fixture.config_path,
        serde_json::json!({
            "vaultPath": vault,
            "indexDir": fixture.index_dir,
            "embedding": {
                "provider": "openai-compatible",
                "model": "already-chosen-model",
                "baseUrl": "http://elsewhere.invalid/v1"
            }
        })
        .to_string(),
    )
    .expect("seed config");

    let options = fixture.options();
    let mut io = fixture.io(
        vec![
            "1", // local folder
            "",  // vault folder -> the detected one
            "",  // add another vault? -> no
            "",  // enable embeddings? -> defaults to YES, it is configured
            "",  // which endpoint -> Ollama preset
            "",  // embedding model -> the configured one wins over the preset
            "",  // embedding base URL -> likewise
            "",  // transport -> stdio
            "",  // skills -> no
            "",  // snippets -> no
            "",  // write it -> yes
        ],
        // A blank line for the optional API key: the endpoint is local and needs none.
        vec![""],
    );

    let report = run_with_io(&request(&options, false), &mut io)
        .await
        .expect("the wizard completes");
    assert!(report.written);

    let json = fixture.config_json();
    assert_eq!(json["embedding"]["model"], "already-chosen-model");
    assert_eq!(json["embedding"]["baseUrl"], "http://elsewhere.invalid/v1");
    assert_eq!(json["embedding"]["provider"], "openai-compatible");
    // A blank key means no key, and no reference either — not a reference to nothing.
    assert!(
        json["embedding"].get("apiKeyRef").is_none(),
        "a blank key must leave no reference: {json}"
    );

    let _ = std::fs::remove_dir_all(&fixture.base);
}

/// The Ollama preset fills the model and the base URL when there is nothing to prefill from,
/// and a supplied API key is stored as a REFERENCE.
#[tokio::test]
async fn the_ollama_preset_fills_the_endpoint_and_the_key_becomes_a_reference() {
    let fixture = Fixture::new("embedding-ollama");
    let vault = fixture.base.join("Vault");
    std::fs::create_dir_all(&vault).expect("vault dir");
    let options = fixture.options();
    let mut io = fixture.io(
        vec![
            "1",                            // local folder
            vault.to_str().expect("utf-8"), // vault folder (it exists, so no create question)
            "",                             // add another vault? -> no
            "y",                            // enable embeddings
            "",                             // endpoint -> Ollama
            "",                             // model -> the preset's
            "",                             // base URL -> the preset's
            "",                             // transport -> stdio
            "",                             // skills -> no
            "",                             // snippets -> no
            "",                             // write it -> yes
        ],
        vec!["an-embedding-key"],
    );

    let report = run_with_io(&request(&options, false), &mut io)
        .await
        .expect("the wizard completes");
    assert!(report.written);

    let json = fixture.config_json();
    assert_eq!(json["embedding"]["model"], "nomic-embed-text");
    assert_eq!(json["embedding"]["baseUrl"], "http://localhost:11434/v1");
    // The config holds the reference; the value is in the store.
    assert_eq!(json["embedding"]["apiKeyRef"]["kind"], "encryptedFile");
    assert_eq!(json["embedding"]["apiKeyRef"]["id"], "openai-embedding");
    assert!(fixture.secret_exists("openai-embedding"));
    let text = std::fs::read_to_string(&fixture.config_path).expect("config");
    assert!(
        !text.contains("an-embedding-key"),
        "the credential must never reach the file: {text}"
    );

    let _ = std::fs::remove_dir_all(&fixture.base);
}

/// Choosing HTTP asks for the port, and declining authentication leaves it off.
///
/// The port is the substantive assertion: `setup-service` used to force HTTP unconditionally
/// and discard `--transport`, so a transport SCREEN only means something if the answer reaches
/// the file. The DECLINED half is what this test is for — nothing is provisioned and nothing
/// is referenced; the accepted half is
/// [`accepting_http_auth_on_a_local_root_stores_a_token_by_reference`].
#[tokio::test]
async fn choosing_http_takes_a_port_and_a_declined_auth_stays_off() {
    let fixture = Fixture::new("transport-http");
    let vault = fixture.base.join("Vault");
    std::fs::create_dir_all(&vault).expect("vault dir");
    let options = fixture.options();
    let mut io = fixture.io(
        vec![
            "1",                            // local folder
            vault.to_str().expect("utf-8"), // vault folder
            "",                             // add another vault? -> no
            "",                             // embeddings -> no
            "2",                            // transport -> HTTP
            "4321",                         // port
            "n",                            // enable bearer auth? -> no
            "n",                            // register with local agents? -> no
            "",                             // skills -> no
            "",                             // snippets -> no
            "",                             // write it -> yes
        ],
        Vec::new(),
    );

    let report = run_with_io(&request(&options, false), &mut io)
        .await
        .expect("the wizard completes");
    assert!(report.written);

    let json = fixture.config_json();
    assert_eq!(json["transport"], "http");
    assert_eq!(json["http"]["port"], 4321);
    assert!(
        json["auth"].is_null() || json["auth"]["enabled"] == serde_json::json!(false),
        "a declined auth answer must leave it off: {json}"
    );
    assert!(
        json["auth"].is_null() || json["auth"].get("tokenRef").is_none(),
        "no token may be referenced when auth was declined: {json}"
    );

    let _ = std::fs::remove_dir_all(&fixture.base);
}

/// Accepting authentication on a LOCAL root provisions a token through the injected store.
///
/// The path most users take, and until `setup_service` took an [`AuthStore`] it was the one
/// path a wizard test could not drive: a local root finishes through `setup_service`, which
/// hard-coded the real stores, so this run reached the developer's login keychain and hung on
/// a macOS authorization dialog. The mount-table test below has always been able to assert
/// this; the point here is that the ordinary shape now can too.
#[tokio::test]
async fn accepting_http_auth_on_a_local_root_stores_a_token_by_reference() {
    let fixture = Fixture::new("transport-http-auth-local");
    let vault = fixture.base.join("Vault");
    std::fs::create_dir_all(&vault).expect("vault dir");
    let options = fixture.options();
    let mut io = fixture.io(
        vec![
            "1",                            // local folder
            vault.to_str().expect("utf-8"), // vault folder
            "",                             // add another vault? -> no
            "",                             // embeddings -> no
            "2",                            // transport -> HTTP
            "4321",                         // port
            "y",                            // enable bearer auth
            "n",                            // register with local agents? -> no
            "",                             // skills -> no
            "",                             // snippets -> no
            "",                             // write it -> yes
        ],
        Vec::new(),
    );

    let report = run_with_io(&request(&options, false), &mut io)
        .await
        .expect("the wizard completes");
    assert!(report.written, "{:?}", report.messages);

    let json = fixture.config_json();
    assert_eq!(json["auth"]["enabled"], true);
    // `encryptedFile` rather than `osKeyring` is the whole assertion: the store came from the
    // wizard's injected resolver, not from the process default.
    assert_eq!(json["auth"]["tokenRef"]["kind"], "encryptedFile");
    assert_eq!(json["auth"]["tokenRef"]["id"], "http-auth-token");
    assert!(fixture.secret_exists("http-auth-token"));
    assert!(
        json["auth"].get("token").is_none(),
        "no plaintext token field may exist: {json}"
    );

    let _ = std::fs::remove_dir_all(&fixture.base);
}

/// The same thing on the mount-table path, which provisions through the wizard's own store
/// rather than through `setup_service`.
///
/// Kept as a separate test because the two paths reach `provision_auth_token` through
/// different code: a local root goes via `setup_service`, a mount table is written by the
/// wizard itself. One passing does not imply the other.
#[tokio::test]
async fn accepting_http_auth_stores_a_token_by_reference() {
    let fixture = Fixture::new("transport-http-auth");
    let (base_url, _mock) = spawn_mock().await;
    let options = fixture.options();
    let mut answers = algolia_root_answers(&base_url);
    answers.extend([
        "",     // add another vault? -> no
        "",     // embeddings -> no
        "2",    // transport -> HTTP
        "4321", // port
        "y",    // enable bearer auth
        "n",    // register with local agents? -> no
        "",     // skills -> no
        // No snippets question: the root is remote.
        "", // write it -> yes
    ]);
    let mut io = fixture.io(answers, vec![API_KEY]);

    let report = run_with_io(&request(&options, false), &mut io)
        .await
        .expect("the wizard completes");
    assert!(report.written, "{:?}", report.messages);

    let json = fixture.config_json();
    assert_eq!(json["transport"], "http");
    assert_eq!(json["http"]["port"], 4321);
    assert_eq!(json["auth"]["enabled"], true);
    assert_eq!(json["auth"]["tokenRef"]["kind"], "encryptedFile");
    assert_eq!(json["auth"]["tokenRef"]["id"], "http-auth-token");
    assert!(fixture.secret_exists("http-auth-token"));
    assert!(
        json["auth"].get("token").is_none(),
        "no plaintext token field may exist: {json}"
    );

    let _ = std::fs::remove_dir_all(&fixture.base);
}

// ---------------------------------------------------------------------------
// A remote root
// ---------------------------------------------------------------------------

/// The answers for an Algolia root at `base_url`, up to and including the experimental
/// confirmation. Split out because three tests differ only in what happens after the probe.
fn algolia_root_answers(base_url: &str) -> Vec<&str> {
    vec![
        "3",       // where do your notes live -> a shared Algolia index
        "TESTAPP", // application id
        "wiki",    // index name
        base_url,  // REST endpoint override
        "",        // may the agent write? -> no
        "y",       // enable experimental.algoliaVaults
    ]
}

/// A remote root becomes a ONE-MOUNT table with `mountAt: ""`, the experimental flag is
/// enabled only after being confirmed, and the credential lands in the store as a reference.
///
/// The shape assertion is the substantive one: a remote root cannot be expressed as a
/// top-level `vaultPath` at all, so this is the path that proves the wizard can produce a
/// mount table — the one thing `setup-service` refuses to write, and therefore the one thing
/// the wizard has to write itself.
#[tokio::test]
async fn an_algolia_root_becomes_a_one_mount_table_with_a_confirmed_flag() {
    let fixture = Fixture::new("algolia-root");
    let (base_url, _mock) = spawn_mock().await;
    let options = fixture.options();
    let mut answers = algolia_root_answers(&base_url);
    answers.extend([
        "", // add another vault? -> no
        "", // embeddings -> no
        "", // transport -> stdio
        "", // skills -> no
        // No snippets question: a remote root has no local vault folder to install into.
        "", // write it -> yes
    ]);
    let mut io = fixture.io(answers, vec![API_KEY]);

    let report = run_with_io(&request(&options, false), &mut io)
        .await
        .expect("the wizard completes");
    assert!(report.written, "{:?}", report.messages);

    let json = fixture.config_json();
    assert!(
        json["vaultPath"].is_null(),
        "a remote root has no vault path: {json}"
    );
    let mounts = json["mounts"].as_array().expect("a mount table");
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0]["id"], "vault");
    assert_eq!(mounts[0]["mountAt"], "");
    assert_eq!(mounts[0]["backend"]["kind"], "algolia");
    // `writable` defaults to false and is omitted when it is, so "not writable" is either
    // absent or explicitly false — never true.
    assert_ne!(mounts[0]["backend"]["writable"], serde_json::json!(true));
    assert_eq!(
        mounts[0]["backend"]["apiKeyRef"]["id"],
        "mount-vault-api-key"
    );
    // A single remote mount needs its own flag and NOT multiVault: one mount is the legacy
    // shape spelled out longhand.
    assert_eq!(json["experimental"]["algoliaVaults"], true);
    assert!(
        json["experimental"]["multiVault"].is_null()
            || json["experimental"]["multiVault"] == serde_json::json!(false),
        "one mount must not need multiVault: {json}"
    );
    assert!(fixture.secret_exists("mount-vault-api-key"));
    let text = std::fs::read_to_string(&fixture.config_path).expect("config");
    assert!(!text.contains(API_KEY), "the key must not reach the file");

    // The snippets install was skipped WITH a reason rather than silently dropped.
    assert!(
        report
            .messages
            .iter()
            .any(|message| message.contains("Obsidian snippets skipped")),
        "{:?}",
        report.messages
    );

    let _ = std::fs::remove_dir_all(&fixture.base);
}

/// Declining the experimental flag aborts, writes nothing, and stores no credential.
///
/// The ordering this proves: the flag is confirmed BEFORE the credential is asked for, so a
/// "no" costs the operator nothing and leaves nothing behind.
#[tokio::test]
async fn declining_the_experimental_flag_stores_no_credential() {
    let fixture = Fixture::new("algolia-decline-flag");
    let options = fixture.options();
    let mut answers = algolia_root_answers(UNREACHABLE);
    // Replace the confirmation with a refusal.
    let last = answers.len() - 1;
    answers[last] = "n";
    let mut io = fixture.io(answers, vec![API_KEY]);

    let error = run_with_io(&request(&options, false), &mut io)
        .await
        .expect_err("a declined flag aborts");
    let message = error.to_string();
    assert!(message.contains("experimental.algoliaVaults"), "{message}");
    assert!(message.contains("no secret was stored"), "{message}");
    assert!(!fixture.config_path.exists(), "nothing may be written");
    assert!(!fixture.secret_exists("mount-vault-api-key"));

    let _ = std::fs::remove_dir_all(&fixture.base);
}

/// A failed probe BLOCKS: nothing is written, and the credential the run had just stored is
/// removed rather than orphaned.
///
/// The rollback is the part worth pinning. The probe needs the credential in the store to
/// run at all, so "store, then probe, then decide" is forced — which means every abort after
/// that point owes the operator a keychain with nothing extra in it.
#[tokio::test]
async fn a_failed_probe_blocks_and_rolls_the_credential_back() {
    let fixture = Fixture::new("algolia-probe-blocks");
    let options = fixture.options();
    let mut answers = algolia_root_answers(UNREACHABLE);
    answers.push("n"); // add the mount anyway? -> no
    let mut io = fixture.io(answers, vec![API_KEY]);

    let error = run_with_io(&request(&options, false), &mut io)
        .await
        .expect_err("a failed probe aborts");
    let message = error.to_string();
    assert!(message.contains("did not pass its probe"), "{message}");
    assert!(message.contains("Nothing was written"), "{message}");
    assert!(!fixture.config_path.exists());
    assert!(
        !fixture.secret_exists("mount-vault-api-key"),
        "the credential this run stored must be removed: {message}"
    );
    assert!(
        message.contains("removed the credential stored at"),
        "the rollback must be reported, not silent: {message}"
    );

    let _ = std::fs::remove_dir_all(&fixture.base);
}

/// The same failed probe, kept anyway: the mount lands in the config and the operator is told
/// `doctor --probe-remote` will report it degraded.
#[tokio::test]
async fn a_failed_probe_can_be_kept_anyway() {
    let fixture = Fixture::new("algolia-probe-keep");
    let options = fixture.options();
    let mut answers = algolia_root_answers(UNREACHABLE);
    answers.extend([
        "y", // add the mount anyway? -> yes
        "",  // add another vault? -> no
        "",  // embeddings -> no
        "",  // transport -> stdio
        "",  // skills -> no
        "",  // write it -> yes
    ]);
    let mut io = fixture.io(answers, vec![API_KEY]);

    let report = run_with_io(&request(&options, false), &mut io)
        .await
        .expect("keeping the mount completes the wizard");
    assert!(report.written);
    assert_eq!(fixture.config_json()["mounts"][0]["id"], "vault");
    assert!(fixture.secret_exists("mount-vault-api-key"));
    assert!(
        report
            .messages
            .iter()
            .any(|message| message.contains("despite a failed probe")
                && message.contains("--probe-remote")),
        "{:?}",
        report.messages
    );

    let _ = std::fs::remove_dir_all(&fixture.base);
}

/// A CouchDB root walks the whole couchdb sequence — including the E2EE and writable
/// questions a flag would otherwise have defaulted — and `--dry-run` proves it did so
/// without storing a credential or contacting anything.
///
/// Split from the probe tests deliberately: a couchdb probe needs the Node sidecar, whose
/// presence varies by machine, so the reachability behaviour is pinned on the algolia mount
/// (where a mock makes it deterministic) and the QUESTION SEQUENCE is pinned here.
#[tokio::test]
async fn a_couchdb_root_asks_the_whole_sequence_and_dry_run_stores_nothing() {
    let fixture = Fixture::new("couchdb-root-dry");
    let options = fixture.options();
    let mut io = fixture.io(
        vec![
            "2",                     // where do your notes live -> CouchDB
            "https://couch.invalid", // server URL
            "obsidian",              // database
            "couch-user",            // user name
            "y",                     // end-to-end encrypted? -> yes
            "n",                     // may the agent write? -> no
            "y",                     // enable experimental.couchdbVaults
            "",                      // add another vault? -> no
            "",                      // embeddings -> no
            "",                      // transport -> stdio
            "",                      // skills -> no
            "",                      // write it -> yes
        ],
        // Never read: --dry-run returns before any credential is asked for.
        Vec::new(),
    );

    let report = run_with_io(&request(&options, true), &mut io)
        .await
        .expect("the dry run completes");

    assert!(!report.written, "a dry run must write nothing");
    assert!(!fixture.config_path.exists());
    assert!(!fixture.secret_exists("mount-vault-password"));
    assert!(!fixture.secret_exists("mount-vault-e2ee-passphrase"));

    // The answers reached the config the recap showed, E2EE and all.
    let mount = &report.persisted_config.mounts.as_ref().expect("a table")[0];
    let json = serde_json::to_value(mount).expect("serialize the mount");
    assert_eq!(json["backend"]["kind"], "couchdb");
    assert_eq!(json["backend"]["url"], "https://couch.invalid");
    assert_eq!(json["backend"]["database"], "obsidian");
    assert_eq!(json["backend"]["username"], "couch-user");
    // Omitted when false, so "the agent may not write" is absent-or-false, never true.
    assert_ne!(json["backend"]["writable"], serde_json::json!(true));
    assert!(
        json["backend"]["e2ee"]["passphraseRef"].is_object(),
        "the E2EE answer must produce a passphrase reference: {json}"
    );

    let _ = std::fs::remove_dir_all(&fixture.base);
}

// ---------------------------------------------------------------------------
// The additional-mounts loop
// ---------------------------------------------------------------------------

/// A second mount under a subfolder: the id is SUGGESTED from `mountAt` and editable, the
/// legacy root is migrated, and `multiVault` is confirmed rather than assumed.
#[tokio::test]
async fn the_additional_mounts_loop_suggests_an_id_and_migrates_the_root() {
    let fixture = Fixture::new("extra-mounts");
    let vault = fixture.base.join("Vault");
    let team = fixture.base.join("TeamVault");
    std::fs::create_dir_all(&vault).expect("vault dir");
    std::fs::create_dir_all(&team).expect("team dir");
    let options = fixture.options();
    let mut io = fixture.io(
        vec![
            "1",                            // local folder
            vault.to_str().expect("utf-8"), // vault folder
            "y",                            // add another vault? -> yes
            "1",                            // what kind? -> a local folder
            "Team/Alpha",                   // mount it under which folder?
            "",                             // mount id -> the suggestion, `team-alpha`
            team.to_str().expect("utf-8"),  // vault folder for that mount
            "y",                            // enable experimental.multiVault
            "",                             // add another vault? -> no
            "",                             // embeddings -> no
            "",                             // transport -> stdio
            "",                             // skills -> no
            "",                             // snippets -> no
            "",                             // write it -> yes
        ],
        Vec::new(),
    );

    let report = run_with_io(&request(&options, false), &mut io)
        .await
        .expect("the wizard completes");
    assert!(report.written, "{:?}", report.messages);

    let json = fixture.config_json();
    assert!(
        json["vaultPath"].is_null(),
        "a table and a top-level vaultPath are mutually exclusive: {json}"
    );
    let mounts = json["mounts"].as_array().expect("a mount table");
    assert_eq!(mounts.len(), 2);
    // The local root was migrated into an explicit root mount, keeping its path.
    assert_eq!(mounts[0]["id"], "vault");
    assert_eq!(mounts[0]["mountAt"], "");
    assert_eq!(
        mounts[0]["backend"]["vaultPath"],
        vault.display().to_string()
    );
    // The suggestion is a slug of the prefix, and it was accepted with a blank answer.
    assert_eq!(mounts[1]["id"], "team-alpha");
    assert_eq!(mounts[1]["mountAt"], "Team/Alpha");
    assert_eq!(json["experimental"]["multiVault"], true);
    // The two mounts must not share an index directory: the second's is derived per id, and
    // the wizard's `--index-dir` sets the TOP-LEVEL one only.
    assert_eq!(json["indexDir"], fixture.index_dir.display().to_string());
    assert!(
        mounts[1]["backend"].get("indexDir").is_none()
            || mounts[1]["backend"]["indexDir"] != json["indexDir"],
        "a non-root mount must not be given the root's index dir: {json}"
    );

    let _ = std::fs::remove_dir_all(&fixture.base);
}

/// An id typed over the suggestion wins.
#[tokio::test]
async fn a_typed_mount_id_overrides_the_suggestion() {
    let fixture = Fixture::new("extra-mounts-id");
    let vault = fixture.base.join("Vault");
    let team = fixture.base.join("TeamVault");
    std::fs::create_dir_all(&vault).expect("vault dir");
    std::fs::create_dir_all(&team).expect("team dir");
    let options = fixture.options();
    let mut io = fixture.io(
        vec![
            "1",
            vault.to_str().expect("utf-8"),
            "y",                           // add another vault
            "1",                           // a local folder
            "Team/Alpha",                  // mountAt
            "alpha",                       // id, typed over the `team-alpha` suggestion
            team.to_str().expect("utf-8"), // its vault folder
            "y",                           // multiVault
            "",                            // no more mounts
            "",                            // no embeddings
            "",                            // stdio
            "",                            // no skills
            "",                            // no snippets
            "",                            // write it
        ],
        Vec::new(),
    );

    run_with_io(&request(&options, false), &mut io)
        .await
        .expect("the wizard completes");
    assert_eq!(fixture.config_json()["mounts"][1]["id"], "alpha");

    let _ = std::fs::remove_dir_all(&fixture.base);
}

/// The slug rule, directly: a prefix becomes a valid id, and the root's empty prefix falls
/// back to a name rather than to an id the config validator would reject.
#[test]
fn mount_ids_are_suggested_as_slugs() {
    assert_eq!(suggested_mount_id("Team"), "team");
    assert_eq!(suggested_mount_id("Team/Alpha"), "team-alpha");
    assert_eq!(suggested_mount_id("_Wiki"), "wiki");
    assert_eq!(suggested_mount_id("Team  Alpha/2026"), "team-alpha-2026");
    // The config requires `[a-z0-9][a-z0-9-]*`, so neither of these may suggest itself.
    assert_eq!(suggested_mount_id(""), "vault");
    assert_eq!(suggested_mount_id("///"), "vault");
}

// ---------------------------------------------------------------------------
// Interruptions
// ---------------------------------------------------------------------------

/// An end of input at three different depths aborts cleanly and writes nothing — and the one
/// that lands AFTER a credential was stored rolls it back.
///
/// Three points rather than one, chosen for what each proves: before anything happened, in
/// the middle of a per-kind sequence, and after the probe. An EOF test only at the first
/// question would prove nothing about the rollback.
#[tokio::test]
async fn an_end_of_input_at_any_question_aborts_without_writing() {
    // 1. At the very first question.
    let fixture = Fixture::new("eof-first");
    let options = fixture.options();
    let error = run_with_io(
        &request(&options, false),
        &mut fixture.io(Vec::new(), Vec::new()),
    )
    .await
    .expect_err("no answers at all is an abort");
    assert!(
        error.to_string().contains("Where do your notes live?"),
        "{error}"
    );
    assert!(error.to_string().contains("Nothing was written"), "{error}");
    assert!(!fixture.config_path.exists());
    let _ = std::fs::remove_dir_all(&fixture.base);

    // 2. Half way through a kind's sequence, before any credential exists.
    let fixture = Fixture::new("eof-midflow");
    let options = fixture.options();
    let error = run_with_io(
        &request(&options, false),
        &mut fixture.io(vec!["3", "TESTAPP"], vec![API_KEY]),
    )
    .await
    .expect_err("running out mid-sequence is an abort");
    assert!(error.to_string().contains("Index name"), "{error}");
    assert!(!fixture.config_path.exists());
    assert!(!fixture.secret_exists("mount-vault-api-key"));
    let _ = std::fs::remove_dir_all(&fixture.base);

    // 3. At the recap — AFTER the credential was stored and the probe ran.
    let fixture = Fixture::new("eof-recap");
    let (base_url, _mock) = spawn_mock().await;
    let options = fixture.options();
    let mut answers = algolia_root_answers(&base_url);
    answers.extend([
        "", // add another vault? -> no
        "", // embeddings -> no
        "", // transport -> stdio
        "", // skills -> no
            // and then the input ends, at "Write it?"
    ]);
    let error = run_with_io(
        &request(&options, false),
        &mut fixture.io(answers, vec![API_KEY]),
    )
    .await
    .expect_err("running out at the recap is an abort");
    let message = error.to_string();
    assert!(message.contains("Write it?"), "{message}");
    assert!(!fixture.config_path.exists(), "nothing may be written");
    assert!(
        !fixture.secret_exists("mount-vault-api-key"),
        "an interruption after the credential was stored must roll it back: {message}"
    );
    let _ = std::fs::remove_dir_all(&fixture.base);
}

/// Declining the recap is the same abort: nothing written, credential rolled back.
///
/// The recap exists so the LAST thing an operator does is read the config and agree to it.
/// That is only true if disagreeing actually stops the write.
#[tokio::test]
async fn declining_the_recap_writes_nothing_and_rolls_back() {
    let fixture = Fixture::new("recap-decline");
    let (base_url, _mock) = spawn_mock().await;
    let options = fixture.options();
    let mut answers = algolia_root_answers(&base_url);
    answers.extend([
        "",  // no more mounts
        "",  // no embeddings
        "",  // stdio
        "",  // no skills
        "n", // write it? -> NO
    ]);
    let error = run_with_io(
        &request(&options, false),
        &mut fixture.io(answers, vec![API_KEY]),
    )
    .await
    .expect_err("a declined recap aborts");
    let message = error.to_string();
    assert!(message.contains("aborted at the recap"), "{message}");
    assert!(!fixture.config_path.exists());
    assert!(!fixture.secret_exists("mount-vault-api-key"), "{message}");

    let _ = std::fs::remove_dir_all(&fixture.base);
}

// ---------------------------------------------------------------------------
// The existing-mounts-config refusal
// ---------------------------------------------------------------------------

/// The wizard refuses to EDIT a config that already declares a mount table, before the first
/// question, and the refusal names the three commands that do edit one.
///
/// Not in tension with the tests above, which have the wizard CREATE tables: writing a table
/// it just assembled from answers is faithful by construction, while re-deriving an existing
/// one from a fresh set of answers would drop every per-mount setting the wizard never asks
/// about.
#[tokio::test]
async fn the_wizard_refuses_to_edit_an_existing_mounts_config() {
    let fixture = Fixture::new("refuse-mounts");
    let vault = fixture.base.join("Vault");
    std::fs::create_dir_all(&vault).expect("vault dir");
    let handwritten = serde_json::json!({
        "experimental": { "multiVault": true },
        "mounts": [
            { "id": "vault", "mountAt": "", "backend": { "kind": "filesystem", "vaultPath": vault } },
            { "id": "team", "mountAt": "Team", "backend": { "kind": "filesystem", "vaultPath": vault } }
        ]
    })
    .to_string();
    std::fs::write(&fixture.config_path, &handwritten).expect("seed config");

    let options = fixture.options();
    // Answers that WOULD have completed a run, to prove none of them was even reached.
    let error = run_with_io(
        &request(&options, false),
        &mut fixture.io(
            vec!["1", vault.to_str().expect("utf-8"), "", "", "", "", "", ""],
            Vec::new(),
        ),
    )
    .await
    .expect_err("an existing mount table is refused");
    let message = error.to_string();
    assert!(message.contains("mount table"), "{message}");
    // The refusal points at the commands that CAN do this, all three of them.
    assert!(message.contains("mounts add"), "{message}");
    assert!(message.contains("mounts list"), "{message}");
    assert!(message.contains("mounts remove"), "{message}");
    assert!(message.contains("print-config"), "{message}");
    assert!(
        !message.contains("auth"),
        "the refusal must blame the table, not auth: {message}"
    );
    assert_eq!(
        std::fs::read_to_string(&fixture.config_path).expect("config"),
        handwritten,
        "the hand-written config must be untouched, byte for byte"
    );

    let _ = std::fs::remove_dir_all(&fixture.base);
}

// ---------------------------------------------------------------------------
// `mounts add`'s guided mode
// ---------------------------------------------------------------------------

fn couchdb_pre_answers(id: Option<&str>, url: Option<&str>) -> MountsAddKind {
    MountsAddKind::Couchdb {
        common: MountsAddCommon {
            id: id.map(str::to_string),
            mount_at: None,
            keep_anyway: false,
            yes: false,
        },
        url: url.map(str::to_string),
        database: None,
        username: None,
        password_stdin: false,
        writable: false,
        e2ee: false,
        sidecar_path: None,
    }
}

/// Guided mode asks for EXACTLY the flags that were left out, and nothing else.
///
/// The two halves that matter. The supplied flags are pre-answers and are never re-asked —
/// re-asking would make `mounts add --url ...` worse than useless. And the OPTIONAL flags
/// (`--username`, `--writable`, `--e2ee`) are not asked either: they are documented flags
/// with documented defaults, so "fill in what you forgot" must not become a five-question
/// interrogation. The wizard asks those, because there were no flags for it to default from.
#[test]
fn guided_mode_prompts_only_for_the_missing_required_flags() {
    let kind = couchdb_pre_answers(Some("phone"), Some("https://couch.example"));
    // Exactly two answers, for exactly the two gaps, in the order the sequence asks them:
    // `mountAt` before `id` (so an id question could suggest a slug), then the kind's own.
    let mut answers = AnswerReader::from_lines(vec![
        "LiveSync".to_string(), // mount it under which folder?
        "obsidian".to_string(), // LiveSync database name
    ]);
    let spec = resolve_mount_spec(&kind, Some(&mut answers)).expect("the gaps are asked for");

    match spec {
        MountSpec::Couchdb {
            common,
            url,
            database,
            username,
            writable,
            e2ee,
            ..
        } => {
            assert_eq!(common.id, "phone", "a supplied --id must not be re-asked");
            assert_eq!(common.mount_at, "LiveSync");
            assert_eq!(url, "https://couch.example");
            assert_eq!(database, "obsidian");
            // The optional flags kept their documented defaults, unasked. If any of them had
            // been asked, one of the two answers above would have been consumed by it and
            // `database` would hold the wrong string.
            assert_eq!(username, None);
            assert!(!writable);
            assert!(!e2ee);
        }
        other => panic!("expected a couchdb spec, got {other:?}"),
    }
}

/// With no way to ask, every missing flag is reported TOGETHER, in one clap-shaped error.
///
/// This is the branch a script and a CI job take. The property that matters is that it is an
/// error at all: a prompt here would hang forever on a stdin nobody is typing into. Reporting
/// them together rather than one per re-run is the second half — three round trips to learn
/// three flag names is a bad command.
#[test]
fn without_a_terminal_every_missing_flag_is_named_at_once() {
    let kind = couchdb_pre_answers(None, None);
    let error = resolve_mount_spec(&kind, None).expect_err("missing flags with no way to ask");
    let message = error.to_string();
    for flag in [
        "--mount-at <MOUNT_AT>",
        "--id <ID>",
        "--url <URL>",
        "--database <DATABASE>",
    ] {
        assert!(message.contains(flag), "{flag} must be named: {message}");
    }
    // And it says what to do instead, both ways.
    assert!(message.contains("not a tty"), "{message}");
    assert!(message.contains("setup-service --wizard"), "{message}");
}

/// Nothing missing means nothing asked, on either path.
///
/// The regression guard for the guided mode's cost: a fully-flagged `mounts add` must behave
/// exactly as it did before this existed, including with no terminal at all.
#[test]
fn a_fully_flagged_add_asks_nothing_and_needs_no_terminal() {
    let kind = MountsAddKind::Filesystem {
        common: MountsAddCommon {
            id: Some("team".to_string()),
            mount_at: Some("Team".to_string()),
            keep_anyway: false,
            yes: false,
        },
        vault_path: Some(PathBuf::from("/vaults/team")),
    };
    // `None` for the reader: the non-interactive path, which must not need it.
    let spec = resolve_mount_spec(&kind, None).expect("nothing is missing");
    match spec {
        MountSpec::Filesystem { common, vault_path } => {
            assert_eq!(common.id, "team");
            assert_eq!(common.mount_at, "Team");
            assert_eq!(vault_path, PathBuf::from("/vaults/team"));
        }
        other => panic!("expected a filesystem spec, got {other:?}"),
    }
    // An empty `--mount-at ""` is the ROOT, not a missing answer.
    let root = MountsAddKind::Filesystem {
        common: MountsAddCommon {
            id: Some("vault".to_string()),
            mount_at: Some(String::new()),
            keep_anyway: false,
            yes: false,
        },
        vault_path: Some(PathBuf::from("/vaults/root")),
    };
    match resolve_mount_spec(&root, None).expect("an empty prefix is an answer") {
        MountSpec::Filesystem { common, .. } => assert_eq!(common.mount_at, ""),
        other => panic!("expected a filesystem spec, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// argv
// ---------------------------------------------------------------------------

/// `setup-service --wizard` reaches the new flow **through the real binary**, and a closed
/// stdin aborts it cleanly with nothing written.
///
/// A subprocess test rather than a library one because what can break here is the wiring —
/// clap's `--wizard` flag, `normalize_cli_args`, the dispatch in `commands::run`, and the
/// `.await` on an entry point that is now async. Every library test above calls
/// `run_with_io` directly and would keep passing with the flag unreachable.
#[test]
fn cli_argv_reaches_the_wizard_and_a_closed_stdin_writes_nothing() {
    let base = temp_dir("argv-wizard");
    let config_path = base.join("config.json");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_deep-obsidian-mcp"))
        .arg("--config")
        .arg(&config_path)
        .args(["setup-service", "--wizard"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run the binary");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        !output.status.success(),
        "an unanswerable wizard must fail\nstdout: {stdout}\nstderr: {stderr}"
    );
    // The flow was REACHED: the first screen printed before stdin ran out.
    assert!(
        stdout.contains("Where do your notes live?"),
        "the wizard's first screen must be reached: {stdout}"
    );
    assert!(stdout.contains("A local folder"), "{stdout}");
    assert!(stdout.contains("A shared Algolia index"), "{stdout}");
    // And it aborted rather than hanging or half-writing.
    assert!(stderr.contains("aborted at"), "{stderr}");
    assert!(stderr.contains("Nothing was written"), "{stderr}");
    assert!(
        !config_path.exists(),
        "an aborted wizard must leave no config"
    );
    assert!(!config_path.with_extension("json.bak").exists());

    let _ = std::fs::remove_dir_all(&base);
}

/// A whole local-root run driven through the real binary's stdin, writing a real config.
///
/// The complement to the closed-stdin test above: that one proves the flow is REACHED, this
/// one proves it completes through argv — clap, the arg normalizer, the async dispatch, the
/// `setup_service` hand-off and the printing, none of which the library tests touch.
///
/// It also pins the printing, which no library test can see: the wizard renders its own
/// report, its checks and its next steps, so `commands::run` must not render the report a
/// second time. Before that was fixed the same block appeared twice with the closing advice
/// stranded between the copies.
///
/// Answered without a credential on purpose — `rpassword` needs a tty, so a piped stdin
/// cannot supply one. Every credential path is covered at the library layer instead.
#[test]
fn cli_argv_completes_a_local_install_and_prints_its_report_once() {
    use std::io::Write;

    let base = temp_dir("argv-wizard-complete");
    let config_path = base.join("config.json");
    let vault = base.join("Vault");
    let answers = [
        "1",                            // local folder
        vault.to_str().expect("utf-8"), // vault folder
        "",                             // create it? -> yes
        "",                             // add another vault? -> no
        "",                             // embeddings -> no
        "",                             // transport -> stdio
        "",                             // skills -> no
        "",                             // snippets -> no
        "",                             // write it -> yes
    ]
    .join("\n")
        + "\n";

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_deep-obsidian-mcp"))
        .arg("--config")
        .arg(&config_path)
        // An explicit index dir: without one the packaged default would put this test's index
        // under the developer's real application-support directory.
        .arg("--index-dir")
        .arg(base.join("index"))
        .args(["setup-service", "--wizard"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the binary");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(answers.as_bytes())
        .expect("write the answers");
    let output = child.wait_with_output().expect("run the binary");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "the wizard failed\nstdout: {stdout}\nstderr: {stderr}"
    );

    // The config was written, with the shape a local single vault has always had.
    let text = std::fs::read_to_string(&config_path).expect("config written");
    let json: serde_json::Value = serde_json::from_str(&text).expect("config is json");
    assert_eq!(json["vaultPath"], vault.display().to_string());
    assert!(json.get("mounts").is_none(), "{text}");
    assert_eq!(json["transport"], "stdio");
    assert!(vault.is_dir());

    // The recap was PRINTED, through the config renderer, and before the write — the whole
    // point of the last screen is that the file is shown before it is agreed to.
    let recap = stdout
        .find("This is the configuration that will be written to")
        .expect("the recap header");
    let body = stdout
        .find("\"vaultPath\"")
        .expect("the rendered config body");
    let wrote = stdout.find("wrote config:").expect("the write message");
    assert!(
        recap < body && body < wrote,
        "the recap must be rendered before the write:\n{stdout}"
    );

    // The closing block appears exactly once, and the advice is genuinely last.
    assert_eq!(
        stdout.matches("wrote config:").count(),
        1,
        "the report was rendered more than once:\n{stdout}"
    );
    assert_eq!(stdout.matches("Next steps:").count(), 1, "{stdout}");
    let checks = stdout.find("Checks:").expect("the doctor block");
    let next = stdout.find("Next steps:").expect("the next-steps block");
    assert!(
        checks < next,
        "the checks must precede the advice:\n{stdout}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// `mounts add couchdb` with missing flags and no terminal fails naming them, **through the
/// real binary**, instead of hanging on a prompt.
///
/// The subprocess is the point: `Stdio::null()` is not a tty, which is exactly the condition
/// `resolve_mount_spec_from_flags` tests for. A library test can assert the message; only
/// this can assert that the process exits at all.
#[test]
fn cli_argv_reports_missing_mount_flags_instead_of_hanging() {
    let base = temp_dir("argv-guided");
    let vault = base.join("vault");
    std::fs::create_dir_all(&vault).expect("vault dir");
    let config_path = base.join("config.json");
    std::fs::write(
        &config_path,
        serde_json::json!({ "vaultPath": vault, "indexDir": base.join("index") }).to_string(),
    )
    .expect("seed config");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_deep-obsidian-mcp"))
        .arg("--config")
        .arg(&config_path)
        .args(["mounts", "add", "couchdb", "--url", "https://couch.example"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run the binary");

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(!output.status.success(), "missing flags must fail");
    for flag in [
        "--id <ID>",
        "--mount-at <MOUNT_AT>",
        "--database <DATABASE>",
    ] {
        assert!(stderr.contains(flag), "{flag} must be named: {stderr}");
    }
    // The one flag that WAS supplied is not reported as missing.
    assert!(!stderr.contains("--url <URL>"), "{stderr}");
    // Nothing was touched.
    assert!(!config_path.with_extension("json.bak").exists());

    let _ = std::fs::remove_dir_all(&base);
}
