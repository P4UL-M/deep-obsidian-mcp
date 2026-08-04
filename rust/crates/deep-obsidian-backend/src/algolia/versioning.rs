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
use crate::{BackendError, BaseVersion};

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
    // have not disagreed. A tombstone is the exception: its hash is the hash of an
    // empty body, so an empty write over one must still resurrect the note rather
    // than be short-circuited into "already up to date".
    if let Some(head_note) = &head {
        if !head_note.deleted
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
    // Divergence is sticky: a head-based write still has not merged the forked
    // content sitting in history, so the flag survives until something explicitly
    // resolves it (a 5c concern — this slice has no resolve path).
    let has_divergence = forked_from.is_some()
        || head
            .as_ref()
            .map(|note| note.has_divergence)
            .unwrap_or(false);
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

    if let (Some(prev_note), Some(prev_version)) = (&head, &head_version) {
        // (3) Copy the superseded version to history BEFORE deleting it from main.
        let chunk_filter = format!(
            "recordType:chunk AND noteId:{} AND versionId:{}",
            super::quote_filter_value(remote_path),
            super::quote_filter_value(prev_version)
        );
        let prev_chunks = empty_if_missing_index(
            backend
                .client()
                .browse_all(backend.index(), Some(&chunk_filter))
                .await,
            Vec::new(),
        )?;
        let mut history_records: Vec<Value> = prev_chunks;
        let mut prev_note_value =
            serde_json::to_value(prev_note).expect("a note record serializes");
        // History note records get a version-scoped objectID so several versions of
        // one note coexist there; only the MAIN index's note record is a stable head
        // pointer.
        prev_note_value["objectID"] = json!(format!("note:{remote_path}@{prev_version}"));
        prev_note_value["supersededBy"] = json!(version_id.clone());
        history_records.push(prev_note_value);
        super::map_algolia(
            backend
                .client()
                .save_objects(backend.history_index(), history_records)
                .await,
        )?;
        // That write is what brought the history index into existence, so its
        // settings can finally be applied.
        backend.ensure_history_settings().await;

        // (4) Delete the superseded chunks from main, by an EXPLICIT vPrev filter.
        // See the module docs for why the inverse filter is forbidden.
        empty_if_missing_index(
            backend
                .client()
                .delete_by_query(backend.index(), &chunk_filter)
                .await,
            Value::Null,
        )?;
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
