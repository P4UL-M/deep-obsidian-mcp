//! User administration through the backend's restricted control interface.
//!
//! These commands create application identities only. They never provision a
//! CouchDB database/account, Linux user or storage backend.
use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use deep_obsidian_control::{Grant, InitialUser, Role};
use deep_obsidian_types::{MountConfig, ResolvedServiceConfig};
use serde_json::{json, Value};

use crate::wizard::AnswerReader;

#[derive(Debug, Clone, Subcommand)]
pub enum UsersCommand {
    /// List application users.
    List,
    /// Report whether multiuser is enabled.
    Status,
    /// Create an application user and display the generated password once.
    Add { name: String },
    /// Delete an application user and revoke their authorizations.
    Remove { name: String },
    /// Show a user's mount grants and default destination.
    Show { name: String },
    /// Assign or replace one mount role. This does not provision storage.
    Grant {
        name: String,
        #[arg(long)]
        mount: String,
        #[arg(long, value_parser=parse_role)]
        role: Role,
    },
    /// Remove access to a mount.
    Revoke {
        name: String,
        #[arg(long)]
        mount: String,
    },
    /// Generate a new password and revoke the user's existing sessions.
    RotatePassword { name: String },
    /// Set the default write destination to an accessible mount, or clear it.
    Default {
        name: String,
        #[arg(long, required_unless_present = "clear", conflicts_with = "clear")]
        mount: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    /// Enable multiuser atomically after explicitly assigning users and mount roles.
    Enable {
        /// Guide user and mount assignments before one final confirmation.
        #[arg(long, conflicts_with = "plan")]
        wizard: bool,
        /// JSON array of initial users and grants, without passwords.
        #[arg(long, conflicts_with = "wizard")]
        plan: Option<PathBuf>,
        /// Accept the displayed assignments (required for a noninteractive plan).
        #[arg(long)]
        yes: bool,
    },
    /// Delete all application users/grants and restore classic access to all mounts.
    Disable {
        #[arg(long)]
        yes: bool,
    },
}

fn parse_role(value: &str) -> std::result::Result<Role, String> {
    match value {
        "reader" => Ok(Role::Reader),
        "editor" => Ok(Role::Editor),
        "owner" => Ok(Role::Owner),
        _ => Err("role must be reader, editor or owner".to_owned()),
    }
}

/// Execute against a linked/running backend or an exclusively locked offline one.
/// Generated credentials are emitted only in this explicit operator response.
pub async fn run(
    command: UsersCommand,
    config: &ResolvedServiceConfig,
    dry_run: bool,
    json_output: bool,
) -> Result<()> {
    let request = match command {
        UsersCommand::List => json!({"operation":"users.list"}),
        UsersCommand::Status => json!({"operation":"multiuser.status"}),
        UsersCommand::Add { name } => json!({"operation":"users.add","name":name}),
        UsersCommand::Remove { name } => json!({"operation":"users.remove","name":name}),
        UsersCommand::Show { name } => json!({"operation":"users.show","name":name}),
        UsersCommand::Grant { name, mount, role } => {
            json!({"operation":"users.grant","name":name,"mount":mount,"role":role})
        }
        UsersCommand::Revoke { name, mount } => {
            json!({"operation":"users.revoke","name":name,"mount":mount})
        }
        UsersCommand::RotatePassword { name } => {
            json!({"operation":"users.rotate-password","name":name})
        }
        UsersCommand::Default { name, mount, .. } => {
            json!({"operation":"users.default","name":name,"defaultMount":mount})
        }
        UsersCommand::Enable { plan, yes, .. } => {
            let users = if let Some(path) = plan {
                let users: Vec<InitialUser> =
                    serde_json::from_slice(&std::fs::read(&path).with_context(|| {
                        format!("cannot read enablement plan {}", path.display())
                    })?)
                    .context("enablement plan must be an array of users and explicit grants")?;
                validate_plan(&users, &config.mount_table())?;
                if !json_output {
                    print_plan(&users);
                }
                confirm("Enable multiuser with these assignments?", yes, dry_run)?;
                users
            } else {
                require_terminal("users enable --wizard")?;
                let mut answers = AnswerReader::interactive();
                let users = collect_initial_users(&mut answers, &config.mount_table())?;
                if !yes
                    && !dry_run
                    && !answers.yes_no("Enable multiuser with these assignments?", true)?
                {
                    bail!("cancelled; multiuser was not enabled");
                }
                users
            };
            json!({"operation":"multiuser.enable","users":users})
        }
        UsersCommand::Disable { yes } => {
            confirm(
                "Delete ALL application users/grants and restore classic access to ALL mounts?",
                yes,
                dry_run,
            )?;
            json!({"operation":"multiuser.disable"})
        }
    };
    if dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"dryRun":true,"request":request}))?
        );
        return Ok(());
    }
    let result = deep_obsidian_server::control::call_local(config, request).await?;
    print_result(&result, json_output)
}

fn require_terminal(command: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("{command} requires an interactive terminal; use --plan <file> --yes for noninteractive enablement");
    }
    Ok(())
}
fn confirm(prompt: &str, yes: bool, dry_run: bool) -> Result<()> {
    if yes || dry_run {
        return Ok(());
    }
    require_terminal("confirmation")?;
    if !AnswerReader::interactive().yes_no(prompt, false)? {
        bail!("cancelled; no change made");
    }
    Ok(())
}

/// Shared by the setup wizard and `users enable --wizard`. No side effects or
/// passwords exist until the backend commits this entire plan.
pub fn collect_initial_users(
    answers: &mut AnswerReader,
    mounts: &[MountConfig],
) -> Result<Vec<InitialUser>> {
    let mut users = Vec::new();
    loop {
        let name = answers.required_line("User name", None)?;
        if users.iter().any(|u: &InitialUser| u.name == name) {
            println!("That name is already in this enablement plan.");
            continue;
        }
        let mut grants = Vec::new();
        for mount in mounts {
            let label = format!(
                "Access to {} ({})",
                mount.id,
                if mount.mount_at.is_empty() {
                    "/"
                } else {
                    &mount.mount_at
                }
            );
            let selected =
                answers.choice(&label, &["No access", "Reader", "Editor", "Owner"], 0)?;
            let role = match selected {
                1 => Some(Role::Reader),
                2 => Some(Role::Editor),
                3 => Some(Role::Owner),
                _ => None,
            };
            if let Some(role) = role {
                grants.push(Grant {
                    mount_id: mount.id.clone(),
                    role,
                });
            }
        }
        let default_mount = if grants.len() == 1 {
            Some(grants[0].mount_id.clone())
        } else if grants.len() > 1 {
            let mut choices = vec!["Require an explicit destination".to_owned()];
            choices.extend(grants.iter().map(|g| g.mount_id.clone()));
            let options = choices.iter().map(String::as_str).collect::<Vec<_>>();
            let selection = answers.choice("Default write destination", &options, 0)?;
            (selection > 0).then(|| grants[selection - 1].mount_id.clone())
        } else {
            None
        };
        users.push(InitialUser {
            name,
            grants,
            default_mount,
        });
        if !answers.yes_no("Add another user?", false)? {
            break;
        }
    }
    validate_plan(&users, mounts)?;
    print_plan(&users);
    let assigned = users
        .iter()
        .flat_map(|u| &u.grants)
        .map(|g| g.mount_id.as_str())
        .collect::<std::collections::HashSet<_>>();
    for mount in mounts {
        if !assigned.contains(mount.id.as_str()) {
            println!("{}: inaccessible to ordinary users (no grant)", mount.id);
        }
    }
    Ok(users)
}

fn validate_plan(users: &[InitialUser], mounts: &[MountConfig]) -> Result<()> {
    if users.is_empty() {
        bail!("at least one user is required");
    }
    let mut names = std::collections::HashSet::new();
    for user in users {
        if user.name.is_empty()
            || user.name.len() > 64
            || !user
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            bail!("invalid username; use 1–64 ASCII letters, digits, dots, underscores or hyphens");
        }
        if !names.insert(&user.name) {
            bail!("duplicate username in enablement plan");
        }
        let mut seen = std::collections::HashSet::new();
        for grant in &user.grants {
            if !mounts.iter().any(|m| m.id == grant.mount_id) {
                bail!("unknown mount {}", grant.mount_id);
            }
            if !seen.insert(&grant.mount_id) {
                bail!("duplicate mount assignment for {}", user.name);
            }
            if !users.iter().any(|u| {
                u.grants
                    .iter()
                    .any(|g| g.mount_id == grant.mount_id && g.role == Role::Owner)
            }) {
                bail!(
                    "mount {} needs at least one owner; assign an owner before enabling",
                    grant.mount_id
                );
            }
        }
        if let Some(default) = &user.default_mount {
            if !user.grants.iter().any(|g| &g.mount_id == default) {
                bail!("default mount requires an access grant");
            }
        }
    }
    Ok(())
}
fn print_plan(users: &[InitialUser]) {
    println!("Multiuser assignments (passwords will be generated after confirmation):");
    for user in users {
        println!("  {}", user.name);
        for grant in &user.grants {
            println!(
                "    {}: {}",
                grant.mount_id,
                match grant.role {
                    Role::Reader => "reader",
                    Role::Editor => "editor",
                    Role::Owner => "owner",
                }
            );
        }
        if user.grants.is_empty() {
            println!("    no vault access");
        }
        if let Some(m) = &user.default_mount {
            println!("    default: {m}");
        }
    }
}
/// Operator output intentionally contains freshly generated credentials when the
/// request creates/rotates one. Do not send it to the server's logs.
pub(crate) fn print_result(result: &Value, _json_output: bool) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(result)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mounts() -> Vec<MountConfig> {
        vec![MountConfig {
            id: "private".into(),
            mount_at: String::new(),
            backend: deep_obsidian_types::MountBackendConfig::Filesystem {
                vault_path: PathBuf::from("/tmp/vault"),
                index_dir: None,
            },
            recall_weight: None,
            unknown: Default::default(),
        }]
    }
    #[test]
    fn wizard_collects_explicit_grants_and_never_generates_credentials() {
        let users = collect_initial_users(
            &mut AnswerReader::from_lines(vec!["paul".into(), "4".into(), "n".into()]),
            &mounts(),
        )
        .unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].grants[0].role, Role::Owner);
        assert_eq!(users[0].default_mount, Some("private".into()));
        let serialized = serde_json::to_string(&users).unwrap();
        assert!(!serialized.contains("password"));
    }
    #[test]
    fn wizard_cancel_eof_and_invalid_ownership_do_not_produce_a_plan() {
        assert!(collect_initial_users(
            &mut AnswerReader::from_lines(vec!["paul".into()]),
            &mounts()
        )
        .is_err());
        assert!(collect_initial_users(
            &mut AnswerReader::from_lines(vec!["paul".into(), "2".into(), "n".into()]),
            &mounts()
        )
        .is_err());
        assert!(validate_plan(&[], &mounts()).is_err());
    }
}
