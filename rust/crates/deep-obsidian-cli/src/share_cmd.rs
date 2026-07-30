//! `deep-obsidian-mcp share ...` — operate on a shared Algolia wiki.
//!
//! Model C (design: docs/algolia-shared-wiki.md): the wiki lives in the index
//! and is authored through the mount, so there is no standing export and no
//! recurring push. Local content enters once via `seed`; `dump` materializes
//! the index back out; `retract` is the single destructive operation.

use anyhow::{anyhow, Context, Result};
use deep_obsidian_config::secrets::SecretResolver;
use deep_obsidian_server::shared::{
    connect_mount, reads,
    seed::{apply_seed, plan_seed, remove_seeded_local_files, SeedAction},
    versioning, SharedMountRuntime,
};
use deep_obsidian_types::{SecretRef, SharedMountConfig};
use secrecy::{ExposeSecret, SecretString};

use crate::cli::ShareAction;
use crate::config::ResolvedRuntimeConfig;

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

fn connect(
    resolved: &ResolvedRuntimeConfig,
    config: &SharedMountConfig,
) -> Result<SharedMountRuntime> {
    let secrets = SecretResolver::new();
    connect_mount(config, &secrets, &resolved.service.index_dir)
        .map_err(|error| anyhow!("mount {}: {error}", config.index_name))
}

/// Normalizes `--prefix` values to trailing-slash, vault-relative folders.
fn normalize_prefixes(prefixes: &[String]) -> Result<Vec<String>> {
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
        .filter(|prefix| !prefix.is_empty() && prefix != "/")
        .collect();
    if normalized.is_empty() {
        return Err(anyhow!(
            "--prefix requires a vault-relative folder, e.g. _Wiki/"
        ));
    }
    Ok(normalized)
}

/// Strips the mount prefix from a user-supplied path so both the mounted form
/// (`_Shared/Team/_Wiki/Foo.md`) and the index-relative form (`_Wiki/Foo.md`)
/// address the same note.
fn to_remote_path(mount: &SharedMountRuntime, path: &str) -> String {
    let trimmed = path.trim().trim_start_matches('/');
    trimmed
        .strip_prefix(mount.mount_at())
        .unwrap_or(trimmed)
        .to_string()
}

pub async fn run_share(
    resolved: &ResolvedRuntimeConfig,
    action: &ShareAction,
    dry_run: bool,
) -> Result<()> {
    match action {
        ShareAction::Seed {
            prefixes,
            move_files,
            index,
            yes,
        } => {
            let normalized = normalize_prefixes(prefixes)?;
            let config = select_mount_config(resolved, index.as_deref())?;
            let mount = connect(resolved, &config)?;

            // Seeding the mount's own prefix would "import" the virtual
            // namespace — there are no files there by construction.
            for prefix in &normalized {
                if prefix.starts_with(mount.mount_at())
                    || mount.mount_at().starts_with(prefix.as_str())
                {
                    return Err(anyhow!(
                        "prefix {prefix} lies inside the virtual mount {} — it matches no local \
files. Seed a LOCAL folder instead (e.g. _Wiki/).",
                        mount.mount_at()
                    ));
                }
            }

            let plan = plan_seed(&resolved.service.vault_path, &mount, &normalized).await?;
            println!(
                "mount {} (app {}, mounted at {}):",
                mount.index(),
                mount.config.app_id,
                mount.mount_at()
            );
            if plan.first_push {
                println!("  FIRST IMPORT into this index — full note list:");
            }
            for item in &plan.items {
                let label = match item.action {
                    SeedAction::Create => "create",
                    SeedAction::Update => "update",
                    SeedAction::Unchanged => continue,
                };
                println!("  {label}  {}", item.path);
            }
            let unchanged = plan.items.len() - plan.changed_count();
            if unchanged > 0 {
                println!("  ({unchanged} already up to date)");
            }
            if plan.items.is_empty() {
                println!("  no local notes under {normalized:?}");
                return Ok(());
            }
            if dry_run {
                println!("(dry-run: nothing written)");
                return Ok(());
            }
            if (plan.first_push || *move_files) && !yes {
                let question = if *move_files {
                    format!(
                        "Import {} note(s) into index {} and DELETE the local copies?",
                        plan.items.len(),
                        mount.index()
                    )
                } else {
                    format!(
                        "Import {} note(s) into index {}?",
                        plan.items.len(),
                        mount.index()
                    )
                };
                if !confirm(&question)? {
                    println!("aborted");
                    return Ok(());
                }
            }
            if plan.changed_count() > 0 {
                let report =
                    apply_seed(&resolved.service.vault_path, &mount, &normalized, &plan).await?;
                println!(
                    "seeded {} note(s), {} already up to date",
                    report.seeded, report.unchanged
                );
            } else {
                println!("index already up to date");
            }
            if *move_files {
                let (deleted, skipped) =
                    remove_seeded_local_files(&resolved.service.vault_path, &mount, &normalized)
                        .await?;
                for path in &deleted {
                    println!("  removed local  {path}");
                }
                for path in &skipped {
                    println!("  kept (drifted since import)  {path}");
                }
                println!(
                    "{} local file(s) removed; the index now holds the only copy",
                    deleted.len()
                );
            }
            println!(
                "author the wiki through the mount: {} (back it up with `share dump`)",
                mount.mount_at()
            );
            Ok(())
        }
        ShareAction::Dump { to, index } => {
            let config = select_mount_config(resolved, index.as_deref())?;
            let mount = connect(resolved, &config)?;
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
            if dry_run {
                println!(
                    "(dry-run: would dump index {} to {})",
                    mount.index(),
                    target.display()
                );
                return Ok(());
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
        ShareAction::Status => {
            if resolved.service.shared.is_empty() {
                println!("no shared mounts configured");
                return Ok(());
            }
            for config in &resolved.service.shared {
                let mount = connect(resolved, config)?;
                let notes = mount
                    .client
                    .browse_all(mount.index(), Some("recordType:note"))
                    .await?;
                let history = mount
                    .client
                    .browse_all(&mount.history_index, Some("recordType:note"))
                    .await
                    .map(|records| records.len())
                    .unwrap_or(0);
                let (cache_entries, cache_bytes) = mount.cache.stats();
                println!(
                    "mount {} (app {}, mounted at {}{})",
                    mount.index(),
                    mount.config.app_id,
                    mount.mount_at(),
                    if mount.config.writable {
                        ""
                    } else {
                        ", read-only"
                    }
                );
                println!("  notes: {}", notes.len());
                println!("  superseded versions in history: {history}");
                println!("  local cache: {cache_entries} note(s), {cache_bytes} bytes");
                for path in notes
                    .iter()
                    .filter(|record| {
                        record
                            .get("hasDivergence")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false)
                    })
                    .filter_map(|record| record.get("path").and_then(serde_json::Value::as_str))
                {
                    println!("  diverged: {path}  (resolve_divergence to reconcile)");
                }
            }
            Ok(())
        }
        ShareAction::Retract { path, index, yes } => {
            let config = select_mount_config(resolved, index.as_deref())?;
            let mount = connect(resolved, &config)?;
            let remote = to_remote_path(&mount, path);
            let head = versioning::fetch_head(&mount, &remote)
                .await?
                .ok_or_else(|| anyhow!("note not found in index {}: {remote}", mount.index()))?;
            println!(
                "retract {} from index {} (head {} by {})",
                remote,
                mount.index(),
                head.version_id,
                head.participant_id
            );
            if dry_run {
                println!("(dry-run: nothing removed)");
                return Ok(());
            }
            if !yes
                && !confirm(
                    "This deletes the note AND its entire version history, permanently. Proceed?",
                )?
            {
                println!("aborted");
                return Ok(());
            }
            versioning::retract_note(&mount, &remote).await?;
            println!("retracted {remote} (note, chunks, and history removed)");
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
            // failure) must fail HERE, not at the next mount connection.
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
                .ok_or_else(|| {
                    anyhow!("config file not found: {}", resolved.config_path.display())
                })?;
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
        ShareAction::Key {
            index,
            filters,
            parent_key,
        } => {
            let config = select_mount_config(resolved, index.as_deref())?;
            let secrets = SecretResolver::new();
            // A secured key INHERITS its parent's ACLs; the `filters`
            // restriction constrains search only. Deriving from the mount's
            // write key therefore produces a key that reads a narrow slice but
            // can write ANYWHERE in the index — verified against a live
            // account. So the parent must be search-only, and we check it
            // rather than trust it.
            let parent = match parent_key.as_deref() {
                Some(explicit) => explicit.to_string(),
                None => deep_obsidian_server::shared::resolve_api_key(&config, &secrets)
                    .map_err(|error| anyhow!("{error}"))?,
            };
            let probe = deep_obsidian_algolia::AlgoliaClient::new(
                &config.app_id,
                &parent,
                config.base_url.as_deref(),
            );
            let acls = probe
                .key_acls(&parent)
                .await
                .map_err(|error| anyhow!("cannot inspect the parent key's ACLs: {error}"))?;
            let write_acls: Vec<&String> = acls
                .iter()
                .filter(|acl| deep_obsidian_algolia::WRITE_ACLS.contains(&acl.as_str()))
                .collect();
            if !write_acls.is_empty() {
                return Err(anyhow!(
                    "refusing to derive a teammate key from a parent that can write \
(ACLs: {write_acls:?}).\n\nA secured key inherits its parent's ACLs and its `filters` \
restriction applies to SEARCH ONLY — the result would read a narrow slice while writing \
anywhere in the index.\n\nCreate a search-only key for this index (Algolia dashboard \
> API Keys > New, ACL: search only), then:\n  deep-obsidian-mcp share key --parent-key \
<that-key>{}",
                    filters
                        .as_deref()
                        .map(|f| format!(" --filters '{f}'"))
                        .unwrap_or_default()
                ));
            }
            if !acls.iter().any(|acl| acl == "search") {
                return Err(anyhow!(
                    "the parent key cannot search (ACLs: {acls:?}); a teammate key derived \
from it would be useless"
                ));
            }
            // `browse` is a separate ACL from `search`, and several mount reads
            // need it (listing the mount root, note_history, dump). Without it
            // the teammate gets a bare 403 from those, so say so up front
            // rather than letting them discover it.
            if !acls.iter().any(|acl| acl == "browse") {
                println!(
                    "WARNING: the parent key lacks the `browse` ACL (has: {acls:?}).\n\
Reads that enumerate exhaustively will fail with 403 for the teammate:\n\
  - list_children on the mount ROOT (subfolders of a named folder still work)\n\
  - note_history, and `share dump`\n\
Add `browse` to the key for a fully usable read-only mount.\n"
                );
            }
            let restrictions = filters
                .as_deref()
                .map(|filters| format!("filters={}", urlencoding_encode(filters)))
                .unwrap_or_default();
            let key = deep_obsidian_algolia::generate_secured_api_key(&parent, &restrictions);
            println!(
                "secured key (parent ACLs verified search-only{}):",
                filters
                    .as_deref()
                    .map(|filters| format!(", scoped to `{filters}`"))
                    .unwrap_or_default()
            );
            println!("{key}");
            println!();
            println!("The teammate supplies it via DEEP_OBSIDIAN_ALGOLIA_API_KEY, or");
            println!("`share set-key` on their side. Writes will be refused by Algolia.");
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
