//! Append-only versioned writes, and the cutover whose ORDER is the design.
//!
//! Ported from PR #40's `shared/versioning.rs`. The cutover is unchanged and must
//! stay so:
//!
//! 1. read the current head (`vPrev`);
//! 2. push the new version's chunks to the main index;
//! 3. copy `vPrev`'s note + chunks into the history index — **before** deleting
//!    anything, so a crash between the two duplicates rather than loses;
//! 4. delete `vPrev`'s chunks from main by an **explicit** `noteId:X AND
//!    versionId:vPrev` filter. Never `NOT versionId:vNew`: two participants writing
//!    the same note at the same time would each delete the other's freshly-pushed
//!    chunks, and the note would end up with a head pointing at chunks that no
//!    longer exist;
//! 5. overwrite the note record — the head pointer — and AWAIT it, because Algolia
//!    writes are asynchronous and every capture flow ends by reading back what it
//!    just wrote.
//!
//! Then the retention purge runs over this note's history.
//!
//! # What changed from #40
//!
//! `push_note_version` took `base_version_id: Option<&str>`, which collapses two
//! distinct facts. This slice threads [`BaseVersion`] itself: `Absent` means the
//! caller READ the destination and found nothing, and a head existing anyway is a
//! divergence just as surely as a stale version id is. Under #40's signature that
//! case passed `None` and the fork check `(Some(head), Some(base)) if head != base`
//! never fired, so a concurrent create was silently overwritten with no divergence
//! recorded. See [`fork_of`].

use deep_obsidian_algolia::records::NoteRecord;
use serde_json::{json, Value};
use tracing::{debug, warn};

use super::records_build::{build_note_records, NoteVersionMeta};
use super::{
    empty_if_missing_index, new_version_id, now_ms, retention_keep_set, AlgoliaVaultBackend,
};
use crate::{BackendError, BaseVersion, SoftDeleteOutcome};

#[derive(Debug)]
pub struct VersionedWriteOutcome {
    pub version_id: String,
    pub parent_version_id: Option<String>,
    /// The head this version forked away from, when it did. `Some` means the write
    /// landed as a branch rather than as a continuation.
    pub forked_from: Option<String>,
    pub has_divergence: bool,
    pub created: bool,
    /// True when the content already matched the head, so nothing was pushed.
    pub unchanged: bool,
}

/// The head note record for a path, if any.
///
/// Two distinct failures are folded into `Ok(None)` on purpose:
///
/// * **no index yet** — an Algolia index springs into existence on its first write,
///   so every read against a never-written corpus answers 404. That means "no
///   records", not a failure;
/// * **403 `objectID not allowed`** — a SECURED key whose filter restriction
///   excludes this object. Surfacing that verbatim would let a scoped participant
///   tell "exists but hidden" from "does not exist" and so enumerate paths outside
///   their scope. An outright invalid key reports a different message and still
///   errors, so this does not hide a broken credential.
pub async fn fetch_head(
    backend: &AlgoliaVaultBackend,
    remote_path: &str,
) -> Result<Option<NoteRecord>, BackendError> {
    let ids = vec![deep_obsidian_algolia::note_object_id(remote_path)];
    let raw = backend.client().get_objects(backend.index(), &ids).await;
    let mut results = match raw {
        Err(error) if error.is_forbidden_by_key_scope() => {
            debug!(
                "the Algolia key for index '{}' may not address {remote_path}; reporting it as \
                 absent rather than as hidden, so a scoped key cannot be used to enumerate paths \
                 outside its scope",
                backend.index()
            );
            Vec::new()
        }
        other => empty_if_missing_index(other, Vec::new())?,
    };
    Ok(results
        .pop()
        .flatten()
        .and_then(|value| serde_json::from_value(value).ok()))
}

/// The head this write forks away from, given what the caller observed.
///
/// The whole guarded-write semantic in one function, and the one place it differs
/// from CouchDB's:
///
/// * `Version(v)` and the head is no longer `v` → a fork. Someone wrote between the
///   caller's read and this write. **Including when the head is now a TOMBSTONE**:
///   somebody deleted the note after the caller read it, and overwriting the deletion
///   without recording anything would make the delete vanish with no trace of the
///   disagreement. The tombstone's version is what this forked away from;
/// * `Absent` and a LIVE head exists → also a fork. The caller looked, saw nothing,
///   and composed content on that basis; a head that appeared since is exactly the
///   concurrent create the three-variant [`BaseVersion`] exists to catch;
/// * `Absent` and the head is a TOMBSTONE → **not** a fork. This is the ordinary
///   resurrection of a soft-deleted note: reads report a tombstone as absent, so the
///   caller's observation was CORRECT, and the write continues the version chain
///   rather than branching off it. Without this arm every undelete would be marked
///   `hasDivergence` and land in `conflicted_paths()`, which would make that list
///   useless — it would be dominated by notes nobody disagreed about;
/// * `Unobserved` → no fork can be asserted. The caller established no precondition,
///   so claiming a divergence would invent one.
fn fork_of(base_version: &BaseVersion, head: Option<&NoteRecord>) -> Option<String> {
    let head = head?;
    let head_version = head.version_id.clone();
    match base_version {
        BaseVersion::Version(base) if *base != head_version => Some(head_version),
        BaseVersion::Version(_) => None,
        BaseVersion::Absent if head.deleted => None,
        BaseVersion::Absent => Some(head_version),
        BaseVersion::Unobserved => None,
    }
}

/// Whether `has_divergence` should survive this write.
///
/// Divergence is STICKY by default: a later head-based write has still not merged the
/// forked content sitting in history, so the flag outlives the write that recorded it.
/// Exactly one thing clears it — a caller stating that the content it is writing IS the
/// reconciliation (`resolve_divergence`) — and even then only when this write does not
/// itself fork, because a write that forked has created a NEW divergence and clearing
/// the flag would erase the fact one call after establishing it.
///
/// The claim can only come from the caller. The storage sees an overwrite either way and
/// has no way to tell a merge from a fresh clobber, which is precisely why the server
/// never merges on its own.
fn divergence_after_write(
    resolve_divergence: bool,
    forked_from: Option<&str>,
    head: Option<&NoteRecord>,
) -> bool {
    if forked_from.is_some() {
        return true;
    }
    if resolve_divergence {
        return false;
    }
    head.map(|note| note.has_divergence).unwrap_or(false)
}

/// Write one new version of `remote_path`.
///
/// # Why a stale base does NOT fail here
///
/// A CouchDB mount reports [`BackendError::VersionConflict`] and writes nothing,
/// because CouchDB's compare-and-swap can refuse and the losing writer's content has
/// somewhere to go (their own device, still holding it). A shared Algolia corpus is
/// the opposite situation on both counts: the storage has no CAS, and — under
/// mount-only authorship — the index is the ONLY copy of what the writer composed.
/// Refusing the write would discard it.
///
/// So divergence is RECORDED rather than blocked: the write lands, `forkedFrom`
/// names the head it branched from, `hasDivergence` marks the note, and the
/// superseded version is in history where a merge can find it. Nothing is lost and
/// the disagreement is visible — which is the property that matters when several
/// participants share one corpus.
///
/// # How this composes with the MCP `expectedHash` guard above it
///
/// The tool layer reads the note, hashes the body, compares that to the caller's
/// `expectedHash`, and only then composes `content`. A caller working from a stale
/// copy is therefore rejected ABOVE this boundary, with the frozen hash-conflict
/// wording — it never reaches the fork path. What is left for the fork path is the
/// window between that comparison and this write: a true TOCTOU race, where the
/// caller's precondition WAS satisfied and another participant landed a version in
/// the microseconds since. That is the one case a fork is the right answer to, and
/// it is why the two mechanisms are not redundant.
pub async fn push_note_version(
    backend: &AlgoliaVaultBackend,
    remote_path: &str,
    content: &str,
    known_files: &[String],
    base_version: &BaseVersion,
    resolve_divergence: bool,
) -> Result<VersionedWriteOutcome, BackendError> {
    let head = fetch_head(backend, remote_path).await?;
    let head_version = head.as_ref().map(|note| note.version_id.clone());
    let participant_id = backend.participant_id().to_string();
    let updated_at_ms = now_ms();
    let version_id = new_version_id(&participant_id);

    // Identical content: no new version at all (an idempotent push).
    //
    // Checked BEFORE the fork check on purpose: if the bytes already match the head,
    // there is nothing to diverge about — two participants arriving at the same text
    // have not disagreed. Two exceptions:
    //
    // * a TOMBSTONE — its hash is the hash of an empty body, so an empty write over one
    //   must still resurrect the note rather than be short-circuited into "already up
    //   to date";
    // * a resolve-divergence write over a DIVERGED head. A merge whose result equals
    //   the current head is a perfectly ordinary outcome (the overtaken version added
    //   nothing that survived), and short-circuiting it would leave the note marked
    //   diverged with no write that could ever clear the mark — the flag would be
    //   permanently stuck for exactly the notes a caller had already reconciled. So
    //   this falls through to a real write whose only effect is clearing the flag.
    //   PR #40 short-circuited here and had that trap.
    if let Some(head_note) = &head {
        let clearing_divergence = resolve_divergence && head_note.has_divergence;
        if !head_note.deleted
            && !clearing_divergence
            && head_note.content_hash == deep_obsidian_core::content_hash(content.as_bytes())
        {
            return Ok(VersionedWriteOutcome {
                version_id: head_note.version_id.clone(),
                parent_version_id: head_note.parent_version_id.clone(),
                forked_from: None,
                has_divergence: head_note.has_divergence,
                created: false,
                unchanged: true,
            });
        }
    }

    let forked_from = fork_of(base_version, head.as_ref());
    let has_divergence =
        divergence_after_write(resolve_divergence, forked_from.as_deref(), head.as_ref());
    if resolve_divergence && forked_from.is_some() {
        warn!(
            "the write of {remote_path} on Algolia index '{}' asked to resolve the recorded \
             divergence, but it forked off the head itself, so the note stays marked \
             hasDivergence: resolving would have cleared a divergence this very write created. \
             Re-read the note, merge again from the CURRENT head, and write once more.",
            backend.index()
        );
    }
    if let Some(forked) = &forked_from {
        warn!(
            "write of {remote_path} on Algolia index '{}' was based on {} but the head had already \
             moved to {forked}; the write landed as a FORK (forkedFrom={forked}) and the note is \
             marked hasDivergence — nothing was lost, and the superseded version is in the history \
             index",
            backend.index(),
            match base_version {
                BaseVersion::Version(base) => format!("version {base}"),
                BaseVersion::Absent => "an absent note".to_string(),
                BaseVersion::Unobserved => "no observed version".to_string(),
            }
        );
    }

    let meta = NoteVersionMeta {
        version_id: version_id.clone(),
        // The version this one continues: what the writer based on when they
        // established it, and otherwise the head it superseded. For a fork both are
        // recorded — `parentVersionId` is where the content came from,
        // `forkedFrom` is what it displaced.
        parent_version_id: base_version
            .as_version()
            .map(str::to_string)
            .or_else(|| head_version.clone()),
        forked_from: forked_from.clone(),
        has_divergence,
        participant_id: participant_id.clone(),
        updated_at_ms,
    };
    let built = build_note_records(remote_path, content, known_files, &meta);

    // (2) The new version's chunks go into main FIRST, so the head never points at
    // chunks that are not there yet.
    let chunk_values: Vec<Value> = built
        .chunks
        .iter()
        .map(|chunk| serde_json::to_value(chunk).expect("a chunk record serializes"))
        .collect();
    if !chunk_values.is_empty() {
        super::map_algolia(
            backend
                .client()
                .save_objects(backend.index(), chunk_values)
                .await,
        )?;
    }

    // (3) + (4): copy the superseded version to history BEFORE deleting it from main,
    // and delete by an EXPLICIT vPrev filter. Shared with the soft delete so the two
    // cannot drift — see `archive_version`.
    if let Some(prev_note) = &head {
        archive_version(backend, remote_path, prev_note, &version_id).await?;
    }

    // (5) The head pointer, AWAITED. Algolia writes are asynchronous; without the
    // wait the "write, then read back to verify" pattern every capture and
    // maintenance flow ends with would fail against a real account. Tasks on one
    // index are processed in order, so awaiting this last write also guarantees the
    // chunks from step (2) are queryable.
    let note_value = serde_json::to_value(&built.note).expect("a note record serializes");
    super::map_algolia(
        backend
            .client()
            .save_objects_awaited(backend.index(), vec![note_value])
            .await,
    )?;
    // The main index may have been created by this very write, so provision its
    // settings now — nothing else does, and without them faceting, `distinct` and
    // the searchable attributes are all wrong (a facet query fails outright).
    backend.ensure_main_settings().await;

    // The cache is updated only AFTER the push succeeded: it is a read cache, never
    // a write buffer, so a failed push must leave no trace of content the corpus
    // does not hold.
    backend
        .cache()
        .put(remote_path, &version_id, &built.note.content_hash, content);

    purge_history(backend, remote_path).await?;

    Ok(VersionedWriteOutcome {
        version_id,
        parent_version_id: meta.parent_version_id,
        forked_from,
        has_divergence,
        created: head.is_none() || head.as_ref().is_some_and(|note| note.deleted),
        unchanged: false,
    })
}

/// Copy one version of a note out of the main index and into history, then remove that
/// version's chunks from main.
///
/// Extracted because a normal write and a soft delete need EXACTLY this, in exactly this
/// order, and two copies of it would be two chances to get the order wrong. The order is
/// the whole design: the copy happens BEFORE the delete, so a crash between them
/// duplicates rather than loses; and the delete names `versionId` explicitly, never
/// `NOT versionId:<new>`, because two participants writing the same note at once would
/// each delete the other's freshly-pushed chunks under the inverse filter.
///
/// `superseded_by` is stamped onto the archived note record so history is walkable
/// forwards, and the archived record gets a VERSION-SCOPED objectID so several versions
/// of one note coexist there — only the main index's note record is a stable head
/// pointer.
async fn archive_version(
    backend: &AlgoliaVaultBackend,
    remote_path: &str,
    previous: &NoteRecord,
    superseded_by: &str,
) -> Result<(), BackendError> {
    let previous_version = previous.version_id.clone();
    let chunk_filter = format!(
        "recordType:chunk AND noteId:{} AND versionId:{}",
        super::quote_filter_value(remote_path),
        super::quote_filter_value(&previous_version)
    );
    let previous_chunks = empty_if_missing_index(
        backend
            .client()
            .browse_all(backend.index(), Some(&chunk_filter))
            .await,
        Vec::new(),
    )?;
    let mut history_records: Vec<Value> = previous_chunks;
    let mut previous_note = serde_json::to_value(previous).expect("a note record serializes");
    previous_note["objectID"] = json!(format!("note:{remote_path}@{previous_version}"));
    previous_note["supersededBy"] = json!(superseded_by);
    history_records.push(previous_note);
    super::map_algolia(
        backend
            .client()
            .save_objects(backend.history_index(), history_records)
            .await,
    )?;
    // That write is what brought the history index into existence, so its settings can
    // finally be applied.
    backend.ensure_history_settings().await;

    empty_if_missing_index(
        backend
            .client()
            .delete_by_query(backend.index(), &chunk_filter)
            .await,
        Value::Null,
    )?;
    Ok(())
}

/// Remove a note by replacing its head with a TOMBSTONE.
///
/// Ported from PR #40. Three properties, all load-bearing, and the reason this is a soft
/// delete rather than a purge:
///
/// * the note is GONE from every read and listing — every one of them filters
///   `NOT deleted:true` ([`super::reads::LIVE_NOTES`]) and the chunks leave the main
///   index, so search cannot match it either;
/// * other participants can tell it was REMOVED rather than merely find it missing,
///   because the record is still there. On a shared corpus that distinction is the
///   difference between "someone deleted this" and "did my sync break?";
/// * the content is still RECOVERABLE: the superseded version is in history, so
///   `recoverable_from` names a version a versioned read can still serve, and writing
///   it back resurrects the note (which the fork logic deliberately does not treat as a
///   divergence — see [`fork_of`]).
///
/// Deleting an already-deleted note is a successful no-op rather than an error: the
/// caller's intent is already satisfied, and failing would make the operation
/// non-idempotent for nothing.
pub async fn soft_delete_note(
    backend: &AlgoliaVaultBackend,
    remote_path: &str,
) -> Result<SoftDeleteOutcome, BackendError> {
    let head = fetch_head(backend, remote_path)
        .await?
        .ok_or_else(|| super::note_not_found(remote_path))?;
    if head.deleted {
        return Ok(SoftDeleteOutcome {
            version_id: head.version_id,
            already_deleted: true,
            recoverable_from: head.parent_version_id,
        });
    }

    let participant_id = backend.participant_id().to_string();
    let version_id = new_version_id(&participant_id);
    let previous_version = head.version_id.clone();

    archive_version(backend, remote_path, &head, &version_id).await?;

    // The tombstone keeps the note's IDENTITY and its folder facets — so a reader who
    // asks for it is told the note is gone rather than told nothing — and carries no
    // body at all.
    let mut tombstone = head.clone();
    tombstone.version_id = version_id.clone();
    tombstone.parent_version_id = Some(previous_version.clone());
    tombstone.deleted = true;
    tombstone.chunk_count = 0;
    tombstone.size_bytes = 0;
    tombstone.content_hash = deep_obsidian_core::content_hash(b"");
    tombstone.updated_at_ms = now_ms();
    tombstone.participant_id = participant_id;
    tombstone.superseded_by = None;
    // A tombstone forks off nothing. Carrying the head's `forkedFrom` forward would
    // make a delete look like the fork that preceded it.
    tombstone.forked_from = None;
    // AWAITED, like every head-pointer move: a caller that deletes and then lists must
    // not still see the note.
    super::map_algolia(
        backend
            .client()
            .save_objects_awaited(
                backend.index(),
                vec![serde_json::to_value(&tombstone).expect("a tombstone serializes")],
            )
            .await,
    )?;

    // The cache is a READ cache of live content, so a removed note must leave it — or
    // the next read on this process would serve the body from a note that no longer
    // exists.
    backend.cache().remove(remote_path);
    purge_history(backend, remote_path).await?;

    Ok(SoftDeleteOutcome {
        version_id,
        already_deleted: false,
        recoverable_from: Some(previous_version),
    })
}

/// Every retained version of a note, newest first.
///
/// Assembled from two indexes because that is where the two halves live: the head is the
/// main index's note record, every superseded version is a history record. A note nobody
/// has ever superseded therefore has a one-entry history and no history index at all,
/// which [`empty_if_missing_index`] turns into an empty list rather than a 404.
///
/// A TOMBSTONE is included and is `current`. Reporting it as absent would make a deleted
/// note's history unreadable, which is precisely the recovery path the soft delete exists
/// to preserve.
pub async fn note_history(
    backend: &AlgoliaVaultBackend,
    remote_path: &str,
) -> Result<crate::NoteHistory, BackendError> {
    let head = fetch_head(backend, remote_path)
        .await?
        .ok_or_else(|| super::note_not_found(remote_path))?;
    let archived = empty_if_missing_index(
        backend
            .client()
            .browse_all(
                backend.history_index(),
                Some(&format!(
                    "recordType:note AND noteId:{}",
                    super::quote_filter_value(remote_path)
                )),
            )
            .await,
        Vec::new(),
    )?;

    let mut versions: Vec<crate::NoteVersion> = archived
        .iter()
        .filter_map(|record| {
            Some(crate::NoteVersion {
                version_id: record.get("versionId")?.as_str()?.to_string(),
                participant_id: record
                    .get("participantId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                updated_at_ms: record
                    .get("updatedAtMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                parent_version_id: optional_string(record, "parentVersionId"),
                forked_from: optional_string(record, "forkedFrom"),
                superseded_by: optional_string(record, "supersededBy"),
                current: false,
            })
        })
        .collect();
    versions.push(crate::NoteVersion {
        version_id: head.version_id.clone(),
        participant_id: head.participant_id.clone(),
        updated_at_ms: head.updated_at_ms,
        parent_version_id: head.parent_version_id.clone(),
        forked_from: head.forked_from.clone(),
        // The head is by definition not superseded. It is stated rather than copied from
        // the record, whose `supersededBy` is only ever set on the archived COPY.
        superseded_by: None,
        current: true,
    });
    // Newest first, and the head wins a tie: two writes in the same millisecond are
    // possible (the version id carries a random salt precisely because they are), and a
    // history whose first entry was not the current version would be read as one.
    versions.sort_by_key(|version| (std::cmp::Reverse(version.updated_at_ms), !version.current));
    Ok(crate::NoteHistory {
        has_divergence: head.has_divergence,
        versions,
    })
}

/// A record field as an owned string, treating both an absent key and an explicit
/// `null` as absence — which is how Algolia represents an unset optional attribute.
fn optional_string(record: &Value, key: &str) -> Option<String> {
    record
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
}

/// Apply the floor-union-ceiling retention rule to one note's history records.
///
/// Best-effort by construction: it runs after the head has already moved, so a
/// failure here means old versions linger, not that the write did not land.
async fn purge_history(
    backend: &AlgoliaVaultBackend,
    remote_path: &str,
) -> Result<(), BackendError> {
    let (min_versions, max_age_days) = backend.retention();
    let history_notes = empty_if_missing_index(
        backend
            .client()
            .browse_all(
                backend.history_index(),
                Some(&format!(
                    "recordType:note AND noteId:{}",
                    super::quote_filter_value(remote_path)
                )),
            )
            .await,
        Vec::new(),
    )?;
    let versions: Vec<(String, u64)> = history_notes
        .iter()
        .filter_map(|record| {
            Some((
                record.get("versionId")?.as_str()?.to_string(),
                record
                    .get("updatedAtMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            ))
        })
        .collect();
    let keep = retention_keep_set(&versions, min_versions, max_age_days, now_ms());
    for (version_id, _) in versions {
        if keep.contains(&version_id) {
            continue;
        }
        empty_if_missing_index(
            backend
                .client()
                .delete_by_query(
                    backend.history_index(),
                    &format!(
                        "noteId:{} AND versionId:{}",
                        super::quote_filter_value(remote_path),
                        super::quote_filter_value(&version_id)
                    ),
                )
                .await,
            Value::Null,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(version_id: &str) -> NoteRecord {
        let meta = NoteVersionMeta {
            version_id: version_id.to_string(),
            parent_version_id: None,
            forked_from: None,
            has_divergence: false,
            participant_id: "tester".to_string(),
            updated_at_ms: 1,
        };
        build_note_records("A.md", "# A\n", &[], &meta).note
    }

    fn tombstone(version_id: &str) -> NoteRecord {
        let mut record = head(version_id);
        record.deleted = true;
        record
    }

    /// The three [`BaseVersion`] variants map to three different answers, and the
    /// `Absent` one is the arm #40's `Option<&str>` signature could not express.
    #[test]
    fn fork_detection_distinguishes_all_three_observations() {
        let existing = head("v1");

        // Observed the current head: a continuation, not a fork.
        assert_eq!(
            fork_of(&BaseVersion::Version("v1".to_string()), Some(&existing)),
            None
        );
        // Observed a version that is no longer the head: a fork off v1.
        assert_eq!(
            fork_of(&BaseVersion::Version("v0".to_string()), Some(&existing)),
            Some("v1".to_string())
        );
        // Observed NOTHING, but a head exists: a concurrent create, i.e. a fork.
        // This is the regression the `BaseVersion` port exists for.
        assert_eq!(
            fork_of(&BaseVersion::Absent, Some(&existing)),
            Some("v1".to_string())
        );
        // Observed nothing reliably: no precondition, so no divergence to assert.
        assert_eq!(fork_of(&BaseVersion::Unobserved, Some(&existing)), None);

        // Nothing there at all: never a fork, whatever the caller observed.
        for base in [
            BaseVersion::Unobserved,
            BaseVersion::Absent,
            BaseVersion::Version("v0".to_string()),
        ] {
            assert_eq!(fork_of(&base, None), None, "{base:?} against no head");
        }
    }

    /// Resurrecting a soft-deleted note is NOT a divergence.
    ///
    /// A read reports a tombstone as absent, so a caller arriving with
    /// `BaseVersion::Absent` observed the truth. Marking every undelete as divergent
    /// would fill `conflicted_paths()` with notes nobody disagreed about — and it is
    /// exactly what the unconditional `Absent` arm did before this case was split out.
    #[test]
    fn resurrecting_a_tombstone_is_not_a_fork_but_overwriting_a_delete_is() {
        let deleted = tombstone("v1");

        // The caller read, saw nothing (because a tombstone reads as absent), and
        // writes: a continuation of the chain, not a branch off it.
        assert_eq!(fork_of(&BaseVersion::Absent, Some(&deleted)), None);
        // Unobserved is likewise no assertion at all.
        assert_eq!(fork_of(&BaseVersion::Unobserved, Some(&deleted)), None);

        // But a caller holding a LIVE version that has since been deleted IS diverging:
        // the delete happened after their read, and overwriting it silently would erase
        // the deletion with no trace of the disagreement.
        assert_eq!(
            fork_of(&BaseVersion::Version("v0".to_string()), Some(&deleted)),
            Some("v1".to_string())
        );
        // ...unless the tombstone IS the version they observed, which cannot happen
        // through a read (a tombstone reads as absent) but is well-defined anyway.
        assert_eq!(
            fork_of(&BaseVersion::Version("v1".to_string()), Some(&deleted)),
            None
        );
    }
}
