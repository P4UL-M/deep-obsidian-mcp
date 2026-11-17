//! Client-side selection of a native configuration or a running Docker backend.
//!
//! This profile is routing metadata only. It never stores backend credentials or
//! changes the server's configuration. Forwarding uses Docker's argument vector;
//! the fixed in-container script passes client arguments through `"$@"`.
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};

const FORWARDED: &str = "DEEP_OBSIDIAN_CLI_FORWARDED";
const CONTAINER_SCRIPT: &str = r#"set -eu
config=${DO_CONFIG_PATH:-${DO_STATE_DIR:-/var/lib/deep-obsidian-mcp}/config.json}
mounted=${DO_MOUNTED_CONFIG:-/etc/deep-obsidian/config.json}
if [ -f "$mounted" ]; then config=$mounted; fi
# docker exec does not inherit exports made by the service entrypoint. Read the
# same secret inside the container; never put its value in Docker arguments.
token_file=${DO_SECRETS_DIR:-/run/secrets}/auth_token
if [ -f "$token_file" ]; then
    if [ ! -r "$token_file" ]; then
        printf '%s\n' 'cannot read the container bearer-token secret' >&2
        exit 1
    fi
    token=$(head -n 1 "$token_file")
    if [ -n "$(printf '%s' "$token" | tr -d '[:space:]')" ]; then
        export DEEP_OBSIDIAN_AUTH_TOKEN="$token"
    fi
    unset token
fi
exec deep-obsidian-cli --config "$config" "$@"
"#;

/// The management dispatcher either continues locally with rewritten arguments,
/// or returns the status of a completed link/forwarding operation.
#[derive(Debug, PartialEq, Eq)]
pub enum Dispatch {
    Continue(Vec<String>),
    Exit(i32),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Profile {
    version: u32,
    target: Target,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    known_local_configs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum Target {
    Local { config: PathBuf },
    Docker { context: String, container: String },
}

#[derive(Debug, Parser)]
#[command(
    name = "deep-obsidian-cli link",
    about = "Select and remember a Docker backend or local configuration"
)]
struct LinkOptions {
    /// Inspect the saved link without changing it.
    #[arg(long, conflicts_with_all = ["container", "local_config"])]
    status: bool,
    /// Select a running Deep Obsidian container by name or ID.
    #[arg(long, conflicts_with = "local_config")]
    container: Option<String>,
    /// Docker context to discover or target (including SSH-backed contexts).
    #[arg(long, conflicts_with = "local_config")]
    context: Option<String>,
    /// Select an existing native service configuration.
    #[arg(long, conflicts_with_all = ["container", "context"])]
    local_config: Option<PathBuf>,
    /// Emit routing metadata as JSON; never includes backend secrets.
    #[arg(long)]
    json: bool,
}

struct Output {
    code: i32,
    stdout: Vec<u8>,
}

trait Runner {
    fn output(&self, args: &[String]) -> Result<Output>;
    fn inherit(&self, args: &[String]) -> Result<i32>;
}

struct Docker;
impl Runner for Docker {
    fn output(&self, args: &[String]) -> Result<Output> {
        let output = Command::new("docker")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .context("cannot run Docker; install its client or choose a local configuration")?;
        Ok(Output {
            code: exit_code(output.status),
            stdout: output.stdout,
        })
    }

    fn inherit(&self, args: &[String]) -> Result<i32> {
        let status = Command::new("docker")
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("cannot execute the linked Docker backend")?;
        Ok(exit_code(status))
    }
}

/// Called before management-command parsing. An explicit `--config` or `--local`
/// selects native operation; no saved link preserves the previous local defaults.
pub fn dispatch(args: &[String]) -> Result<Dispatch> {
    dispatch_with(
        args,
        &deep_obsidian_config::default_config_path().with_file_name("client.json"),
        &deep_obsidian_config::default_config_path(),
        &Docker,
        std::env::var_os(FORWARDED).is_some(),
        io::stdin().is_terminal(),
    )
}

fn dispatch_with(
    args: &[String],
    profile_path: &Path,
    default_config: &Path,
    runner: &dyn Runner,
    forwarded: bool,
    interactive: bool,
) -> Result<Dispatch> {
    let (args, local) = strip_local(args);
    if forwarded {
        return Ok(Dispatch::Continue(args));
    }
    let command = command_index(&args);
    if let Some(index) = command {
        match args[index].as_str() {
            "link" => {
                let mut link_args = vec!["deep-obsidian-cli link".to_owned()];
                // --json can be supplied before the command like other CLI globals.
                if args[..index].iter().any(|a| a == "--json") {
                    link_args.push("--json".to_owned());
                }
                link_args.extend_from_slice(&args[index + 1..]);
                let options = match LinkOptions::try_parse_from(link_args) {
                    Ok(options) => options,
                    Err(error) => {
                        let code = error.exit_code();
                        error.print()?;
                        return Ok(Dispatch::Exit(code));
                    }
                };
                link(options, profile_path, default_config, runner, interactive)?;
                return Ok(Dispatch::Exit(0));
            }
            "unlink" => {
                if args[index + 1..].iter().any(|a| a == "--help" || a == "-h") {
                    println!("Usage: deep-obsidian-cli unlink\nRemove the client link; keep server configuration and data.");
                    return Ok(Dispatch::Exit(0));
                }
                if args.len() != index + 1 {
                    bail!("usage: deep-obsidian-cli unlink");
                }
                match fs::remove_file(profile_path) {
                    Ok(()) => println!("Unlinked. Commands use local configuration again."),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        println!("No saved link. Commands use local configuration.");
                    }
                    Err(e) => return Err(e).context("cannot remove the client link"),
                }
                return Ok(Dispatch::Exit(0));
            }
            _ => {}
        }
    }
    if local || has_explicit_config(&args) || is_client_install(&args) {
        return Ok(Dispatch::Continue(args));
    }
    let Some(index) = command else {
        return Ok(Dispatch::Continue(args));
    };
    if !is_backend_command(&args[index]) {
        return Ok(Dispatch::Continue(args));
    }
    let Some(profile) = read_profile(profile_path)? else {
        return Ok(Dispatch::Continue(args));
    };
    match profile.target {
        Target::Local { config } => {
            if !config.is_file() {
                bail!("linked local configuration {} is unavailable; run `deep-obsidian-cli link` or `unlink` (no fallback was used)", config.display());
            }
            let mut rewritten = vec!["--config".to_owned(), config.to_string_lossy().into_owned()];
            rewritten.extend(args);
            Ok(Dispatch::Continue(rewritten))
        }
        Target::Docker { context, container } => {
            let candidates = docker_candidates(runner, &context)
                .context("linked Docker backend is unavailable; run `deep-obsidian-cli link` or `unlink` (no fallback was used)")?;
            if !candidates.iter().any(|c| c.matches(&container)) {
                bail!("linked container {container:?} is not a running Deep Obsidian service in context {context:?}; start it or run `deep-obsidian-cli link` (no fallback was used)");
            }
            let invocation = forward_args(
                &context,
                &container,
                &args,
                interactive && io::stdout().is_terminal(),
            );
            Ok(Dispatch::Exit(runner.inherit(&invocation)?))
        }
    }
}

fn strip_local(args: &[String]) -> (Vec<String>, bool) {
    let mut local = false;
    let mut positional = false;
    let args = args
        .iter()
        .filter_map(|arg| {
            if arg == "--" {
                positional = true;
            }
            if !positional && arg == "--local" {
                local = true;
                None
            } else {
                Some(arg.clone())
            }
        })
        .collect();
    (args, local)
}

fn has_explicit_config(args: &[String]) -> bool {
    args.iter()
        .take_while(|a| a.as_str() != "--")
        .any(|a| a == "--config" || a.starts_with("--config="))
}

fn is_client_install(args: &[String]) -> bool {
    args.iter()
        .take_while(|a| a.as_str() != "--")
        .any(|a| matches!(a.as_str(), "--mcp" | "--skills" | "--vault-snippets"))
}

fn is_backend_command(command: &str) -> bool {
    matches!(
        command,
        "users"
            | "multiuser"
            | "mounts"
            | "secrets"
            | "doctor"
            | "probe"
            | "print-config"
            | "setup-service"
            | "couchdb"
            | "algolia"
    )
}

fn command_index(args: &[String]) -> Option<usize> {
    let mut skip = false;
    for (index, arg) in args.iter().enumerate() {
        if skip {
            skip = false;
            continue;
        }
        if arg == "--" {
            return args.get(index + 1).map(|_| index + 1);
        }
        if arg.starts_with('-') {
            skip = matches!(
                arg.as_str(),
                "--config"
                    | "--vault"
                    | "--vault-path"
                    | "--index-dir"
                    | "--transport"
                    | "--host"
                    | "--port"
                    | "--mcp-path"
                    | "--health-path"
                    | "--embedding-provider"
                    | "--embedding-model"
                    | "--embedding-base-url"
                    | "--reindex-debounce-ms"
                    | "--reindex-interval-ms"
                    | "--stdio-mode"
            );
            continue;
        }
        return Some(index);
    }
    None
}

fn forward_args(context: &str, container: &str, args: &[String], tty: bool) -> Vec<String> {
    let mut invocation = docker_prefix(context);
    invocation.extend(["exec".into(), "-i".into()]);
    if tty {
        invocation.push("-t".into());
    }
    invocation.extend([
        "--env".into(),
        format!("{FORWARDED}=1"),
        container.into(),
        "sh".into(),
        "-c".into(),
        CONTAINER_SCRIPT.into(),
        "deep-obsidian-cli".into(),
    ]);
    invocation.extend_from_slice(args);
    invocation
}

fn docker_prefix(context: &str) -> Vec<String> {
    vec!["--context".into(), context.into()]
}

#[derive(Debug, Clone)]
struct Container {
    id: String,
    name: String,
    image: String,
    status: String,
}
impl Container {
    fn matches(&self, selector: &str) -> bool {
        self.name == selector
            || self.id == selector
            || (selector.len() >= 12 && self.id.starts_with(selector))
    }
}

fn docker_candidates(runner: &dyn Runner, context: &str) -> Result<Vec<Container>> {
    let mut args = docker_prefix(context);
    args.extend([
        "ps".into(),
        "--no-trunc".into(),
        "--format".into(),
        "{{json .}}".into(),
    ]);
    let output = runner.output(&args)?;
    if output.code != 0 {
        // Never echo arbitrary Docker errors: a context configuration can contain
        // credential-bearing endpoints. The context name is enough to diagnose it.
        bail!(
            "cannot list running containers in Docker context {context:?} (exit {})",
            output.code
        );
    }
    let mut containers = Vec::new();
    for line in String::from_utf8(output.stdout)?
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        let value: serde_json::Value =
            serde_json::from_str(line).context("Docker returned invalid container metadata")?;
        let field = |name| {
            value
                .get(name)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned()
        };
        let image = field("Image");
        let labels = field("Labels");
        let recognized = image
            .rsplit('/')
            .next()
            .unwrap_or("")
            .starts_with("deep-obsidian-mcp:")
            || image.rsplit('/').next().unwrap_or("") == "deep-obsidian-mcp"
            || labels.split(',').any(|label| {
                matches!(
                    label,
                    "com.docker.compose.service=deep-obsidian"
                        | "org.opencontainers.image.title=deep-obsidian-mcp"
                )
            });
        if recognized {
            let id = field("ID");
            let name = field("Names");
            if !id.is_empty() && !name.is_empty() {
                containers.push(Container {
                    id,
                    name,
                    image,
                    status: field("Status"),
                });
            }
        }
    }
    Ok(containers)
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

fn current_context(runner: &dyn Runner) -> Result<String> {
    let output = runner.output(&["context".into(), "show".into()])?;
    if output.code != 0 {
        bail!("cannot determine the current Docker context");
    }
    let context = String::from_utf8(output.stdout)?.trim().to_owned();
    if context.is_empty() {
        bail!("Docker returned an empty context name");
    }
    Ok(context)
}

fn read_profile(path: &Path) -> Result<Option<Profile>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("cannot read the saved client link"),
    };
    let profile: Profile = serde_json::from_slice(&bytes).context(
        "invalid client link; run `deep-obsidian-cli link` or `unlink` (no fallback was used)",
    )?;
    if profile.version != 1 {
        bail!(
            "unsupported client link version {}; relink with a compatible CLI",
            profile.version
        );
    }
    match &profile.target {
        Target::Docker { context, container } if context.is_empty() || container.is_empty() => {
            bail!("saved Docker link has an empty target; relink")
        }
        Target::Local { config } if !config.is_absolute() => {
            bail!("saved local link must contain an absolute configuration path; relink")
        }
        _ => {}
    }
    if profile.known_local_configs.iter().any(|p| !p.is_absolute()) {
        bail!("saved local configuration registry must contain absolute paths; relink");
    }
    Ok(Some(profile))
}

fn save_profile(path: &Path, target: Target) -> Result<()> {
    let parent = path
        .parent()
        .context("client profile needs a parent directory")?;
    fs::create_dir_all(parent)?;
    let suffix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temp = parent.join(format!(".client.{}.{suffix}.tmp", std::process::id()));
    let previous = read_profile(path).ok().flatten();
    let mut known_local_configs = previous
        .as_ref()
        .map(|p| p.known_local_configs.clone())
        .unwrap_or_default();
    if let Some(Profile {
        target: Target::Local { config },
        ..
    }) = previous
    {
        if !known_local_configs.contains(&config) {
            known_local_configs.push(config);
        }
    }
    if let Target::Local { config } = &target {
        if !known_local_configs.contains(config) {
            known_local_configs.push(config.clone());
        }
    }
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        serde_json::to_writer_pretty(
            &mut file,
            &Profile {
                version: 1,
                target,
                known_local_configs,
            },
        )?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result.context("cannot save the selected client link")
}

fn link(
    options: LinkOptions,
    path: &Path,
    default_config: &Path,
    runner: &dyn Runner,
    interactive: bool,
) -> Result<()> {
    if options.status {
        let profile = read_profile(path)?;
        if options.json {
            println!("{}", serde_json::to_string_pretty(&profile)?);
        } else {
            match profile {
                Some(Profile {
                    target: Target::Local { config },
                    ..
                }) => println!(
                    "Linked local configuration: {}{}",
                    config.display(),
                    if config.is_file() {
                        ""
                    } else {
                        " (unavailable)"
                    }
                ),
                Some(Profile {
                    target: Target::Docker { context, container },
                    ..
                }) => {
                    let available = docker_candidates(runner, &context)
                        .map(|c| c.iter().any(|c| c.matches(&container)))
                        .unwrap_or(false);
                    println!(
                        "Linked Docker backend: {container} (context {context}, {})",
                        if available { "running" } else { "unavailable" }
                    );
                }
                None => println!("No saved link. Commands use the local configuration."),
            }
        }
        return Ok(());
    }
    if let Some(config) = options.local_config {
        let config = deep_obsidian_config::expand_home_path(config)
            .canonicalize()
            .context("local configuration must exist")?;
        validate_local_config(&config)?;
        return complete_link(path, Target::Local { config }, options.json);
    }
    let docker_required = options.context.is_some() || options.container.is_some();
    let context = match options.context {
        Some(context) => Some(context),
        None => match current_context(runner) {
            Ok(context) => Some(context),
            Err(error) if docker_required => return Err(error),
            Err(_) => None,
        },
    };
    let containers = match &context {
        Some(context) => match docker_candidates(runner, context) {
            Ok(containers) => containers,
            Err(error) if docker_required => return Err(error),
            Err(_) => Vec::new(),
        },
        None => Vec::new(),
    };
    if let Some(selector) = options.container {
        let matches: Vec<_> = containers.iter().filter(|c| c.matches(&selector)).collect();
        if matches.len() != 1 {
            bail!("{selector:?} does not select exactly one running Deep Obsidian container in the chosen Docker context");
        }
        return complete_link(
            path,
            Target::Docker {
                context: context.unwrap(),
                container: matches[0].name.clone(),
            },
            options.json,
        );
    }
    let mut candidates: Vec<(String, Target)> = containers
        .into_iter()
        .map(|container| {
            let context = context.as_ref().unwrap().clone();
            (
                format!(
                    "Docker: {} — {} ({}, context {})",
                    container.name, container.image, container.status, context
                ),
                Target::Docker {
                    context,
                    container: container.name,
                },
            )
        })
        .collect();
    let mut local_paths = vec![default_config.to_path_buf()];
    if let Some(profile) = read_profile(path).ok().flatten() {
        if let Target::Local { config } = profile.target {
            if !local_paths.contains(&config) {
                local_paths.push(config);
            }
        }
        for config in profile.known_local_configs {
            if !local_paths.contains(&config) {
                local_paths.push(config);
            }
        }
    }
    for config in local_paths {
        if config.is_file() && validate_local_config(&config).is_ok() {
            let config = config.canonicalize()?;
            candidates.push((
                format!(
                    "Local: {} (configured; service not verified)",
                    config.display()
                ),
                Target::Local { config },
            ));
        }
    }
    match candidates.len() {
        0 => bail!("no running Deep Obsidian container or known local configuration found; select a Docker context with `link --context NAME` or use `link --local-config PATH`"),
        1 => complete_link(path, candidates.remove(0).1, options.json),
        _ if !interactive => {
            let choices = candidates.iter().enumerate().map(|(i, (label, _))| format!("  {}. {label}", i + 1)).collect::<Vec<_>>().join("\n");
            bail!("multiple backends found:\n{choices}\nselect explicitly with `link --container NAME [--context NAME]` or `link --local-config PATH`");
        }
        _ => {
            eprintln!("Choose the backend for future management commands:");
            for (index, (label, _)) in candidates.iter().enumerate() { eprintln!("  {}. {label}", index + 1); }
            eprint!("Backend [1-{}]: ", candidates.len());
            io::stderr().flush()?;
            let mut answer = String::new();
            io::stdin().read_line(&mut answer)?;
            let selected = answer.trim().parse::<usize>().ok().filter(|n| *n >= 1 && *n <= candidates.len())
                .context("no valid backend selected; previous link is unchanged")?;
            complete_link(path, candidates.remove(selected - 1).1, options.json)
        }
    }
}

fn validate_local_config(path: &Path) -> Result<()> {
    if !path.is_file() {
        bail!("local configuration is not a file: {}", path.display());
    }
    deep_obsidian_config::read_config_file(path)?.context("local configuration is empty")?;
    Ok(())
}

fn complete_link(path: &Path, target: Target, json: bool) -> Result<()> {
    save_profile(path, target.clone())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&read_profile(path)?)?);
    } else {
        match target {
            Target::Local { config } => {
                println!("Linked local configuration: {}", config.display())
            }
            Target::Docker { context, container } => {
                println!("Linked Docker backend: {container} (context {context})")
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake {
        outputs: RefCell<Vec<Output>>,
        calls: RefCell<Vec<Vec<String>>>,
        exit: i32,
    }
    impl Fake {
        fn new(outputs: Vec<Output>) -> Self {
            Self {
                outputs: RefCell::new(outputs),
                calls: RefCell::new(Vec::new()),
                exit: 0,
            }
        }
    }
    impl Runner for Fake {
        fn output(&self, args: &[String]) -> Result<Output> {
            self.calls.borrow_mut().push(args.to_vec());
            if self.outputs.borrow().is_empty() {
                bail!("unexpected Docker invocation");
            }
            Ok(self.outputs.borrow_mut().remove(0))
        }
        fn inherit(&self, args: &[String]) -> Result<i32> {
            self.calls.borrow_mut().push(args.to_vec());
            Ok(self.exit)
        }
    }
    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_owned()).collect()
    }
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "deep-obsidian-link-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn profile(&self) -> PathBuf {
            self.0.join("client.json")
        }
        fn config(&self) -> PathBuf {
            self.0.join("config.json")
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn out(stdout: &str) -> Output {
        Output {
            code: 0,
            stdout: stdout.as_bytes().to_vec(),
        }
    }
    const CONTAINERS: &str = r#"{"ID":"0123456789abcdef","Names":"deep-one","Image":"ghcr.io/p4ul-m/deep-obsidian-mcp:v1","Labels":"","Status":"Up 1 hour"}
{"ID":"fedcba9876543210","Names":"other","Image":"postgres:16","Labels":"","Status":"Up 1 hour"}"#;

    #[test]
    fn no_link_keeps_local_arguments_and_never_contacts_docker() {
        let temp = Temp::new();
        let fake = Fake::new(vec![]);
        let args = strings(&["mounts", "list"]);
        assert_eq!(
            dispatch_with(&args, &temp.profile(), &temp.config(), &fake, false, false).unwrap(),
            Dispatch::Continue(args)
        );
        assert!(fake.calls.borrow().is_empty());
    }
    #[test]
    fn native_link_injects_its_config_and_explicit_config_overrides_it() {
        let temp = Temp::new();
        fs::write(temp.config(), "{}").unwrap();
        save_profile(
            &temp.profile(),
            Target::Local {
                config: temp.config(),
            },
        )
        .unwrap();
        let fake = Fake::new(vec![]);
        let routed = dispatch_with(
            &strings(&["users", "list"]),
            &temp.profile(),
            &temp.config(),
            &fake,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            routed,
            Dispatch::Continue(vec![
                "--config".into(),
                temp.config().to_string_lossy().into_owned(),
                "users".into(),
                "list".into()
            ])
        );
        let args = strings(&["--config", "/other/config.json", "users", "list"]);
        assert_eq!(
            dispatch_with(&args, &temp.profile(), &temp.config(), &fake, false, false).unwrap(),
            Dispatch::Continue(args)
        );
    }
    #[test]
    fn unavailable_link_errors_without_local_fallback() {
        let temp = Temp::new();
        save_profile(
            &temp.profile(),
            Target::Docker {
                context: "watson".into(),
                container: "gone".into(),
            },
        )
        .unwrap();
        let fake = Fake::new(vec![out(CONTAINERS)]);
        let error = dispatch_with(
            &strings(&["users", "add", "laura"]),
            &temp.profile(),
            &temp.config(),
            &fake,
            false,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("no fallback"));
        assert_eq!(fake.calls.borrow().len(), 1);
    }
    #[test]
    fn docker_forwarding_preserves_arguments_and_uses_container_config() {
        let temp = Temp::new();
        save_profile(
            &temp.profile(),
            Target::Docker {
                context: "watson".into(),
                container: "deep-one".into(),
            },
        )
        .unwrap();
        let mut fake = Fake::new(vec![out(CONTAINERS)]);
        fake.exit = 23;
        let args = strings(&["users", "add", "space ; $(touch /tmp/never) ' name"]);
        assert_eq!(
            dispatch_with(&args, &temp.profile(), &temp.config(), &fake, false, false).unwrap(),
            Dispatch::Exit(23)
        );
        let calls = fake.calls.borrow();
        let forward = &calls[1];
        assert_eq!(
            &forward[..4],
            strings(&["--context", "watson", "exec", "-i"])
        );
        assert!(forward.contains(&format!("{FORWARDED}=1")));
        assert!(forward.contains(&CONTAINER_SCRIPT.to_owned()));
        assert_eq!(&forward[forward.len() - args.len()..], args);
        assert!(!CONTAINER_SCRIPT.contains("space ;"));
    }

    #[cfg(unix)]
    #[test]
    fn container_forwarding_loads_the_service_secret_without_exposing_it_in_arguments() {
        use std::os::unix::fs::PermissionsExt;

        let temp = Temp::new();
        let secrets = temp.0.join("secrets with spaces");
        fs::create_dir(&secrets).unwrap();
        let mounted = temp.0.join("mounted config.json");
        fs::write(&mounted, "{}").unwrap();
        let stub = temp.0.join("deep-obsidian-cli");
        fs::write(
            &stub,
            "#!/bin/sh\n[ \"${DEEP_OBSIDIAN_AUTH_TOKEN:-}\" = \"$EXPECTED_TOKEN\" ] || exit 42\nprintf '%s\\n' \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();

        let secret = "fixture-bearer-not-a-real-secret";
        // Match the entrypoint: a nonempty secret file wins over an inherited
        // environment value; empty/absent files leave that environment alone.
        for (file, inherited, expected) in [
            (
                Some(format!("{secret}\nignored second line\n")),
                Some("stale"),
                secret,
            ),
            (Some(" \n".to_owned()), Some("fallback"), "fallback"),
            (None, Some("fallback"), "fallback"),
            (None, None, ""),
        ] {
            let token_file = secrets.join("auth_token");
            if let Some(contents) = file {
                fs::write(&token_file, contents).unwrap();
            } else if token_file.exists() {
                fs::remove_file(&token_file).unwrap();
            }
            let mut command = Command::new("/bin/sh");
            command
                .args([
                    "-c",
                    CONTAINER_SCRIPT,
                    "deep-obsidian-cli",
                    "probe",
                    "--json",
                ])
                .env_clear()
                .env("PATH", format!("{}:/usr/bin:/bin", temp.0.display()))
                .env("DO_CONFIG_PATH", temp.config())
                .env("DO_MOUNTED_CONFIG", &mounted)
                .env("DO_SECRETS_DIR", &secrets)
                .env("EXPECTED_TOKEN", expected);
            if let Some(token) = inherited {
                command.env("DEEP_OBSIDIAN_AUTH_TOKEN", token);
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert_eq!(
                stdout.lines().collect::<Vec<_>>(),
                vec!["--config", mounted.to_str().unwrap(), "probe", "--json"]
            );
            assert!(!stdout.contains(secret));
            assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
        }
        assert!(
            !forward_args("watson", "deep-one", &strings(&["probe"]), false)
                .iter()
                .any(|argument| argument.contains(secret))
        );
    }
    #[test]
    fn explicit_selection_saves_context_and_stable_container_name() {
        let temp = Temp::new();
        let fake = Fake::new(vec![out(CONTAINERS)]);
        let args = strings(&["link", "--context", "watson", "--container", "0123456789ab"]);
        assert_eq!(
            dispatch_with(&args, &temp.profile(), &temp.config(), &fake, false, false).unwrap(),
            Dispatch::Exit(0)
        );
        assert_eq!(
            read_profile(&temp.profile()).unwrap().unwrap().target,
            Target::Docker {
                context: "watson".into(),
                container: "deep-one".into()
            }
        );
    }
    #[test]
    fn multiple_candidates_require_selection_noninteractively() {
        let temp = Temp::new();
        fs::write(temp.config(), "{}").unwrap();
        let fake = Fake::new(vec![out("desktop-linux\n"), out(CONTAINERS)]);
        let error = dispatch_with(
            &strings(&["link"]),
            &temp.profile(),
            &temp.config(),
            &fake,
            false,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("multiple backends"));
        assert!(!temp.profile().exists());
    }
    #[test]
    fn forwarded_and_client_install_calls_stay_local() {
        let temp = Temp::new();
        save_profile(
            &temp.profile(),
            Target::Docker {
                context: "watson".into(),
                container: "deep-one".into(),
            },
        )
        .unwrap();
        let fake = Fake::new(vec![]);
        for (args, forwarded) in [
            (strings(&["users", "list"]), true),
            (strings(&["setup-service", "--mcp"]), false),
            (strings(&["--local", "users", "list"]), false),
        ] {
            let expected = args
                .iter()
                .filter(|a| a.as_str() != "--local")
                .cloned()
                .collect();
            assert_eq!(
                dispatch_with(
                    &args,
                    &temp.profile(),
                    &temp.config(),
                    &fake,
                    forwarded,
                    false
                )
                .unwrap(),
                Dispatch::Continue(expected)
            );
        }
        assert!(fake.calls.borrow().is_empty());
    }
    #[test]
    fn invalid_profile_is_not_silently_ignored_for_management() {
        let temp = Temp::new();
        fs::write(
            temp.profile(),
            "{\"version\":99,\"target\":{\"kind\":\"local\",\"config\":\"/x\"}}",
        )
        .unwrap();
        let error = dispatch_with(
            &strings(&["mounts", "list"]),
            &temp.profile(),
            &temp.config(),
            &Fake::new(vec![]),
            false,
            false,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("unsupported client link version"));
    }
    #[test]
    fn unlink_only_removes_routing_profile() {
        let temp = Temp::new();
        fs::write(temp.config(), "{}").unwrap();
        save_profile(
            &temp.profile(),
            Target::Local {
                config: temp.config(),
            },
        )
        .unwrap();
        dispatch_with(
            &strings(&["unlink"]),
            &temp.profile(),
            &temp.config(),
            &Fake::new(vec![]),
            false,
            false,
        )
        .unwrap();
        assert!(!temp.profile().exists());
        assert!(temp.config().exists());
    }
    #[test]
    fn metadata_recognizes_compose_and_ignores_unrelated_images() {
        let fake = Fake::new(vec![out("{\"ID\":\"a\",\"Names\":\"custom\",\"Image\":\"local-build\",\"Labels\":\"com.docker.compose.service=deep-obsidian\",\"Status\":\"Up\"}\n{\"ID\":\"b\",\"Names\":\"random\",\"Image\":\"postgres:16\",\"Labels\":\"\",\"Status\":\"Up\"}")]);
        let candidates = docker_candidates(&fake, "default").unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].name, "custom");
    }
    #[test]
    fn stopped_linked_container_errors_without_changing_profile_or_falling_back() {
        let temp = Temp::new();
        fs::write(temp.config(), "{}").unwrap();
        save_profile(
            &temp.profile(),
            Target::Docker {
                context: "watson".into(),
                container: "deep-one".into(),
            },
        )
        .unwrap();
        let before = fs::read(temp.profile()).unwrap();
        let fake = Fake::new(vec![out("")]);
        let error = dispatch_with(
            &strings(&["users", "list"]),
            &temp.profile(),
            &temp.config(),
            &fake,
            false,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("no fallback was used"));
        assert_eq!(fs::read(temp.profile()).unwrap(), before);
        assert_eq!(fake.calls.borrow().len(), 1);
    }

    #[test]
    fn inaccessible_docker_context_never_falls_back_to_local_configuration() {
        let temp = Temp::new();
        fs::write(temp.config(), "{}").unwrap();
        save_profile(
            &temp.profile(),
            Target::Docker {
                context: "watson".into(),
                container: "deep-one".into(),
            },
        )
        .unwrap();
        let fake = Fake::new(vec![Output {
            code: 1,
            stdout: vec![],
        }]);
        let error = dispatch_with(
            &strings(&["mounts", "remove", "--id", "shared"]),
            &temp.profile(),
            &temp.config(),
            &fake,
            false,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("no fallback was used"));
        assert_eq!(fake.calls.borrow().len(), 1);
    }

    #[test]
    fn local_configuration_history_survives_linking_to_docker() {
        let temp = Temp::new();
        fs::write(temp.config(), "{}").unwrap();
        save_profile(
            &temp.profile(),
            Target::Local {
                config: temp.config(),
            },
        )
        .unwrap();
        save_profile(
            &temp.profile(),
            Target::Docker {
                context: "default".into(),
                container: "deep-one".into(),
            },
        )
        .unwrap();
        let profile = read_profile(&temp.profile()).unwrap().unwrap();
        assert_eq!(profile.known_local_configs, vec![temp.config()]);
        let fake = Fake::new(vec![out("default\n"), out(CONTAINERS)]);
        let nonexistent_default = temp.0.join("not-default.json");
        let error = dispatch_with(
            &strings(&["link"]),
            &temp.profile(),
            &nonexistent_default,
            &fake,
            false,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("multiple backends found"));
        assert!(error.to_string().contains("Local:"));
    }

    #[test]
    fn literal_arguments_after_double_dash_do_not_change_routing() {
        let args = strings(&["users", "add", "--", "--local", "--config=not-a-flag"]);
        let (stripped, local) = strip_local(&args);
        assert_eq!(stripped, args);
        assert!(!local);
        assert!(!has_explicit_config(&args));
    }

    #[test]
    fn user_with_missing_native_target_gets_an_error_instead_of_defaults() {
        let temp = Temp::new();
        let missing = temp.0.join("missing.json");
        save_profile(&temp.profile(), Target::Local { config: missing }).unwrap();
        let error = dispatch_with(
            &strings(&["users", "list"]),
            &temp.profile(),
            &temp.config(),
            &Fake::new(vec![]),
            false,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("no fallback was used"));
    }

    #[cfg(unix)]
    #[test]
    fn routing_profile_is_private_and_contains_no_credentials() {
        use std::os::unix::fs::PermissionsExt;
        let temp = Temp::new();
        save_profile(
            &temp.profile(),
            Target::Docker {
                context: "watson".into(),
                container: "deep-one".into(),
            },
        )
        .unwrap();
        assert_eq!(
            fs::metadata(temp.profile()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(temp.profile()).unwrap()).unwrap();
        assert_eq!(value["target"]["context"], "watson");
        assert!(value.get("password").is_none());
        assert!(value.get("token").is_none());
    }
}
