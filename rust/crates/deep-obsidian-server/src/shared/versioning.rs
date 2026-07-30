//! Append-only versioned writes (design §3/§5).
//!
//! Cutover order is load-bearing:
//!   1. read the current head (`vPrev`);
//!   2. push the new version's chunks to the main index;
//!   3. copy `vPrev`'s note + chunks into the history index (before deleting);
//!   4. delete `vPrev`'s chunks from main by EXPLICIT `versionId:vPrev` —
//!      never `NOT versionId:vNew`, which would destroy a concurrent writer's
//!      chunks;
//!   5. overwrite the note record (head pointer).
//! Then retention purge (§3.1) runs on this note's history.

use super::records_build::{build_note_records, NoteVersionMeta};
use super::{new_version_id, now_ms, retention_keep_set, Result, SharedError, SharedMountRuntime};
use deep_obsidian_algolia::records::NoteRecord;
use serde_json::{json, Value};

#[derive(Debug)]
pub struct VersionedWriteOutcome {
    pub version_id: String,
    pub parent_version_id: Option<String>,
    pub forked_from: Option<String>,
    pub has_divergence: bool,
    pub created: bool,
}

/// Fetches the head note record for a remote path, if any.
pub async fn fetch_head(
    mount: &SharedMountRuntime,
    remote_path: &str,
) -> Result<Option<NoteRecord>> {
    let ids = vec![deep_obsidian_algolia::note_object_id(remote_path)];
    let raw = mount.client.get_objects(mount.index(), &ids).await;
    // Either "no index yet" or "this key may not see that object" means the
    // same thing to a reader: there is no such note here.
    //
    // The second case matters for security: Algolia answers 403 `objectID not
    // allowed` when a secured key's filter restriction excludes the object.
    // Surfacing that verbatim let a scoped teammate tell "exists but hidden"
    // from "does not exist" and so enumerate paths outside their scope. An
    // outright invalid key reports a different message and still errors.
    let mut results = match raw {
        Err(error) if error.is_forbidden_by_key_scope() => Vec::new(),
        other => super::empty_if_missing_index(other, Vec::new())?,
    };
    Ok(results
        .pop()
        .flatten()
        .and_then(|value| serde_json::from_value(value).ok()))
}

/// Writes one new version of `remote_path` with content `content`.
///
/// `base_version_id` is the version the writer based their edit on (their
/// hydrated copy). When it is set and differs from the current head, the new
/// version is flagged as a fork and the note gains `hasDivergence: true` —
/// divergence is recorded, never blocked.
pub async fn push_note_version(
    mount: &SharedMountRuntime,
    remote_path: &str,
    content: &str,
    known_files: &[String],
    base_version_id: Option<&str>,
    resolve_divergence: bool,
) -> Result<VersionedWriteOutcome> {
    let head = fetch_head(mount, remote_path).await?;
    let head_version = head.as_ref().map(|note| note.version_id.clone());
    let participant_id = mount.participant_id();
    let updated_at_ms = now_ms();
    let version_id = new_version_id(&participant_id);

    // Unchanged content: skip the write entirely (idempotent push).
    if let Some(head_note) = &head {
        if head_note.content_hash == crate::tools::content_hash(content.as_bytes()) {
            return Ok(VersionedWriteOutcome {
                version_id: head_note.version_id.clone(),
                parent_version_id: head_note.parent_version_id.clone(),
                forked_from: None,
                has_divergence: head_note.has_divergence,
                created: false,
            });
        }
    }

    let forked_from = match (&head_version, base_version_id) {
        (Some(head_id), Some(base)) if head_id != base => Some(head_id.clone()),
        _ => None,
    };
    // Divergence persists until a writer explicitly resolves it: a head-based
    // write still hasn't merged the forked content sitting in history.
    let has_divergence = if resolve_divergence && forked_from.is_none() {
        false
    } else {
        forked_from.is_some() || head.as_ref().map(|note| note.has_divergence).unwrap_or(false)
    };

    let meta = NoteVersionMeta {
        version_id: version_id.clone(),
        parent_version_id: base_version_id.map(str::to_string).or(head_version.clone()),
        forked_from: forked_from.clone(),
        has_divergence,
        participant_id: participant_id.clone(),
        updated_at_ms,
    };
    let built = build_note_records(remote_path, content, known_files, &meta);

    // (2) new chunks into main.
    let chunk_values: Vec<Value> = built
        .chunks
        .iter()
        .map(|chunk| serde_json::to_value(chunk).expect("chunk serializes"))
        .collect();
    if !chunk_values.is_empty() {
        mount.client.save_objects(mount.index(), chunk_values).await?;
    }

    // (3) copy the superseded version into history BEFORE deleting from main,
    // so a crash between the two leaves a duplicate rather than a loss.
    if let (Some(prev_note), Some(prev_version)) = (&head, &head_version) {
        let prev_chunks = super::empty_if_missing_index(
            mount
                .client
                .browse_all(
                    mount.index(),
                    Some(&format!(
                        "recordType:chunk AND noteId:\"{remote_path}\" AND versionId:\"{prev_version}\""
                    )),
                )
                .await,
            Vec::new(),
        )?;
        let mut history_records: Vec<Value> = prev_chunks;
        let mut prev_note_value = serde_json::to_value(prev_note).expect("note serializes");
        // History note records get a version-scoped objectID so versions coexist.
        prev_note_value["objectID"] =
            json!(format!("note:{remote_path}@{prev_version}"));
        prev_note_value["supersededBy"] = json!(version_id.clone());
        history_records.push(prev_note_value);
        mount
            .client
            .save_objects(&mount.history_index, history_records)
            .await?;
        // The write above brought the history index into existence, so its
        // settings can finally be applied.
        super::ensure_index_settings(
            &mount.client,
            &mount.history_index,
            &mount.history_provisioned,
            deep_obsidian_algolia::records::history_index_settings(),
        )
        .await;

        // (4) delete the superseded chunks from main — explicit vPrev filter.
        super::empty_if_missing_index(
            mount
                .client
                .delete_by_query(
                    mount.index(),
                    &format!(
                        "recordType:chunk AND noteId:\"{remote_path}\" AND versionId:\"{prev_version}\""
                    ),
                )
                .await,
            Value::Null,
        )?;
    }

    // (5) head pointer update — awaited, so the note is immediately readable.
    // Algolia writes are async; without this the "write then read back to
    // verify" pattern every capture/maintenance flow ends with would fail.
    // Tasks on one index are processed in order, so awaiting this last write
    // also guarantees the chunks from step (2) are queryable.
    let note_value = serde_json::to_value(&built.note).expect("note serializes");
    mount
        .client
        .save_objects_awaited(mount.index(), vec![note_value])
        .await?;
    // The main index may have been created by this very write (mount-only
    // authorship never runs `seed`), so provision its settings now.
    super::ensure_index_settings(
        &mount.client,
        mount.index(),
        &mount.main_provisioned,
        deep_obsidian_algolia::records::main_index_settings(),
    )
    .await;

    // Retention purge (§3.1) on this note's history.
    purge_history(mount, remote_path).await?;

    Ok(VersionedWriteOutcome {
        version_id,
        parent_version_id: meta.parent_version_id,
        forked_from,
        has_divergence,
        created: head.is_none(),
    })
}

/// Applies the floor+ceiling retention rule to one note's history records.
async fn purge_history(mount: &SharedMountRuntime, remote_path: &str) -> Result<()> {
    let (min_versions, max_age_days) = mount.retention();
    let history_notes = super::empty_if_missing_index(
        mount
            .client
            .browse_all(
                &mount.history_index,
                Some(&format!("recordType:note AND noteId:\"{remote_path}\"")),
            )
            .await,
        Vec::new(),
    )?;
    let versions: Vec<(String, u64)> = history_notes
        .iter()
        .filter_map(|record| {
            Some((
                record.get("versionId")?.as_str()?.to_string(),
                record.get("updatedAtMs").and_then(Value::as_u64).unwrap_or(0),
            ))
        })
        .collect();
    let keep = retention_keep_set(&versions, min_versions, max_age_days, now_ms());
    for (version_id, _) in versions {
        if !keep.contains(&version_id) {
            super::empty_if_missing_index(
                mount
                    .client
                    .delete_by_query(
                        &mount.history_index,
                        &format!("noteId:\"{remote_path}\" AND versionId:\"{version_id}\""),
                    )
                    .await,
                Value::Null,
            )?;
        }
    }
    Ok(())
}

/// Retract a note entirely: delete head + chunks from main and purge ALL its
/// history (design §8 — retraction is the deliberate exception to
/// non-destruction; without it a mistaken push could never be withdrawn).
pub async fn retract_note(mount: &SharedMountRuntime, remote_path: &str) -> Result<()> {
    super::empty_if_missing_index(
        mount
            .client
            .delete_by_query(mount.index(), &format!("noteId:\"{remote_path}\""))
            .await,
        Value::Null,
    )?;
    super::empty_if_missing_index(
        mount
            .client
            .delete_by_query(&mount.history_index, &format!("noteId:\"{remote_path}\""))
            .await,
        Value::Null,
    )?;
    Ok(())
}

/// Soft-deletes a note: the head becomes a `deleted: true` tombstone, its
/// chunks leave the main index (so search and hydration stop finding it), and
/// the previous version moves to history — so the content is still recoverable
/// via `note_history` / `read_version`, and other participants observe the
/// removal instead of silently missing a record.
///
/// This is the ordinary "delete a note" operation. `retract_note` is the
/// separate, permanent purge that also removes the tombstone and the history.
pub async fn soft_delete_note(
    mount: &SharedMountRuntime,
    remote_path: &str,
) -> Result<SoftDeleteOutcome> {
    let head = fetch_head(mount, remote_path)
        .await?
        .ok_or_else(|| SharedError::NoteNotFound(mount.mounted_path(remote_path)))?;
    if head.deleted {
        return Ok(SoftDeleteOutcome {
            version_id: head.version_id,
            already_deleted: true,
            recoverable_from: head.parent_version_id,
        });
    }

    let participant_id = mount.participant_id();
    let version_id = new_version_id(&participant_id);
    let prev_version = head.version_id.clone();

    // Same ordering discipline as a normal write: copy to history BEFORE
    // removing from main, and delete by an EXPLICIT versionId so a concurrent
    // writer's chunks are never in the delete set.
    let prev_chunks = super::empty_if_missing_index(
        mount
            .client
            .browse_all(
                mount.index(),
                Some(&format!(
                    "recordType:chunk AND noteId:\"{remote_path}\" AND versionId:\"{prev_version}\""
                )),
            )
            .await,
        Vec::new(),
    )?;
    let mut history_records: Vec<Value> = prev_chunks;
    let mut prev_note_value = serde_json::to_value(&head).expect("note serializes");
    prev_note_value["objectID"] = json!(format!("note:{remote_path}@{prev_version}"));
    prev_note_value["supersededBy"] = json!(version_id.clone());
    history_records.push(prev_note_value);
    mount
        .client
        .save_objects(&mount.history_index, history_records)
        .await?;
    super::ensure_index_settings(
        &mount.client,
        &mount.history_index,
        &mount.history_provisioned,
        deep_obsidian_algolia::records::history_index_settings(),
    )
    .await;

    super::empty_if_missing_index(
        mount
            .client
            .delete_by_query(
                mount.index(),
                &format!(
                    "recordType:chunk AND noteId:\"{remote_path}\" AND versionId:\"{prev_version}\""
                ),
            )
            .await,
        Value::Null,
    )?;

    // The tombstone keeps the note's identity and metadata but carries no body.
    let mut tombstone = head.clone();
    tombstone.version_id = version_id.clone();
    tombstone.parent_version_id = Some(prev_version.clone());
    tombstone.deleted = true;
    tombstone.chunk_count = 0;
    tombstone.size_bytes = 0;
    tombstone.content_hash = crate::tools::content_hash(b"");
    tombstone.updated_at_ms = now_ms();
    tombstone.participant_id = participant_id;
    tombstone.superseded_by = None;
    tombstone.forked_from = None;
    mount
        .client
        .save_objects_awaited(
            mount.index(),
            vec![serde_json::to_value(&tombstone).expect("tombstone serializes")],
        )
        .await?;

    mount.cache.remove(remote_path);
    purge_history(mount, remote_path).await?;

    Ok(SoftDeleteOutcome {
        version_id,
        already_deleted: false,
        recoverable_from: Some(prev_version),
    })
}

#[derive(Debug)]
pub struct SoftDeleteOutcome {
    pub version_id: String,
    pub already_deleted: bool,
    /// The version holding the content just removed — feed it to
    /// `read_version` to recover, or `upsert_note` it back to undelete.
    pub recoverable_from: Option<String>,
}
