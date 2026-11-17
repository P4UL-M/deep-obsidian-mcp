//! Local, command-specific help derived from the management parser.
//!
//! This runs before target routing: reading help never needs a config, Docker,
//! a credential store, or a reachable backend. Validate the command path through
//! clap even when --help is present, so a typo cannot become successful help.
use anyhow::Result;
use clap::{error::ErrorKind, Arg, ArgAction, Command, CommandFactory};

const DOCS: &str = "https://github.com/P4UL-M/deep-obsidian-mcp/blob/main/docs/cli.md";
const SETTINGS: &[&str] = &[
    "vault_path",
    "index_dir",
    "packaged",
    "insecure_no_auth",
    "transport",
    "stdio_mode",
    "host",
    "port",
    "mcp_path",
    "health_path",
    "auto_reindex",
    "no_auto_reindex",
    "reindex_debounce_ms",
    "reindex_interval_ms",
    "embedding_provider",
    "embedding_model",
    "embedding_base_url",
];

pub(crate) fn print_if_requested(args: &[String]) -> Result<Option<i32>> {
    let Some(error) = requested(args) else {
        return Ok(None);
    };
    let status = error.exit_code();
    error.print()?;
    Ok(Some(status))
}

pub(crate) fn print_root() -> Result<()> {
    command().print_help()?;
    println!();
    Ok(())
}

fn requested(args: &[String]) -> Option<clap::Error> {
    let is_help = args.first().is_some_and(|arg| arg == "help");
    if !args.is_empty()
        && !is_help
        && !args
            .iter()
            .take_while(|arg| arg.as_str() != "--")
            .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        return None;
    }
    let mut query = vec!["deep-obsidian-cli".to_owned()];
    query.extend_from_slice(if is_help { &args[1..] } else { args });
    if args.is_empty() || is_help {
        query.push("--help".into());
    }
    // Validate the complete input before clap's early help exit. Missing
    // required values are normal when asking how to invoke a valid command.
    let validation = query
        .iter()
        .filter(|arg| !matches!(arg.as_str(), "--help" | "-h"))
        .cloned()
        .collect::<Vec<_>>();
    if let Err(error) = command().try_get_matches_from(validation) {
        if !matches!(
            error.kind(),
            ErrorKind::MissingRequiredArgument
                | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        ) {
            return Some(error);
        }
    }
    match command().try_get_matches_from(query) {
        Err(error) => Some(error),
        Ok(_) => None,
    }
}

fn command() -> Command {
    let mut root = crate::cli::Cli::command()
        .subcommand(crate::link::help_command())
        .subcommand(Command::new("unlink").about("Clear the saved backend link"))
        .arg(
            Arg::new("local")
                .long("local")
                .global(true)
                .action(ArgAction::SetTrue),
        )
        .disable_version_flag(false);
    // Propagate globals before choosing what each help page displays. Every
    // accepted management flag keeps its parser definition and validation.
    root.build();
    root = root.mut_arg("version", |arg| arg.short('v'));
    style(root, &[])
}

fn style(command: Command, parents: &[String]) -> Command {
    let mut path = parents.to_vec();
    if !parents.is_empty() || command.get_name() != "deep-obsidian-cli" {
        path.push(command.get_name().to_owned());
    }
    let family = path.first().map(String::as_str);
    let action = path.get(1).map(String::as_str);
    let writing = match family {
        Some("setup-service") => true,
        Some("mounts") => matches!(action, Some("add" | "remove")),
        Some("secrets") => action == Some("set"),
        Some("users") => matches!(
            action,
            Some(
                "add"
                    | "remove"
                    | "grant"
                    | "revoke"
                    | "rotate-password"
                    | "default"
                    | "enable"
                    | "disable"
            )
        ),
        Some("couchdb") => action == Some("restore"),
        Some("algolia") => matches!(action, Some("seed" | "restore" | "retract")),
        _ => false,
    };
    let order = if path.len() == 1 {
        [
            "setup-service",
            "link",
            "unlink",
            "mounts",
            "users",
            "secrets",
            "doctor",
            "probe",
            "print-config",
            "couchdb",
            "algolia",
            "help",
            "version",
        ]
        .iter()
        .position(|n| *n == command.get_name())
        .unwrap_or(99)
    } else {
        0
    };
    let mut command = command
        .display_order(order)
        .term_width(80)
        .max_term_width(80)
        .long_about(None)
        .mut_args(|arg| {
            let id = arg.get_id().as_str().to_owned();
            let visible = if SETTINGS.contains(&id.as_str()) && arg.is_global_set() {
                family == Some("setup-service")
                    || (id == "index_dir"
                        && family == Some("mounts")
                        && path.get(1).is_some_and(|s| s == "add"))
            } else {
                match id.as_str() {
                    "no_json" | "no_dry_run" => false,
                    "json" => family.is_some_and(|f| !matches!(f, "unlink" | "help" | "version")),
                    "dry_run" => writing,
                    "config" | "local" => {
                        !matches!(family, Some("link" | "unlink" | "help" | "version"))
                    }
                    _ => true,
                }
            };
            let mut arg = arg.hide(!visible).long_help(None);
            if SETTINGS.contains(&id.as_str())
                && arg.is_global_set()
                && family == Some("setup-service")
            {
                arg = arg
                    .help_heading("Service settings")
                    .display_order(100 + SETTINGS.iter().position(|key| *key == id).unwrap());
            }
            match id.as_str() {
                "help" => arg
                    .action(ArgAction::HelpShort)
                    .help("Show help for this command"),
                "config" => arg.help("Use a local configuration file; overrides the saved link"),
                "local" => arg.help("Use local configuration instead of the saved link"),
                "json" => arg.help("Print machine-readable JSON"),
                "dry_run" => arg.help("Preview changes without writing"),
                "password_stdin" => {
                    arg.help("Read the password from stdin; otherwise prompted masked")
                }
                "api_key_stdin" => {
                    arg.help("Read the API key from stdin; otherwise prompted masked")
                }
                _ => arg,
            }
        });
    command = match path.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => command.after_help(format!("Start here:\n  deep-obsidian-cli setup-service --wizard\n  deep-obsidian-cli link\n\nCommand help: deep-obsidian-cli <command> --help\nRun the service: deep-obsidian-mcp --help\nGuide: {DOCS}")),
        ["setup-service"] => command.after_help("Start with: deep-obsidian-cli setup-service --wizard\nUse mounts to change an existing mount table. Preview with --dry-run."),
        ["link"] => command.after_help("Examples:\n  deep-obsidian-cli link\n  deep-obsidian-cli link --context watson\n  deep-obsidian-cli link --local-config /path/config.json\n\nDiscovery uses one Docker context and known local configs; it does not scan the network."),
        ["doctor"] => command.after_help("Local checks run by default. Add --probe-remote to check remote mounts read-only.\nUse probe to test the HTTP health and MCP endpoints."),
        ["secrets"] => command.after_help("Use check to identify each secret's store; set replaces a stored value.\nBearer-token recovery and Docker secret files: docs/cli.md#recover-a-lost-bearer-token"),
        ["secrets", "set"] => command.after_help("Examples:\n  deep-obsidian-cli secrets set --target auth-token\n  deep-obsidian-cli secrets set --mount team\n\nThe replacement is prompted masked, or read with --stdin. Rotation keeps the existing\nstore and reference; it does not change configuration or migrate credentials."),
        ["mounts"] => command.after_help("Examples:\n  deep-obsidian-cli mounts list\n  deep-obsidian-cli mounts add couchdb --help\n\nRemoving a mount preserves its backing data and stored credentials."),
        ["users"] => command.after_help("Start with: deep-obsidian-cli users enable --wizard\nPasswords are generated once at creation; recover a lost password with rotate-password.\nDisabling multiuser deletes all application users and restores classic all-vault access."),
        _ => command,
    };
    command.mut_subcommands(|child| style(child, &path))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn root_is_short_and_command_typos_are_errors() {
        let root = requested(&[]).unwrap();
        assert_eq!(root.kind(), ErrorKind::DisplayHelp);
        let text = root.to_string();
        assert!(text.lines().count() <= 45, "{text}");
        assert!(!text.contains("--embedding-model"), "{text}");
        for args in [
            vec!["doctro", "--help"],
            vec!["users", "enabel", "--help"],
            vec!["help", "mounts", "ad"],
        ] {
            let error =
                requested(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).unwrap();
            assert_ne!(error.kind(), ErrorKind::DisplayHelp, "{error}");
            assert_ne!(error.exit_code(), 0);
        }
    }
}
