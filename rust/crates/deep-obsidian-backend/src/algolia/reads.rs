//! Hydrating reads over an Algolia-backed corpus.
//!
//! A read fetches the head note record (one small request — also the freshness
//! check), then serves the body from cache or reassembles it from the chunk records
//! of that head version.
//!
//! Ported from PR #40's `shared/reads.rs`. Two changes, both load-bearing:
//!
//! * [`reassemble_chunks`] is **gap-aware**. #40's version walked sorted chunks and
//!   appended lines past the previous chunk's end, which silently SHIFTS content up
//!   when a note's line coverage has a hole. It has one: the section chunker drops a
//!   whitespace-only preamble (`section_chunks` skips a tile whose text trims to
//!   empty), so `"\n\n# T\nbody\n"` has no chunk covering lines 1–2 and #40's
//!   reassembly would return the note without its leading blank lines — not
//!   byte-exact, and therefore a `contentHash` that no longer matches what a client
//!   fed back as `expectedHash`. Holes are reproduced as empty lines instead, which
//!   is exactly what they were.
//! * listings return MOUNT-RELATIVE paths and [`VaultChildEntry`] values rather than
//!   pre-prefixed strings, because the router owns the logical namespace here.

use std::collections::BTreeSet;

use deep_obsidian_algolia::records::NoteRecord;
use deep_obsidian_algolia::SearchRequest;
use serde_json::Value;
use tracing::warn;

use super::versioning::fetch_head;
use super::{empty_if_missing_index, empty_search_response, map_algolia, AlgoliaVaultBackend};
use crate::{BackendError, VaultChildEntry, VaultEntryKind};

/// Filter fragment selecting LIVE note records.
///
/// A soft-deleted note keeps its record as a tombstone so the removal is observable
/// and the content stays recoverable from history. Every listing and every search
/// must therefore exclude it explicitly, or a deleted note goes on showing up.
pub const LIVE_NOTES: &str = "recordType:note AND NOT deleted:true";

/// How many chunk records one hydration may fetch.
///
/// Algolia's own per-query ceiling is 1000 hits, and a note above 1000 chunks is
/// not a note this design serves. Kept as a named constant so the read path and the
/// history-read path cannot drift apart.
const MAX_CHUNKS_PER_NOTE: usize = 1000;

#[derive(Debug)]
pub struct HydratedNote {
    pub content: String,
    pub note: NoteRecord,
}

/// Reassemble a note body from its chunk records, de-duplicating overlap by line
/// range. Every chunk must belong to one (note, version).
///
/// Three shapes have to work at once:
///
/// * **exact tiling** — section chunks abut, so nothing overlaps and nothing repeats;
/// * **overlapping fallback** — the heading-less chunker overlaps by 12 lines, so a
///   line covered twice must be emitted once (blind concatenation would duplicate
///   twelve lines per boundary);
/// * **holes** — see the module docs: a line no chunk covers is a line the chunker
///   deliberately dropped as whitespace, and is restored as an empty line.
///
/// The first chunk to cover a line wins. Chunks of one version are built from one
/// source, so any two that cover a line agree about it.
pub fn reassemble_chunks(mut chunks: Vec<(usize, usize, String)>) -> String {
    chunks.sort_by_key(|(start_line, _, _)| *start_line);
    let mut lines: Vec<Option<String>> = Vec::new();
    for (start_line, end_line, text) in chunks {
        let _ = end_line;
        // A 0 `start_line` would be a malformed record; treating it as line 1 keeps
        // the arithmetic below in range rather than panicking on a subtraction.
        let start_line = start_line.max(1);
        for (offset, line) in text.split('\n').enumerate() {
            let index = start_line - 1 + offset;
            if lines.len() <= index {
                lines.resize(index + 1, None);
            }
            if lines[index].is_none() {
                lines[index] = Some(line.to_string());
            }
        }
    }
    lines
        .into_iter()
        .map(|line| line.unwrap_or_default())
        .collect::<Vec<String>>()
        .join("\n")
}

/// Every chunk record for one (note, version), from `index`.
///
/// `distinct` is explicitly OFF. The index-level `distinct` returns the best chunk
/// per path, so leaving it on would hand back ONE chunk of a multi-chunk note and
/// the reassembled body would silently be a fragment.
pub async fn fetch_version_chunks(
    backend: &AlgoliaVaultBackend,
    index: &str,
    remote_path: &str,
    version_id: &str,
) -> Result<Vec<(usize, usize, String)>, BackendError> {
    let response = empty_if_missing_index(
        backend
            .client()
            .search(
                index,
                &SearchRequest {
                    query: String::new(),
                    filters: Some(format!(
                        "recordType:chunk AND noteId:{} AND versionId:{}",
                        super::quote_filter_value(remote_path),
                        super::quote_filter_value(version_id)
                    )),
                    hits_per_page: Some(MAX_CHUNKS_PER_NOTE),
                    distinct: Some(false),
                    ..SearchRequest::default()
                },
            )
            .await,
        empty_search_response(),
    )?;
    Ok(response
        .hits
        .iter()
        .filter_map(|hit| {
            Some((
                hit.get("startLine")?.as_u64()? as usize,
                hit.get("endLine")?.as_u64()? as usize,
                hit.get("text")?.as_str()?.to_string(),
            ))
        })
        .collect())
}

/// Hydrate a note: head lookup (freshness check), cache hit or chunk reassembly,
/// cache fill.
///
/// A tombstoned head reads as absent: the record exists so the removal is
/// observable, but the note does not.
pub async fn read_note(
    backend: &AlgoliaVaultBackend,
    remote_path: &str,
) -> Result<HydratedNote, BackendError> {
    let note = fetch_head(backend, remote_path)
        .await?
        .filter(|note| !note.deleted)
        .ok_or_else(|| super::note_not_found(remote_path))?;
    if let Some(content) = backend.cache().get(remote_path, &note.version_id) {
        return Ok(HydratedNote { content, note });
    }
    let chunks =
        fetch_version_chunks(backend, backend.index(), remote_path, &note.version_id).await?;
    // A head pointing at a version with no chunks is a torn write, not an empty
    // note: the cutover pushes chunks BEFORE it moves the head, so this can only be
    // a crashed writer or a hand-edited index. Reporting an empty body would look
    // like the note was emptied on purpose.
    if chunks.is_empty() && note.chunk_count > 0 {
        return Err(BackendError::Message(format!(
            "note {remote_path} on this Algolia mount points at version {} but that version has \
             no chunk records, so its body cannot be reassembled (a write was interrupted \
             mid-cutover, or the index was edited outside this server); the previous version is \
             still in the history index",
            note.version_id
        )));
    }
    let content = reassemble_chunks(chunks);
    backend
        .cache()
        .put(remote_path, &note.version_id, &note.content_hash, &content);
    Ok(HydratedNote { content, note })
}

/// Reassemble ONE named version of a note, current or superseded.
///
/// # Why both indexes are tried, in this order
///
/// A version's chunks live in the MAIN index while it is the head and move to the history
/// index when something supersedes it. So "which index holds version v" is a question
/// about time, not about the version — and a caller who names the current version's id
/// (which is exactly what a `note_history` entry hands them) must not be told it does not
/// exist. Main first, history second.
///
/// The cache is deliberately NOT consulted: it is keyed by (path, head version) and holds
/// live content only. A superseded version is not live content, and a cache designed
/// around the head has no business answering a historical question.
///
/// A version nobody can find is a real, expected outcome — retention purges old versions
/// (see `versioning::purge_history`) — so the error says so rather than reporting a
/// missing note.
pub async fn read_note_version(
    backend: &AlgoliaVaultBackend,
    remote_path: &str,
    version_id: &str,
) -> Result<String, BackendError> {
    for index in [backend.index(), backend.history_index()] {
        let chunks = fetch_version_chunks(backend, index, remote_path, version_id).await?;
        if !chunks.is_empty() {
            return Ok(reassemble_chunks(chunks));
        }
    }
    Err(BackendError::io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!(
            "version {version_id} of {remote_path} is not on this Algolia mount: it was either \
             purged by the retention policy (the {} most recent versions plus anything younger \
             than {} days are kept) or the version id is wrong. note_history lists the versions \
             that are still readable.",
            backend.retention().0,
            backend.retention().1
        ),
    )))
}

/// The head record's declared size, for `Stat`.
///
/// Deliberately the RECORD's `sizeBytes` rather than the length of a hydrated body:
/// `Stat` must not pay for a full reassembly, and the record's figure is the length
/// of the exact bytes whose `contentHash` the record also carries.
pub async fn stat_note(
    backend: &AlgoliaVaultBackend,
    remote_path: &str,
) -> Result<u64, BackendError> {
    let note = fetch_head(backend, remote_path)
        .await?
        .filter(|note| !note.deleted)
        .ok_or_else(|| super::note_not_found(remote_path))?;
    Ok(note.size_bytes as u64)
}

/// Direct children of `prefix`: subfolders from folder-level facet values, files
/// from note records whose `dir` is that folder.
///
/// Returns `(children, folders_truncated)`. Algolia caps facet-value enumeration at
/// 100 per query and answers 400 rather than clamping above it, so a folder with
/// more than 100 direct subfolders cannot be enumerated exhaustively. The flag says
/// so; see [`AlgoliaVaultBackend`]'s docs for where it is reported.
///
/// Ordering matches core's `list_children` exactly — directories first, then files,
/// each group by path — because the MCP `list_children` payload is frozen on that
/// order and a caller must not be able to tell which backend answered.
pub async fn list_children(
    backend: &AlgoliaVaultBackend,
    prefix: Option<&str>,
    include_hidden: bool,
    include_ignored: bool,
) -> Result<(Vec<VaultChildEntry>, bool), BackendError> {
    let prefix = prefix.map(|prefix| prefix.trim_matches('/')).unwrap_or("");
    let depth = if prefix.is_empty() {
        0
    } else {
        prefix.split('/').count()
    };

    // Subfolders: the facet level equals the number of segments in `prefix`, and the
    // restriction is on the level ABOVE it.
    let facet = format!("folders.lvl{depth}");
    let filters = if prefix.is_empty() {
        LIVE_NOTES.to_string()
    } else {
        format!(
            "{LIVE_NOTES} AND folders.lvl{}:{}",
            depth - 1,
            super::quote_filter_value(prefix)
        )
    };
    let (facet_hits, folders_truncated) = empty_if_missing_index(
        backend
            .client()
            .search_facet_values_checked(backend.index(), &facet, "", Some(&filters))
            .await,
        (Vec::new(), false),
    )?;

    let mut directories: BTreeSet<String> = BTreeSet::new();
    for hit in facet_hits {
        let name = hit
            .value
            .rsplit('/')
            .next()
            .unwrap_or(hit.value.as_str())
            .to_string();
        if segment_is_filtered(&name, include_hidden, include_ignored) {
            continue;
        }
        directories.insert(hit.value);
    }

    // Files directly in this folder. The mount ROOT's records carry `dir: ""`, and
    // Algolia rejects an empty filter value (`dir:""` is a 400 "Not allowed empty
    // string"), so the root is enumerated by browsing note records and matching the
    // empty `dir` locally instead.
    let hits: Vec<Value> = if prefix.is_empty() {
        empty_if_missing_index(
            backend
                .client()
                .browse_all(backend.index(), Some(LIVE_NOTES))
                .await,
            Vec::new(),
        )?
        .into_iter()
        .filter(|record| {
            record
                .get("dir")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .is_empty()
        })
        .collect()
    } else {
        empty_if_missing_index(
            backend
                .client()
                .search(
                    backend.index(),
                    &SearchRequest {
                        query: String::new(),
                        filters: Some(format!(
                            "{LIVE_NOTES} AND dir:{}",
                            super::quote_filter_value(prefix)
                        )),
                        hits_per_page: Some(MAX_CHUNKS_PER_NOTE),
                        distinct: Some(false),
                        ..SearchRequest::default()
                    },
                )
                .await,
            empty_search_response(),
        )?
        .hits
    };

    let mut files: Vec<VaultChildEntry> = Vec::new();
    for hit in hits {
        let Some(path) = hit.get("path").and_then(Value::as_str) else {
            continue;
        };
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        if segment_is_filtered(&name, include_hidden, include_ignored) {
            continue;
        }
        files.push(VaultChildEntry {
            name,
            path: path.to_string(),
            kind: VaultEntryKind::File,
            is_markdown: is_markdown_path(path),
            size_bytes: hit.get("sizeBytes").and_then(Value::as_u64),
        });
    }

    let mut children: Vec<VaultChildEntry> = directories
        .into_iter()
        .map(|path| VaultChildEntry {
            name: path.rsplit('/').next().unwrap_or(&path).to_string(),
            path,
            kind: VaultEntryKind::Directory,
            is_markdown: false,
            // A folder synthesized from a facet has no size, exactly as a real
            // directory reports `None` from the filesystem backend.
            size_bytes: None,
        })
        .collect();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    children.extend(files);
    Ok((children, folders_truncated))
}

/// Every live markdown note on the mount, sorted.
///
/// Uses `browse`, not `search`: browse follows cursors to exhaustion, while a search
/// is capped at 1000 hits per page and would silently truncate a corpus above it.
/// `WalkMarkdown` feeds `find_files` and the resource listing, both of which are
/// meaningless if incomplete.
pub async fn walk_markdown(backend: &AlgoliaVaultBackend) -> Result<Vec<String>, BackendError> {
    let records = empty_if_missing_index(
        backend
            .client()
            .browse_all(backend.index(), Some(LIVE_NOTES))
            .await,
        Vec::new(),
    )?;
    let mut files: Vec<String> = records
        .iter()
        .filter_map(|record| record.get("path").and_then(Value::as_str))
        .filter(|path| is_markdown_path(path))
        .filter(|path| !path_is_filtered(path))
        .map(str::to_string)
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

/// Visible top-level folders, sorted.
pub async fn top_level_folders(backend: &AlgoliaVaultBackend) -> Result<Vec<String>, BackendError> {
    let (hits, truncated) = empty_if_missing_index(
        backend
            .client()
            .search_facet_values_checked(backend.index(), "folders.lvl0", "", Some(LIVE_NOTES))
            .await,
        (Vec::new(), false),
    )?;
    if truncated {
        warn!(
            "the Algolia index '{}' has more than {} top-level folders; facet-value enumeration \
             is capped there, so this folder list is not exhaustive",
            backend.index(),
            deep_obsidian_algolia::AlgoliaClient::MAX_FACET_HITS
        );
    }
    let mut folders: Vec<String> = hits
        .into_iter()
        .map(|hit| hit.value)
        .filter(|folder| !segment_is_filtered(folder, false, false))
        .collect();
    folders.sort();
    folders.dedup();
    Ok(folders)
}

/// Every live note whose head records a divergence, sorted.
///
/// "Divergence" here is not CouchDB's unreconciled sibling revision: nothing is
/// unresolved at the storage level, the head is unambiguous, and a read serves it.
/// It means a version was pushed whose base was NOT the head at push time, so the
/// forked content is sitting in the history index and has never been merged into the
/// line the head belongs to. See [`AlgoliaVaultBackend::conflicted_paths`].
pub async fn divergent_paths(backend: &AlgoliaVaultBackend) -> Result<Vec<String>, BackendError> {
    let records = empty_if_missing_index(
        backend
            .client()
            .browse_all(
                backend.index(),
                Some(&format!("{LIVE_NOTES} AND hasDivergence:true")),
            )
            .await,
        Vec::new(),
    )?;
    let mut paths: Vec<String> = records
        .iter()
        .filter(|record| {
            record
                .get("hasDivergence")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .filter_map(|record| record.get("path").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Probe the index for reachability: `get_settings`, tolerating "no index yet".
///
/// A never-written index answers 404, which means the app is reachable and the
/// corpus is empty — not that the mount is down. Every other failure (an invalid
/// key, a network error) reports unreachable, because a mount whose key has been
/// rotated cannot serve reads and saying it is reachable would be useless.
pub async fn probe_reachable(backend: &AlgoliaVaultBackend) -> bool {
    match backend.client().get_settings(backend.index()).await {
        Ok(_) => true,
        Err(error) if error.is_index_not_found() => true,
        Err(error) => {
            warn!(
                "Algolia index '{}' is not reachable: {}",
                backend.index(),
                map_algolia::<()>(Err(error)).expect_err("an error maps to an error")
            );
            false
        }
    }
}

/// True when a path segment is hidden or an ignored directory, mirroring core's
/// `should_ignore_entry` so an Algolia listing filters what a filesystem listing
/// filters.
fn segment_is_filtered(segment: &str, include_hidden: bool, include_ignored: bool) -> bool {
    if !include_hidden && segment.starts_with('.') {
        return true;
    }
    if !include_ignored && deep_obsidian_core::vault::DEFAULT_IGNORED_DIRS.contains(&segment) {
        return true;
    }
    false
}

/// True when ANY segment of `path` is hidden or an ignored directory. Mirrors core's
/// whole-subtree exclusion rather than filtering the leaf only.
fn path_is_filtered(path: &str) -> bool {
    path.split('/')
        .any(|segment| segment_is_filtered(segment, false, false))
}

pub(super) fn is_markdown_path(path: &str) -> bool {
    path.rsplit_once('.')
        .map(|(_, extension)| extension.eq_ignore_ascii_case("md"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Section chunks tile exactly, so reassembly is a plain concatenation — and the
    /// trailing newline survives, because the last tile covers the empty element
    /// `split('\n')` produces for it.
    #[test]
    fn reassemble_exact_tiling_round_trips() {
        let source = "# T\n\n## A\nalpha\n\n## B\nbeta\n";
        let lines: Vec<&str> = source.split('\n').collect();
        let chunks = vec![
            (1usize, 2usize, lines[0..2].join("\n")),
            (3, 5, lines[2..5].join("\n")),
            (6, 8, lines[5..8].join("\n")),
        ];
        assert_eq!(reassemble_chunks(chunks), source);
    }

    /// The heading-less fallback overlaps by 12 lines. A blind concatenation would
    /// repeat every overlap; dedup by line range must not.
    #[test]
    fn reassemble_dedups_overlapping_fallback_chunks() {
        let all: Vec<String> = (1..=6).map(|n| format!("line{n}")).collect();
        let chunks = vec![
            (1usize, 4usize, all[0..4].join("\n")),
            (3, 6, all[2..6].join("\n")), // overlaps lines 3-4
        ];
        assert_eq!(reassemble_chunks(chunks), all.join("\n"));
    }

    #[test]
    fn reassemble_handles_out_of_order_chunks() {
        let all: Vec<String> = (1..=4).map(|n| format!("l{n}")).collect();
        let chunks = vec![
            (3usize, 4usize, all[2..4].join("\n")),
            (1, 2, all[0..2].join("\n")),
        ];
        assert_eq!(reassemble_chunks(chunks), all.join("\n"));
    }

    /// The bug the gap-aware rewrite exists for: the section chunker drops a
    /// whitespace-only preamble, so lines 1-2 of this note have no chunk. Restoring
    /// them as empty lines is byte-exact; shifting the rest up is not.
    #[test]
    fn reassemble_restores_lines_no_chunk_covers() {
        let source = "\n\n# T\nbody\n";
        let lines: Vec<&str> = source.split('\n').collect();
        // Only the heading tile exists: lines 3..=5, i.e. `# T`, `body`, ``.
        let chunks = vec![(3usize, 5usize, lines[2..5].join("\n"))];
        assert_eq!(reassemble_chunks(chunks), source);
    }

    /// Every trailing-newline shape must survive, because `contentHash` is over the
    /// raw bytes and a lost or invented trailing newline breaks the `expectedHash`
    /// guard for that note forever.
    #[test]
    fn reassemble_preserves_every_trailing_newline_shape() {
        for source in [
            "# T\nbody\n",
            "# T\nbody",
            "# T\nbody\n\n",
            "single line no newline",
            "",
            "\n",
        ] {
            let lines: Vec<&str> = source.split('\n').collect();
            let chunks = vec![(1usize, lines.len(), source.to_string())];
            assert_eq!(
                reassemble_chunks(chunks),
                source,
                "round trip failed for {source:?}"
            );
        }
    }

    /// A hole at the END cannot be distinguished from the note ending there, so the
    /// chunker must cover the last line — which it does (`section_chunks` ends the
    /// last tile at `lines.len()`). This pins the consequence: trailing coverage is
    /// what makes the trailing newline recoverable at all.
    #[test]
    fn a_note_ending_mid_chunk_keeps_exactly_what_the_chunks_carry() {
        let chunks = vec![(1usize, 2usize, "a\nb".to_string())];
        assert_eq!(reassemble_chunks(chunks), "a\nb");
    }

    /// Real chunk plans, from the real chunker, for the shapes that matter: a
    /// multi-section note (exact tiling), a heading-less note long enough to trigger
    /// the overlapping fallback, and a blank-line preamble.
    #[test]
    fn reassembling_a_real_chunk_plan_is_byte_exact() {
        let long_body: String = (1..=80)
            .map(|n| format!("plain line {n} with enough words to matter\n"))
            .collect();
        let sources = vec![
            "# Title\n\n## A\n\nalpha body\n\n## B\n\nbeta body\n".to_string(),
            long_body,
            "\n\n# Late heading\n\nbody after blank lines\n".to_string(),
            "no heading and no trailing newline".to_string(),
        ];
        for source in sources {
            let title = deep_obsidian_index::index::note_title("Stem", &source);
            let planned = deep_obsidian_index::index::plan_note_chunks(&source, &title);
            let chunks: Vec<(usize, usize, String)> = planned
                .iter()
                .map(|chunk| (chunk.start_line, chunk.end_line, chunk.text.clone()))
                .collect();
            assert_eq!(
                reassemble_chunks(chunks),
                source,
                "the real chunk plan did not round trip for {:?}...",
                &source[..source.len().min(40)]
            );
        }
    }
}
