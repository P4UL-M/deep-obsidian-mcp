//! `deep-obsidian-mcp share ...` — publish local prefixes to a shared Algolia
//! index, inspect the plan, and mint secured teammate keys.
//!
//! Publication is an explicit act (design §9): the FIRST push to an index
//! prints the complete note list and requires `--yes` or interactive
//! confirmation; `--dry-run` prints the plan without writing, any time.

use anyhow::{anyhow, Context, Result};
use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_types::SecretRef;
use secrecy::{ExposeSecret, SecretString};
use deep_obsidian_server::shared::{
    connect_mount,
    push::{apply_push, plan_push, remove_seeded_local_files, PushAction},
    reads, SharedMountRuntime,
};
use deep_obsidian_types::{SharedExportConfig, SharedMountConfig};

use crate::cli::ShareAction;
use crate::config::ResolvedRuntimeConfig;

fn export_mounts(
    resolved: &ResolvedRuntimeConfig,
    index_filter: Option<&str>,
    require_export: bool,
) -> Result<Vec<SharedMountRuntime>> {
    let secrets = SecretResolver::new();
    let mut mounts = Vec::new();
    for config in &resolved.service.shared {
        if let Some(filter) = index_filter {
            if config.index_name != filter {
                continue;
            }
        }
        if require_export && config.export.is_none() {
            continue;
        }
        mounts.push(
            connect_mount(config, &secrets, &resolved.service.index_dir)
                .map_err(|error| anyhow!("mount {}: {error}", config.index_name))?,
        );
    }
    if mounts.is_empty() {
        return Err(anyhow!(
            "no shared mount matches (configure `shared` in the config file{})",
            index_filter
                .map(|filter| format!(", or check --index {filter}"))
                .unwrap_or_default()
        ));
    }
    Ok(mounts)
}

/// Picks exactly one configured mount: the named one, or the only one.
fn select_mount_config(
    resolved: &ResolvedRuntimeConfig,
    index: Option<&str>,
) -> Result<SharedMountConfig> {
    let mut candidates: Vec<_> = resolved
        .service
        .shared
        .iter()
        .filter(|mount| index.map(|name| mount.index_name == name).unwrap_or(true))
        .cloned()
        .collect();
    match (candidates.len(), index) {
        (0, _) => Err(anyhow!(
            "no shared mount matches; configure `shared` first (or check --index)"
        )),
        (1, _) => Ok(candidates.remove(0)),
        (_, None) => Err(anyhow!(
            "several mounts configured; pick one with --index <name>"
        )),
        (_, Some(_)) => Ok(candidates.remove(0)),
    }
}

fn print_plan(mount: &SharedMountRuntime, plan: &deep_obsidian_server::shared::push::PushPlan) {
    println!(
        "mount {} (app {}, mounted at {}):",
        mount.index(),
        mount.config.app_id,
        mount.mount_at()
    );
    // A frequent wizard-answer trap: exporting the mount path itself. The
    // mount is a VIRTUAL namespace (nothing on disk), so such a prefix can
    // never match a local file — say so instead of printing "nothing to do".
    if let Some(export) = &mount.config.export {
        for prefix in &export.prefixes {
            if prefix.starts_with(mount.mount_at()) || mount.mount_at().starts_with(prefix.as_str())
            {
                println!(
                    "  WARNING: export prefix {prefix} lies inside the virtual mount \
{} — it matches no local files. Export a LOCAL folder instead (e.g. _Wiki/).",
                    mount.mount_at()
                );
            }
        }
    }
    if plan.first_push {
        println!("  FIRST PUSH to this index — full note list:");
    }
    for item in &plan.items {
        let label = match item.action {
            PushAction::Create => "create",
            PushAction::Update => "update",
            PushAction::Unchanged => continue,
        };
        println!("  {label}  {}", item.path);
    }
    let unchanged = plan.items.len() - plan.changed_count();
    if unchanged > 0 {
        println!("  ({unchanged} unchanged)");
    }
    for path in &plan.retract {
        println!("  retract  {path}  (removes it AND its history)");
    }
    for path in &plan.foreign_orphans {
        println!("  keep  {path}  (last written by another participant; not retracted)");
    }
    if plan.changed_count() == 0 && plan.retract.is_empty() {
        println!("  nothing to do");
    }
}

pub async fn run_share(
    resolved: &ResolvedRuntimeConfig,
    action: &ShareAction,
    dry_run: bool,
) -> Result<()> {
    match action {
        ShareAction::Status => {
            for mount in export_mounts(resolved, None, false)? {
                if mount.config.export.is_some() {
                    let plan = plan_push(&resolved.service.vault_path, &mount).await?;
                    print_plan(&mount, &plan);
                } else {
                    println!(
                        "mount {} (consume-only, mounted at {})",
                        mount.index(),
                        mount.mount_at()
                    );
                }
            }
            Ok(())
        }
        ShareAction::Push { yes, index } => {
            for mount in export_mounts(resolved, index.as_deref(), true)? {
                let plan = plan_push(&resolved.service.vault_path, &mount).await?;
                print_plan(&mount, &plan);
                if dry_run {
                    println!("(dry-run: nothing written)");
                    continue;
                }
                if plan.changed_count() == 0 && plan.retract.is_empty() {
                    continue;
                }
                if plan.first_push && !yes {
                    // Opt-out confirmation: protects against a wrong prefix
                    // shipping unintended content on the very first publish.
                    if !confirm(&format!(
                        "Publish these {} note(s) to index {}?",
                        plan.items.len(),
                        mount.index()
                    ))? {
                        println!("aborted");
                        continue;
                    }
                }
                let report = apply_push(&resolved.service.vault_path, &mount, &plan).await?;
                println!(
                    "pushed {} note(s), {} unchanged, {} retracted",
                    report.pushed, report.unchanged, report.retracted
                );
            }
            Ok(())
        }
        ShareAction::Seed {
            prefixes,
            move_files,
            index,
            yes,
        } => {
            let persistent = select_mount_config(resolved, index.as_deref())?;
            let normalized: Vec<String> = prefixes
                .iter()
                .map(|prefix| {
                    let trimmed = prefix.trim().trim_start_matches('/');
                    if trimmed.ends_with('/') {
                        trimmed.to_string()
                    } else {
                        format!("{trimmed}/")
                    }
                })
                .filter(|prefix| !prefix.is_empty() && *prefix != "/")
                .collect();
            if normalized.is_empty() {
                return Err(anyhow!("--prefix requires a vault-relative folder, e.g. _Wiki/"));
            }
            // Ephemeral export rule: the config file is NOT modified — seed is
            // one-shot by design (model C keeps no standing export).
            let mut seed_config = persistent.clone();
            seed_config.export = Some(SharedExportConfig {
                prefixes: normalized.clone(),
                exclude: Vec::new(),
            });
            let secrets = SecretResolver::new();
            let mount = connect_mount(&seed_config, &secrets, &resolved.service.index_dir)
                .map_err(|error| anyhow!("mount {}: {error}", seed_config.index_name))?;

            // A PERSISTENT export overlapping the seeded prefixes would retract
            // everything on the next `share push` once --move removes the local
            // files. Refuse the foot-gun combination outright.
            if *move_files {
                if let Some(export) = &persistent.export {
                    let overlap: Vec<&String> = export
                        .prefixes
                        .iter()
                        .filter(|kept| {
                            normalized.iter().any(|seeded| {
                                kept.starts_with(seeded.as_str())
                                    || seeded.starts_with(kept.as_str())
                            })
                        })
                        .collect();
                    if !overlap.is_empty() {
                        return Err(anyhow!(
                            "config keeps a persistent export over {overlap:?}: after --move the \
next `share push` would RETRACT the seeded notes. Remove the export rule from the config first."
                        ));
                    }
                }
            }

            let mut plan = plan_push(&resolved.service.vault_path, &mount).await?;
            plan.retract.clear(); // seed only adds, never reconciles deletions
            print_plan(&mount, &plan);
            if dry_run {
                println!("(dry-run: nothing written)");
                return Ok(());
            }
            let seeded_count = plan.items.len();
            if seeded_count == 0 {
                println!("no local notes under {:?}", normalized);
                return Ok(());
            }
            if (plan.first_push || *move_files) && !yes {
                let question = if *move_files {
                    format!(
                        "Import {seeded_count} note(s) into index {} and DELETE the local copies?",
                        mount.index()
                    )
                } else {
                    format!(
                        "Import {seeded_count} note(s) into index {}?",
                        mount.index()
                    )
                };
                if !confirm(&question)? {
                    println!("aborted");
                    return Ok(());
                }
            }
            if plan.changed_count() > 0 {
                let report = apply_push(&resolved.service.vault_path, &mount, &plan).await?;
                println!("seeded {} note(s), {} already up to date", report.pushed, report.unchanged);
            } else {
                println!("index already up to date");
            }
            if *move_files {
                let (deleted, skipped) =
                    remove_seeded_local_files(&resolved.service.vault_path, &mount).await?;
                for path in &deleted {
                    println!("  removed local  {path}");
                }
                for path in &skipped {
                    println!("  kept (drifted since push)  {path}");
                }
                println!(
                    "{} local file(s) removed; the index now holds the only copy",
                    deleted.len()
                );
            }
            println!(
                "read/write the wiki through the mount: {}",
                mount.mount_at()
            );
            Ok(())
        }
        ShareAction::Dump { to, index } => {
            let config = select_mount_config(resolved, index.as_deref())?;
            let secrets = SecretResolver::new();
            let mount = connect_mount(&config, &secrets, &resolved.service.index_dir)
                .map_err(|error| anyhow!("mount {}: {error}", config.index_name))?;
            let target = if to.is_absolute() {
                to.clone()
            } else {
                std::env::current_dir()?.join(to)
            };
            if target.starts_with(&resolved.service.vault_path) {
                println!(
                    "WARNING: {} is inside the vault — the dumped notes will be indexed \
locally and show up twice in search.",
                    target.display()
                );
            }
            let report = reads::dump_all(&mount, &target).await?;
            println!(
                "dumped {} note(s) ({} bytes) from index {} to {}",
                report.notes,
                report.bytes,
                mount.index(),
                target.display()
            );
            for path in &report.hash_mismatches {
                println!("  WARNING: hash mismatch on {path} (reassembly differs from record)");
            }
            Ok(())
        }
        ShareAction::SetKey { index } => {
            // Repair/rotation path: store the key for an existing mount and
            // make sure the config's keyRef points at it.
            let mount_config = select_mount_config(resolved, index.as_deref())?;
            let secret = crate::commands::prompt_optional_secret(&format!(
                "Algolia API key for index {}",
                mount_config.index_name
            ))?
            .ok_or_else(|| anyhow!("no key entered; nothing stored"))?;

            let reference = mount_config.key_ref.clone().unwrap_or(SecretRef::OsKeyring {
                service: "deep-obsidian-mcp".to_string(),
                account: format!("algolia-{}", mount_config.index_name),
            });
            let resolver = SecretResolver::new();
            let stored_reference = match resolver.put(
                &reference,
                SecretString::from(secret.expose_secret().to_string()),
            ) {
                Ok(()) => reference,
                Err(error) => {
                    println!("OS keyring unavailable: {error}");
                    println!("Falling back to encrypted local file storage.");
                    let fallback = SecretRef::EncryptedFile {
                        id: format!("algolia-{}", mount_config.index_name),
                    };
                    resolver.put(&fallback, secret)?;
                    fallback
                }
            };

            // VERIFY the round trip through a fresh resolver: a backend that
            // accepts writes but cannot return them (the historical mock-store
            // failure) must fail HERE, not at the next `share push`.
            let verified = SecretResolver::new()
                .get(&stored_reference)
                .context("verification read failed")?
                .is_some();
            if !verified {
                return Err(anyhow!(
                    "the key was accepted but cannot be read back — secret storage on this \
system is not persisting values; check the keyring backend"
                ));
            }

            // Persist the keyRef if the config file doesn't carry it yet (or
            // carries a different one).
            let mut persisted = deep_obsidian_config::read_config_file(&resolved.config_path)?
                .ok_or_else(|| anyhow!("config file not found: {}", resolved.config_path.display()))?;
            let mut config_changed = false;
            for mount in persisted.shared.iter_mut() {
                if mount.index_name == mount_config.index_name
                    && mount.key_ref.as_ref() != Some(&stored_reference)
                {
                    mount.key_ref = Some(stored_reference.clone());
                    config_changed = true;
                }
            }
            if config_changed {
                deep_obsidian_config::write_config_file(&resolved.config_path, &persisted)?;
                println!("updated keyRef in {}", resolved.config_path.display());
            }
            println!(
                "key stored and verified for mount {} ({})",
                mount_config.index_name,
                match &stored_reference {
                    SecretRef::OsKeyring { .. } => "OS keyring",
                    SecretRef::EncryptedFile { .. } => "encrypted file",
                }
            );
            Ok(())
        }
        ShareAction::Key { index, filters } => {
            let mounts = export_mounts(resolved, index.as_deref(), false)?;
            let mount = mounts
                .first()
                .ok_or_else(|| anyhow!("no matching shared mount"))?;
            let secrets = SecretResolver::new();
            let parent_key =
                deep_obsidian_server::shared::resolve_api_key(&mount.config, &secrets)
                    .map_err(|error| anyhow!("{error}"))?;
            let restrictions = filters
                .as_deref()
                .map(|filters| format!("filters={}", urlencoding_encode(filters)))
                .unwrap_or_default();
            let key =
                deep_obsidian_algolia::generate_secured_api_key(&parent_key, &restrictions);
            println!("secured key (read-only{}):",
                filters
                    .as_deref()
                    .map(|filters| format!(", scoped to `{filters}`"))
                    .unwrap_or_default()
            );
            println!("{key}");
            Ok(())
        }
    }
}

fn urlencoding_encode(value: &str) -> String {
    // Minimal query-string escaping for the restriction payload.
    value
        .chars()
        .map(|ch| match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => ch.to_string(),
            _ => format!("%{:02X}", ch as u32),
        })
        .collect()
}

fn confirm(question: &str) -> Result<bool> {
    use std::io::Write;
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}
