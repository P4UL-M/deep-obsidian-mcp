//! Candidate-bounded line search over an Algolia corpus.
//!
//! # Why this is not ripgrep, and says so
//!
//! `grep_search` on a filesystem mount is exhaustive: ripgrep reads every file. There
//! is no equivalent here — the corpus lives in an index that answers ranked lexical
//! queries, and "give me every line matching this regex" is not a query it has. So
//! the search is a two-stage approximation:
//!
//! 1. a LEXICAL PREFILTER pulls a bounded set of candidate chunks out of the index,
//!    using a literal anchor extracted from the caller's pattern;
//! 2. the caller's actual pattern is then evaluated locally, line by line, over those
//!    candidates' text.
//!
//! Stage 1 is where the honesty problem lives. It returns the top
//! [`CANDIDATE_LIMIT`] chunks by relevance, so a match sitting in the 201st most
//! relevant chunk is not found. This is reported rather than hidden: the candidate
//! count and the non-exhaustiveness are logged at `warn`, and a pattern with NO
//! usable literal anchor is REFUSED outright rather than answered from a prefilter
//! that would silently miss most of the corpus. The one thing this must never do is
//! return a short list that looks complete.
//!
//! `distinct` is off for stage 1. The index-level `distinct` returns the best chunk
//! per note, so leaving it on would silently drop every match in a note's other
//! chunks.
//!
//! Ported from PR #40's `shared/retrieval.rs` (`extract_literal_anchor`,
//! `search_mount_with_distinct`, `drop_superseded_hits`) and `shared_tools::grep_remote`.
//! The glob handling is new: #40 only ran its shared grep when the caller passed NO
//! glob, whereas here the router selects this mount BY the glob's literal prefix, so
//! the glob has to be honoured rather than declined.

use deep_obsidian_algolia::SearchRequest;
use regex::{Regex, RegexBuilder};
use serde_json::Value;
use std::collections::HashMap;
use tracing::warn;

use super::{empty_if_missing_index, empty_search_response, AlgoliaVaultBackend};
use crate::{BackendError, GrepContextLine, GrepMatch, GrepSubmatch};

/// How many candidate chunks the lexical prefilter may pull per query.
///
/// The number that makes this search useful without making it a corpus download.
/// Raising it costs one request's payload; lowering it loses matches. It is named so
/// the refusal message and the warning can quote the same figure the code uses.
pub const CANDIDATE_LIMIT: usize = 200;

/// The shortest literal run that can serve as a lexical anchor.
///
/// Below three characters an Algolia query is not selective enough to be a
/// prefilter — it would match most of the corpus, the top-200 cut would be
/// effectively arbitrary, and the result would look like a search that found
/// nothing rather than one that could not be run.
const MIN_ANCHOR_LEN: usize = 3;

/// Refusal for a regex pattern with no literal run long enough to prefilter on.
///
/// Deliberately explicit about the mechanism. A user who reaches this has written a
/// perfectly good ripgrep pattern and needs to know that it is the STORAGE that
/// cannot serve it, not the pattern that is wrong — and that the honest alternative
/// is a pattern carrying a literal, not a retry.
pub const ALGOLIA_GREP_NO_ANCHOR_MESSAGE: &str = "grep_search cannot run this pattern against \
this mount: it is an EXPERIMENTAL Algolia-backed shared corpus, whose line search is a lexical \
prefilter over the index followed by a local regex pass, and this pattern contains no literal run \
of 3 or more characters to prefilter on. Answering it would mean scanning an arbitrary slice of \
the corpus and reporting the result as if it were complete, which is worse than refusing. Add a \
literal substring to the pattern (for example \"retention.*policy\" rather than \"[a-z]+.*policy\"), \
or search this mount with hybrid_search.";

/// One candidate chunk from the lexical prefilter.
struct Candidate {
    path: String,
    start_line: usize,
    version_id: String,
    text: String,
}

/// Run a candidate-bounded grep. See the module docs for what "bounded" costs.
///
/// Returns `(matches, candidate_count)`. The count is the honesty half of the answer and
/// travels into the response rather than only into a log line: the caller's
/// `grep_search` payload reports it alongside `exhaustive: false`, so an agent reading a
/// short match list can tell "there are no more" from "I stopped looking".
pub async fn grep(
    backend: &AlgoliaVaultBackend,
    query: &str,
    regex_mode: bool,
    case_sensitive: bool,
    glob: Option<&str>,
    context_lines: usize,
    limit: usize,
) -> Result<(Vec<GrepMatch>, usize), BackendError> {
    if query.is_empty() {
        return Err(BackendError::Message(
            "grep_search requires a non-empty query".to_string(),
        ));
    }
    let pattern = if regex_mode {
        query.to_string()
    } else {
        regex::escape(query)
    };
    // A literal query IS its own anchor whatever its length: the caller asked for a
    // substring, and Algolia can serve a two-character query perfectly well. Only a
    // regex needs an anchor extracted, and only then can extraction fail.
    let anchor = if regex_mode {
        extract_literal_anchor(&pattern)
            .ok_or_else(|| BackendError::Unsupported(ALGOLIA_GREP_NO_ANCHOR_MESSAGE.to_string()))?
    } else {
        query.to_string()
    };
    let matcher = RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|error| {
            BackendError::Message(format!("grep_search pattern is not a valid regex: {error}"))
        })?;
    let path_filter = glob.map(compile_glob).transpose()?;

    let candidates = fetch_candidates(backend, &anchor).await?;
    let candidate_count = candidates.len();
    let mut matches: Vec<GrepMatch> = Vec::new();
    for candidate in candidates {
        if matches.len() >= limit {
            break;
        }
        if let Some(path_filter) = &path_filter {
            if !path_filter.is_match(&candidate.path) {
                continue;
            }
        }
        collect_chunk_matches(&candidate, &matcher, context_lines, limit, &mut matches);
    }

    // The honesty report, logged AND returned. The log is for whoever has to decide
    // whether `CANDIDATE_LIMIT` is too low; the returned count is for the caller, which
    // reports it in the `grep_search` payload. 5b could only log it because
    // `RecallResponse::Grep` was a bare `Vec`; it is now a `GrepOutcome`.
    warn!(
        "grep_search over Algolia index '{}' is CANDIDATE-BOUNDED, not exhaustive: the anchor \
         {anchor:?} returned {candidate_count} candidate chunks (cap {CANDIDATE_LIMIT}) and \
         {} match{} came out of the local regex pass; a match in a chunk the index ranked below \
         the cap is not reported",
        backend.index(),
        matches.len(),
        if matches.len() == 1 { "" } else { "es" }
    );
    Ok((matches, candidate_count))
}

/// Evaluate `matcher` over one candidate's lines, appending hits with context.
///
/// Context comes from the chunk's OWN lines only. A match on a chunk's first line
/// therefore has no `context_before` even though the note has lines above it:
/// fetching them would be an extra request per match, and reporting fewer context
/// lines is a visible, harmless shortfall, whereas silently attributing the previous
/// chunk's lines to the wrong positions would not be.
fn collect_chunk_matches(
    candidate: &Candidate,
    matcher: &Regex,
    context_lines: usize,
    limit: usize,
    matches: &mut Vec<GrepMatch>,
) {
    let lines: Vec<&str> = candidate.text.split('\n').collect();
    for (offset, line) in lines.iter().enumerate() {
        if matches.len() >= limit {
            return;
        }
        let submatches: Vec<GrepSubmatch> = matcher
            .find_iter(line)
            .map(|found| GrepSubmatch {
                start: found.start(),
                end: found.end(),
                text: found.as_str().to_string(),
            })
            .collect();
        if submatches.is_empty() {
            continue;
        }
        let line_number = candidate.start_line + offset;
        let context_before = (offset.saturating_sub(context_lines)..offset)
            .map(|index| GrepContextLine {
                line_number: candidate.start_line + index,
                line_text: lines[index].to_string(),
            })
            .collect();
        let context_after = ((offset + 1)..(offset + 1 + context_lines).min(lines.len()))
            .map(|index| GrepContextLine {
                line_number: candidate.start_line + index,
                line_text: lines[index].to_string(),
            })
            .collect();
        matches.push(GrepMatch {
            path: candidate.path.clone(),
            line_number,
            submatches,
            line_text: (*line).to_string(),
            context_before,
            context_after,
        });
    }
}

/// The lexical prefilter: up to [`CANDIDATE_LIMIT`] chunk records for `anchor`, with
/// every chunk belonging to a superseded version dropped.
async fn fetch_candidates(
    backend: &AlgoliaVaultBackend,
    anchor: &str,
) -> Result<Vec<Candidate>, BackendError> {
    let response = empty_if_missing_index(
        backend
            .client()
            .search(
                backend.index(),
                &SearchRequest {
                    query: anchor.to_string(),
                    // No `deleted` guard: chunk records carry no such attribute, and a
                    // soft delete removes a note's chunks from the main index
                    // outright, so a tombstoned note has no chunks left to match.
                    // Filtering an absent attribute would make the query depend on
                    // Algolia's missing-value semantics for nothing.
                    filters: Some("recordType:chunk".to_string()),
                    hits_per_page: Some(CANDIDATE_LIMIT),
                    // See the module docs: `distinct` returns the best chunk per note
                    // and would silently lose every match in the others.
                    distinct: Some(false),
                    ..SearchRequest::default()
                },
            )
            .await,
        empty_search_response(),
    )?;
    let candidates: Vec<Candidate> = response
        .hits
        .iter()
        .filter_map(|hit| {
            Some(Candidate {
                path: hit.get("path")?.as_str()?.to_string(),
                start_line: hit.get("startLine").and_then(Value::as_u64).unwrap_or(1) as usize,
                version_id: hit
                    .get("versionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                text: hit
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect();
    drop_superseded(backend, candidates).await
}

/// Drop candidates whose chunk belongs to a version that is no longer the head.
///
/// Two participants writing one note concurrently each push their own chunks; only
/// one wins the head pointer, and the loser's chunks stay in the main index as
/// ORPHANS. They are unreachable from the head (a read reassembles by head version),
/// but a plain chunk query still matches them, so a search would show text the note
/// no longer contains.
///
/// Deleting them instead would mean re-running exactly the destructive race the
/// explicit `versionId:vPrev` delete filter exists to avoid, so they are filtered at
/// QUERY time: one batched `getObjects` over the candidate paths, then keep only
/// head-version chunks.
async fn drop_superseded(
    backend: &AlgoliaVaultBackend,
    candidates: Vec<Candidate>,
) -> Result<Vec<Candidate>, BackendError> {
    if candidates.is_empty() {
        return Ok(candidates);
    }
    let mut paths: Vec<String> = candidates
        .iter()
        .map(|candidate| candidate.path.clone())
        .collect();
    paths.sort();
    paths.dedup();
    let ids: Vec<String> = paths
        .iter()
        .map(|path| deep_obsidian_algolia::note_object_id(path))
        .collect();
    let raw = backend.client().get_objects(backend.index(), &ids).await;
    // A secured key may be scoped so chunk records are visible but note records are
    // not. Failing the whole search there would be worse than skipping the head
    // check, so an unresolvable head keeps its candidates.
    if raw
        .as_ref()
        .err()
        .is_some_and(|error| error.is_forbidden_by_key_scope())
    {
        return Ok(candidates);
    }
    let records = empty_if_missing_index(raw, Vec::new())?;
    let mut head_of: HashMap<String, String> = HashMap::new();
    for record in records.into_iter().flatten() {
        let (Some(path), Some(version)) = (
            record.get("path").and_then(Value::as_str),
            record.get("versionId").and_then(Value::as_str),
        ) else {
            continue;
        };
        // A tombstoned note has no live content at all.
        if record
            .get("deleted")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        head_of.insert(path.to_string(), version.to_string());
    }
    Ok(candidates
        .into_iter()
        .filter(|candidate| {
            head_of
                .get(&candidate.path)
                .is_some_and(|head| *head == candidate.version_id)
        })
        .collect())
}

/// Compile a mount-relative ripgrep-style glob into a whole-path regex.
///
/// Supports the subset a `grep_search` caller actually writes: `**` (any number of
/// path segments), `*` (any run within one segment), `?` (one character within one
/// segment), and literals. Everything else is escaped.
///
/// A glob is honoured EXACTLY rather than reduced to its literal prefix, because the
/// router has already used that prefix to pick this mount: reducing it again here
/// would silently widen `Decisions/*.md` to the whole `Decisions/` subtree.
fn compile_glob(glob: &str) -> Result<Regex, BackendError> {
    let glob = glob.trim_start_matches('/');
    let mut pattern = String::from("^");
    let mut chars = glob.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    // `**/` matches zero or more whole segments, so `**/*.md` still
                    // matches a note at the root.
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        pattern.push_str("(?:[^/]+/)*");
                    } else {
                        pattern.push_str(".*");
                    }
                } else {
                    pattern.push_str("[^/]*");
                }
            }
            '?' => pattern.push_str("[^/]"),
            other => pattern.push_str(&regex::escape(&other.to_string())),
        }
    }
    pattern.push('$');
    Regex::new(&pattern).map_err(|error| {
        BackendError::Message(format!(
            "grep_search glob {glob:?} could not be interpreted: {error}"
        ))
    })
}

/// Extract a lexical anchor from a regex pattern: the longest run of plain literal
/// characters outside any regex metasyntax.
///
/// Returns `None` when the pattern has no usable anchor, and the caller must then
/// REFUSE rather than silently under-report. Ported verbatim from PR #40 apart from
/// the length floor becoming a named constant.
pub fn extract_literal_anchor(pattern: &str) -> Option<String> {
    let mut runs: Vec<String> = vec![String::new()];
    let mut chars = pattern.chars().peekable();
    let mut in_class = false;
    while let Some(character) = chars.next() {
        match character {
            '\\' => {
                // An escaped character is a literal only when it escapes a
                // metacharacter (`\.`, `\-`); `\b`, `\d`, `\w` are class shorthands
                // and break the run.
                if let Some(next) = chars.next() {
                    if !next.is_alphanumeric() {
                        runs.last_mut().expect("a run is always present").push(next);
                    } else {
                        runs.push(String::new());
                    }
                }
            }
            '[' => {
                in_class = true;
                runs.push(String::new());
            }
            ']' if in_class => {
                in_class = false;
            }
            _ if in_class => {}
            '*' | '+' | '?' => {
                // A quantifier makes the PRECEDING character optional or repeated, so
                // that character cannot be part of a literal anchor.
                let last = runs.last_mut().expect("a run is always present");
                last.pop();
                runs.push(String::new());
            }
            '.' | '^' | '$' | '(' | ')' | '|' | '{' | '}' => {
                runs.push(String::new());
            }
            other => runs
                .last_mut()
                .expect("a run is always present")
                .push(other),
        }
    }
    runs.into_iter()
        .max_by_key(String::len)
        .filter(|run| run.trim().len() >= MIN_ANCHOR_LEN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_anchor_extraction_and_refusal() {
        assert_eq!(
            extract_literal_anchor(r"\bSpacelift\b").as_deref(),
            Some("Spacelift")
        );
        assert_eq!(
            extract_literal_anchor("retention.*policy").as_deref(),
            Some("retention")
        );
        assert_eq!(
            extract_literal_anchor(r"foo\.bar baz").as_deref(),
            Some("foo.bar baz")
        );
        // No lexical anchor at all: the caller must be refused, not answered.
        assert_eq!(extract_literal_anchor(r"^\s*$"), None);
        assert_eq!(extract_literal_anchor("[a-z]+"), None);
        assert_eq!(extract_literal_anchor("a?b?"), None);
    }

    /// A literal query escapes to a pattern whose anchor is the query itself, so the
    /// prefilter and the local pass agree about what is being looked for.
    #[test]
    fn escaping_a_literal_query_round_trips_through_anchor_extraction() {
        assert_eq!(
            extract_literal_anchor(&regex::escape("foo.bar")).as_deref(),
            Some("foo.bar")
        );
    }

    #[test]
    fn globs_match_whole_paths_segment_aware() {
        let decisions = compile_glob("Decisions/*.md").expect("glob");
        assert!(decisions.is_match("Decisions/Alpha.md"));
        // `*` does not cross a separator, so a nested note is NOT matched.
        assert!(!decisions.is_match("Decisions/Deep/Alpha.md"));
        assert!(!decisions.is_match("Other/Alpha.md"));

        let recursive = compile_glob("Decisions/**/*.md").expect("glob");
        assert!(recursive.is_match("Decisions/Deep/Alpha.md"));
        // `**/` matches zero segments too.
        assert!(recursive.is_match("Decisions/Alpha.md"));
        assert!(!recursive.is_match("Other/Alpha.md"));

        let anywhere = compile_glob("**/*.md").expect("glob");
        assert!(anywhere.is_match("Alpha.md"));
        assert!(anywhere.is_match("A/B/C.md"));

        // Regex metacharacters in a glob are literals.
        let dotted = compile_glob("a.b/*.md").expect("glob");
        assert!(dotted.is_match("a.b/x.md"));
        assert!(!dotted.is_match("axb/x.md"));

        // A leading slash is tolerated, as it is everywhere else in the config.
        assert!(compile_glob("/Decisions/*.md")
            .expect("glob")
            .is_match("Decisions/Alpha.md"));
    }

    /// The refusal has to name the cause, the mechanism and a pattern that WOULD
    /// work. A bare "unsupported" sends the reader looking for a bug.
    #[test]
    fn the_no_anchor_refusal_names_the_mechanism_and_an_alternative() {
        assert!(ALGOLIA_GREP_NO_ANCHOR_MESSAGE.contains("EXPERIMENTAL"));
        assert!(ALGOLIA_GREP_NO_ANCHOR_MESSAGE.contains("lexical prefilter"));
        assert!(ALGOLIA_GREP_NO_ANCHOR_MESSAGE.contains("hybrid_search"));
        assert!(ALGOLIA_GREP_NO_ANCHOR_MESSAGE.contains("literal substring"));
    }

    /// Context comes from the chunk's own lines, and line numbers are the NOTE's,
    /// not the chunk's.
    #[test]
    fn matches_carry_note_line_numbers_and_in_chunk_context() {
        let candidate = Candidate {
            path: "A.md".to_string(),
            start_line: 10,
            version_id: "v1".to_string(),
            text: "alpha\nbeta needle\ngamma".to_string(),
        };
        let matcher = RegexBuilder::new("needle")
            .case_insensitive(true)
            .build()
            .expect("regex");
        let mut matches = Vec::new();
        collect_chunk_matches(&candidate, &matcher, 1, 50, &mut matches);
        assert_eq!(matches.len(), 1);
        let found = &matches[0];
        assert_eq!(found.line_number, 11, "chunk line 2 is note line 11");
        assert_eq!(found.line_text, "beta needle");
        assert_eq!(found.context_before.len(), 1);
        assert_eq!(found.context_before[0].line_number, 10);
        assert_eq!(found.context_after[0].line_number, 12);
        assert_eq!(found.submatches[0].text, "needle");
        assert_eq!(found.submatches[0].start, 5);
    }

    /// `limit` is respected inside one chunk, not merely between chunks.
    #[test]
    fn the_limit_stops_mid_chunk() {
        let candidate = Candidate {
            path: "A.md".to_string(),
            start_line: 1,
            version_id: "v1".to_string(),
            text: "hit\nhit\nhit".to_string(),
        };
        let matcher = Regex::new("hit").expect("regex");
        let mut matches = Vec::new();
        collect_chunk_matches(&candidate, &matcher, 0, 2, &mut matches);
        assert_eq!(matches.len(), 2);
    }
}
