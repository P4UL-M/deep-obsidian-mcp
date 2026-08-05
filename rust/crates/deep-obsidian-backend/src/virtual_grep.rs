//! An imitation of ripgrep, over note text a backend hands over rather than files.
//!
//! # What this is for
//!
//! `grep_search` means ripgrep. A backend whose corpus is not on disk cannot spawn
//! `rg`, but a backend that can hand over every note's exact text CAN reproduce what
//! `rg` would have printed — and then `grep_search` on that mount is exhaustive in
//! the same sense, not an approximation. That is the whole claim of this module, and
//! the [differential parity test][diff] is what backs it: the same [`GrepParams`] run
//! through `FilesystemVaultBackend` (real `rg`) and through a CouchDB mount holding
//! the same corpus must produce IDENTICAL [`GrepMatch`]es.
//!
//! [diff]: ../../tests/couchdb_sidecar.rs
//!
//! # The semantics being imitated, and where they come from
//!
//! Everything below was established empirically against `rg 14` driven with exactly
//! the argv [`crate::grep::run_grep`] builds, not from the manual:
//!
//! * **Pattern.** ripgrep is built on the `regex` crate, which is what this uses.
//!   `regex: false` is [`regex::escape`] (rg's `--fixed-strings`), and
//!   `case_sensitive: false` is [`RegexBuilder::case_insensitive`] (rg's
//!   `--ignore-case`). Those are the ONLY two pattern flags the rg path passes: no
//!   `--smart-case`, no `--word-regexp`, no `--multiline`, no `--max-columns`.
//! * **Line-oriented.** The haystack is one line at a time, so `^`/`$` anchor to the
//!   line and a pattern cannot span lines. Matches within a line are `find_iter`'s:
//!   non-overlapping, leftmost-first, byte offsets — the same iteration rg reports as
//!   its `submatches` array.
//! * **Line splitting.** Lines are terminated by `\n`, and a trailing `\n` does NOT
//!   produce a final empty line (`"a\n\n"` is two lines; `""` is none). A last line
//!   with no terminator still counts.
//! * **CRLF is NOT stripped before matching.** rg's line terminator is `\n` alone, so
//!   on a CRLF note the `\r` is part of the line: `needle$` does not match
//!   `crlf needle\r`, while `needle\r$` does, and a submatch can therefore contain a
//!   `\r`. [`GrepMatch::line_text`] keeps the `\r` for the same reason (it is rg's
//!   `lines.text` minus the trailing `\n`) while CONTEXT lines have it stripped,
//!   because context has never come from rg at all — see below. That asymmetry is
//!   frozen public behaviour, not an oversight.
//! * **Context.** The rg path does not use `-A`/`-B`: it re-reads each matched note
//!   and slices `context_lines` either side of every match INDEPENDENTLY. So adjacent
//!   matches repeat each other's lines and nothing is deduplicated or interleaved —
//!   there is no `--` separator logic to imitate. This module slices the same way,
//!   from the same [`crate::grep::split_note_lines`], so context text is identical to
//!   what a `read_file` of the note would report.
//! * **Glob.** [`GlobFilter`], below.
//!
//! # What is NOT reproduced, and why
//!
//! * **`.ignore` / `.gitignore` / `.rgignore`.** rg honours `.ignore` files
//!   unconditionally and `.gitignore` inside a git work tree; both were confirmed
//!   empirically. A LiveSync vault has no equivalent — its hidden entries are
//!   `i:`-prefixed internal documents the manifest never lists — so there is nothing
//!   to honour. Structural, not a shortfall.
//! * **Hidden and ignored directories.** rg is run with `--hidden`, so it searches
//!   `.somedir/note.md` and `node_modules/pkg/note.md`. The virtual corpus comes from
//!   the manifest and filters both (see the CouchDB backend's `grep_corpus`), so grep
//!   applies the same VISIBILITY rules as everything else on that mount — tombstones,
//!   hidden and ignored subtrees are out — while the glob, not the extension, decides
//!   which of the remaining entries are read. Deliberate: the alternative is a grep that
//!   finds notes no other tool on the mount can see.
//! * **A pattern that can match a newline.** rg exits 2 with `the literal "\n" is not
//!   allowed in a regex`; here the pattern compiles and simply matches nothing,
//!   because the haystack is a single line. Reproducing the refusal would mean
//!   reimplementing rg's HIR inspection, and a heuristic that guessed wrong would be
//!   worse than the documented difference.
//! * **Invalid UTF-8 and binary content.** rg reports such a line as base64 `bytes`
//!   (which the rg path's parser rejects) and skips a file it detects as binary. Note
//!   text arrives here as a `String`, so the case cannot arise.
//! * **Output ORDER.** rg walks in parallel, so its inter-file order is
//!   nondeterministic (observed to vary run to run). This module scans in sorted path
//!   order, which is deterministic. Within one note both are ascending by line. The
//!   consequence is that a `limit`-truncated result keeps the alphabetically first
//!   matches here and an arbitrary subset under rg; comparisons must sort, and the
//!   parity test does.

use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::{Regex, RegexBuilder};

use crate::grep::split_note_lines;
use crate::{BackendError, GrepContextLine, GrepMatch, GrepSubmatch};

/// The glob applied when the caller names none.
///
/// The rg path's own default (`--glob '*.md'` when `glob` is `None`), reproduced here
/// so a `glob`-less grep sees the same corpus on both kinds of mount.
pub const DEFAULT_GLOB: &str = "*.md";

/// The negative globs the rg path always passes.
///
/// Kept identical to [`crate::grep::run_grep`]'s argv so the two agree by construction.
///
/// # Defence in depth, not a reachable filter
///
/// Every prefix here starts with `.`, and the CouchDB backend's `grep_corpus` drops any
/// path with a hidden segment BEFORE a [`GlobFilter`] sees it. So on that backend these
/// three never decide anything; they are here because this module's job is to be the same
/// filter the rg path is, and a reader comparing the two must not find one of them short.
/// The unit test covering them is asserting the layering, not a path a request can take.
const ALWAYS_EXCLUDED: &[&str] = &["!.obsidian/**", "!.git/**", "!.deep-obsidian-mcp/**"];

/// ripgrep's `--glob` semantics over vault-relative paths.
///
/// # The two rules on top of `globset`
///
/// `globset` matches one pattern against one path; the gitignore-flavoured framing rg
/// puts around it is these two rules, both established empirically:
///
/// 1. **A pattern with no `/` matches the BASENAME, anywhere in the tree.** This is
///    why the default `*.md` finds `Notes/Deep/Gamma.md`. Compiled as `**/<pattern>`.
/// 2. **Polarity decides what an unmatched path means.** With at least one positive
///    pattern, a path no positive pattern matched is EXCLUDED. With only negative
///    (`!`) patterns, an unmatched path is INCLUDED — which is why `--glob '!sub/**'`
///    was observed to return matches from every file except `sub/`, including files
///    the default `*.md` would have excluded (the rg path does not add `*.md` when the
///    caller supplied a glob of their own).
///
/// An exclusion is FINAL here: nothing a later pattern says can re-include a path an
/// earlier `!` excluded. A full gitignore override list is last-match-wins, so the two
/// would differ for a caller glob that re-included something under `.obsidian/`,
/// `.git/` or `.deep-obsidian-mcp/` — but those are the only exclusions this filter ever
/// holds, they are all hidden prefixes, and [`ALWAYS_EXCLUDED`] explains why the CouchDB
/// corpus has already dropped every such path before this filter runs. The case is
/// unobservable rather than handled, and is written down here so nobody builds on the
/// simpler rule believing it to be ripgrep's.
///
/// # The one ripgrep behaviour deliberately NOT reproduced
///
/// rg roots its override matcher at the PROCESS WORKING DIRECTORY, not at the tree it
/// is searching. So on the rg path a glob containing a separator only fires when the
/// vault happens to live under the server's cwd: with cwd elsewhere,
/// `--glob 'Notes/**/*.md'` over an absolute vault path was observed to match NOTHING
/// (exit 1), while the same run from inside the vault matches as expected. That is a
/// cwd-dependent accident of how rg is invoked, and reproducing it would make a
/// glob-scoped grep on a CouchDB mount return nothing at all.
///
/// This filter anchors separator-bearing patterns at the MOUNT ROOT instead — i.e.
/// what rg does when run from the vault root, and what the router already assumes when
/// it strips a mount's prefix off the caller's glob before handing it down.
pub struct GlobFilter {
    /// Positive patterns, in caller order. Empty when the caller passed only
    /// negations.
    include: GlobSet,
    /// Negative (`!`) patterns, in caller order.
    exclude: GlobSet,
}

impl GlobFilter {
    /// Compile the rg path's always-excluded globs plus `glob` (or [`DEFAULT_GLOB`]).
    pub fn new(glob: Option<&str>) -> Result<Self, BackendError> {
        let mut include = GlobSetBuilder::new();
        let mut exclude = GlobSetBuilder::new();
        let mut positives = 0usize;
        for pattern in ALWAYS_EXCLUDED
            .iter()
            .copied()
            .chain(std::iter::once(glob.unwrap_or(DEFAULT_GLOB)))
        {
            let (negated, body) = match pattern.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, pattern),
            };
            // A leading `/` is tolerated everywhere else a path is configured, and
            // gitignore treats it as "anchored at the root" — which is what a
            // separator-bearing pattern already is here.
            let body = body.trim_start_matches('/');
            // Rule 1: no separator means match the basename anywhere.
            let anchored = if body.contains('/') {
                body.to_string()
            } else {
                format!("**/{body}")
            };
            let compiled = Glob::new(&anchored).map_err(|error| {
                BackendError::Message(format!(
                    "grep_search glob {pattern:?} could not be interpreted: {error}"
                ))
            })?;
            if negated {
                exclude.add(compiled);
            } else {
                positives += 1;
                include.add(compiled);
            }
        }
        let build = |builder: GlobSetBuilder| {
            builder
                .build()
                .map_err(|error| BackendError::Message(error.to_string()))
        };
        let include = build(include)?;
        let exclude = build(exclude)?;
        debug_assert!(
            positives <= 1,
            "the rg path passes at most one positive glob"
        );
        Ok(Self { include, exclude })
    }

    /// True when a mount-relative path is in scope.
    ///
    /// Rule 2 lives here: with no positive pattern at all, everything the negations did
    /// not exclude is in scope.
    pub fn is_match(&self, relative_path: &str) -> bool {
        if self.exclude.is_match(relative_path) {
            return false;
        }
        self.include.is_empty() || self.include.is_match(relative_path)
    }
}

/// A compiled pattern, built the way the rg path's flags build one.
#[derive(Debug)]
pub struct LineMatcher(Regex);

impl LineMatcher {
    /// `regex == false` is `--fixed-strings`; `case_sensitive == false` is
    /// `--ignore-case`. There is no third flag on the rg path.
    pub fn new(query: &str, regex: bool, case_sensitive: bool) -> Result<Self, BackendError> {
        let pattern = if regex {
            query.to_string()
        } else {
            regex::escape(query)
        };
        RegexBuilder::new(&pattern)
            .case_insensitive(!case_sensitive)
            .build()
            .map(Self)
            .map_err(|error| {
                BackendError::Message(format!("grep_search pattern is not a valid regex: {error}"))
            })
    }
}

/// Split note text into rg's lines.
///
/// rg terminates lines on `\n` and does not invent a final empty one for a trailing
/// terminator, so this is NOT `split('\n')`. The `\r` of a CRLF pair stays in the line
/// because rg's terminator is `\n` alone — see the module docs.
fn match_lines(content: &str) -> impl Iterator<Item = &str> {
    content
        .split_inclusive('\n')
        .map(|line| line.strip_suffix('\n').unwrap_or(line))
}

/// Evaluate `matcher` over one note's lines and append every match, with context.
///
/// Stops as soon as `matches.len()` reaches `limit`, which is where the rg path stops
/// too (it breaks out of the JSON event loop). `path` is reported verbatim, so the
/// caller decides whether it is mount-relative or logical.
///
/// Returns `true` when the limit is now reached, so a scan can stop opening notes.
pub fn collect_note_matches(
    path: &str,
    content: &str,
    matcher: &LineMatcher,
    context_lines: usize,
    limit: usize,
    matches: &mut Vec<GrepMatch>,
) -> bool {
    // The context slice's line text, which is `read_file`'s and therefore has the `\r`
    // of a CRLF pair stripped — unlike `line_text` below. Built lazily: a note with no
    // match, or a `context_lines: 0` request, must not pay for it.
    let mut context_source: Option<Vec<String>> = None;
    for (index, line) in match_lines(content).enumerate() {
        if matches.len() >= limit {
            return true;
        }
        let submatches: Vec<GrepSubmatch> = matcher
            .0
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
        let (context_before, context_after) = if context_lines == 0 {
            (Vec::new(), Vec::new())
        } else {
            let lines = context_source.get_or_insert_with(|| split_note_lines(content));
            context_window(lines, index, context_lines)
        };
        matches.push(GrepMatch {
            path: path.to_string(),
            line_number: index + 1,
            submatches,
            line_text: line.to_string(),
            context_before,
            context_after,
        });
    }
    matches.len() >= limit
}

/// The `context_lines` either side of `line_index`, sliced exactly as the rg path's
/// `populate_grep_context` slices them: independently per match, clamped at both ends,
/// with no deduplication between neighbouring matches.
fn context_window(
    lines: &[String],
    line_index: usize,
    context_lines: usize,
) -> (Vec<GrepContextLine>, Vec<GrepContextLine>) {
    let before_start = line_index.saturating_sub(context_lines);
    let before = lines[before_start..line_index.min(lines.len())]
        .iter()
        .enumerate()
        .map(|(offset, line)| GrepContextLine {
            line_number: before_start + offset + 1,
            line_text: line.clone(),
        })
        .collect();
    let after_start = (line_index + 1).min(lines.len());
    let after_end = (after_start + context_lines).min(lines.len());
    let after = lines[after_start..after_end]
        .iter()
        .enumerate()
        .map(|(offset, line)| GrepContextLine {
            line_number: after_start + offset + 1,
            line_text: line.clone(),
        })
        .collect();
    (before, after)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row below was READ OFF a real `rg 14` run with the rg path's argv. They
    /// are the evidence that the two rules on top of `globset` are rg's, not invented.
    #[test]
    fn the_default_glob_matches_a_basename_anywhere() {
        let filter = GlobFilter::new(None).expect("glob");
        assert!(filter.is_match("Root.md"));
        assert!(filter.is_match("Notes/Deep/Gamma.md"));
        // ...and only `.md`: rg's `*.md` did not report `Notes/Plain.txt`.
        assert!(!filter.is_match("Notes/Plain.txt"));
    }

    #[test]
    fn a_caller_glob_replaces_the_default_rather_than_narrowing_it() {
        let filter = GlobFilter::new(Some("*.txt")).expect("glob");
        assert!(filter.is_match("Notes/Plain.txt"));
        assert!(!filter.is_match("Root.md"));
    }

    /// `**/` matches zero segments too, so `**/*.md` includes a note at the root —
    /// observed to return the identical set to `*.md`.
    #[test]
    fn double_star_matches_zero_segments() {
        let filter = GlobFilter::new(Some("**/*.md")).expect("glob");
        assert!(filter.is_match("Root.md"));
        assert!(filter.is_match("Notes/Deep/Gamma.md"));
    }

    /// Brace alternation and character classes are ripgrep's, which is the whole
    /// reason its glob engine is used rather than a hand-rolled subset.
    #[test]
    fn brace_alternation_and_character_classes_behave_as_ripgrep_does() {
        let braces = GlobFilter::new(Some("{Adj,Dots}.md")).expect("glob");
        assert!(braces.is_match("Adj.md"));
        assert!(braces.is_match("Dots.md"));
        assert!(!braces.is_match("Unicode.md"));

        // `[AD]*.md` matched `Adj.md` AND `sub/Deep.md`: no separator in the pattern,
        // so it is a basename rule and applies at any depth.
        let class = GlobFilter::new(Some("[AD]*.md")).expect("glob");
        assert!(class.is_match("Adj.md"));
        assert!(class.is_match("sub/Deep.md"));
        assert!(!class.is_match("Unicode.md"));
    }

    /// Rule 2: with only negations, everything else is IN — including files the
    /// default `*.md` would have excluded, because the default is not added when the
    /// caller supplies a glob.
    #[test]
    fn a_negation_only_glob_includes_everything_it_did_not_exclude() {
        let filter = GlobFilter::new(Some("!sub/**")).expect("glob");
        assert!(filter.is_match("Adj.md"));
        assert!(filter.is_match("Unicode.md"));
        assert!(!filter.is_match("sub/Deep.md"));
        // A `.txt` is in scope as well: there is no positive pattern to exclude it.
        assert!(filter.is_match("Plain.txt"));
    }

    /// The three globs the rg path always passes still apply under a caller glob.
    #[test]
    fn the_always_excluded_prefixes_are_never_searched() {
        let filter = GlobFilter::new(Some("**/*.md")).expect("glob");
        assert!(!filter.is_match(".obsidian/plugins/x.md"));
        assert!(!filter.is_match(".git/COMMIT_EDITMSG.md"));
        assert!(!filter.is_match(".deep-obsidian-mcp/index.md"));
        assert!(filter.is_match("obsidian/x.md"));
    }

    /// Separator-bearing patterns anchor at the MOUNT ROOT — the deliberate divergence
    /// from rg's cwd-rooted override matcher, documented on [`GlobFilter`].
    #[test]
    fn a_separator_bearing_glob_anchors_at_the_mount_root() {
        let filter = GlobFilter::new(Some("Notes/**/*.md")).expect("glob");
        assert!(filter.is_match("Notes/Beta.md"));
        assert!(filter.is_match("Notes/Deep/Gamma.md"));
        assert!(!filter.is_match("Other/Beta.md"));
        // A leading slash is tolerated, as it is everywhere else in the config.
        assert!(GlobFilter::new(Some("/Notes/*.md"))
            .expect("glob")
            .is_match("Notes/Beta.md"));
    }

    /// rg's line splitting: a trailing terminator does not invent a final empty line,
    /// and a last line without one still counts.
    #[test]
    fn lines_are_terminated_not_separated() {
        assert_eq!(match_lines("").count(), 0);
        assert_eq!(match_lines("a\n").collect::<Vec<_>>(), vec!["a"]);
        assert_eq!(match_lines("a\n\n").collect::<Vec<_>>(), vec!["a", ""]);
        assert_eq!(match_lines("\n\n\n").collect::<Vec<_>>(), vec!["", "", ""]);
        assert_eq!(match_lines("a\nb").collect::<Vec<_>>(), vec!["a", "b"]);
        // CRLF: the `\r` belongs to the line, because rg's terminator is `\n` alone.
        assert_eq!(
            match_lines("a\r\nb\r\n").collect::<Vec<_>>(),
            vec!["a\r", "b\r"]
        );
    }

    /// The `\r` asymmetry, pinned. `line_text` keeps it (it is rg's `lines.text` minus
    /// the `\n`); context lines do not (they are `read_file`'s lines). A future reader
    /// who "fixes" either half breaks parity, and this is what tells them.
    #[test]
    fn crlf_stays_in_the_matched_line_and_leaves_the_context_lines() {
        let matcher = LineMatcher::new("needle", false, false).expect("matcher");
        let mut matches = Vec::new();
        collect_note_matches(
            "Crlf.md",
            "first\r\ncrlf needle\r\nthird\r\n",
            &matcher,
            1,
            50,
            &mut matches,
        );
        assert_eq!(matches.len(), 1);
        let found = &matches[0];
        assert_eq!(found.line_text, "crlf needle\r");
        assert_eq!(found.context_before[0].line_text, "first");
        assert_eq!(found.context_after[0].line_text, "third");
        // `$` therefore does not anchor after the `\r`...
        let anchored = LineMatcher::new("needle$", true, false).expect("matcher");
        let mut none = Vec::new();
        collect_note_matches("Crlf.md", "crlf needle\r\n", &anchored, 0, 50, &mut none);
        assert!(none.is_empty());
        // ...but `needle\r$` does, and the submatch carries the `\r`.
        let with_cr = LineMatcher::new("needle\\r$", true, false).expect("matcher");
        let mut one = Vec::new();
        collect_note_matches("Crlf.md", "crlf needle\r\n", &with_cr, 0, 50, &mut one);
        assert_eq!(one[0].submatches[0].text, "needle\r");
        assert_eq!(
            (one[0].submatches[0].start, one[0].submatches[0].end),
            (5, 12)
        );
    }

    /// Context is sliced per match with no deduplication, so adjacent matches repeat
    /// each other's lines — which is what the rg path does, because it slices the note
    /// itself rather than asking rg for `-A`/`-B`.
    #[test]
    fn adjacent_matches_repeat_context_rather_than_interleaving_it() {
        let matcher = LineMatcher::new("hit", false, false).expect("matcher");
        let mut matches = Vec::new();
        collect_note_matches(
            "Adj.md",
            "L1\nhit A\nhit B\nhit C\nL5\n",
            &matcher,
            1,
            50,
            &mut matches,
        );
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].context_before[0].line_text, "L1");
        assert_eq!(matches[0].context_after[0].line_text, "hit B");
        // The middle match's context is BOTH neighbouring matches, repeated verbatim.
        assert_eq!(matches[1].context_before[0].line_text, "hit A");
        assert_eq!(matches[1].context_after[0].line_text, "hit C");
        assert_eq!(matches[2].context_after[0].line_text, "L5");
        // Line numbers are the note's, one-based.
        assert_eq!(matches[1].line_number, 3);
        assert_eq!(matches[1].context_before[0].line_number, 2);
        assert_eq!(matches[1].context_after[0].line_number, 4);
    }

    /// Byte offsets, not character offsets, and Unicode case folding — both because
    /// this is the same `regex` crate ripgrep uses. The numbers are rg's, observed.
    #[test]
    fn submatch_offsets_are_bytes_and_folding_is_unicode() {
        let matcher = LineMatcher::new("hit", false, false).expect("matcher");
        let mut matches = Vec::new();
        collect_note_matches(
            "Unicode.md",
            "éà café hit 你好 hit\n",
            &matcher,
            0,
            50,
            &mut matches,
        );
        let offsets: Vec<(usize, usize)> = matches[0]
            .submatches
            .iter()
            .map(|item| (item.start, item.end))
            .collect();
        assert_eq!(offsets, vec![(11, 14), (22, 25)]);

        let folded = LineMatcher::new("CAFÉ", false, false).expect("matcher");
        let mut matches = Vec::new();
        collect_note_matches("Unicode.md", "éà café hit\n", &folded, 0, 50, &mut matches);
        assert_eq!(matches[0].submatches[0].text, "café");
        assert_eq!(matches[0].submatches[0].start, 5);
    }

    /// A fixed-string query is a LITERAL: `.*` finds `.*`, not every line.
    #[test]
    fn a_fixed_string_query_is_escaped() {
        let matcher = LineMatcher::new(".*", false, true).expect("matcher");
        let mut matches = Vec::new();
        collect_note_matches(
            "Dots.md",
            "literal .* here\nregex ab here\n",
            &matcher,
            0,
            50,
            &mut matches,
        );
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 1);
        assert_eq!(matches[0].submatches[0].text, ".*");
        assert_eq!(matches[0].submatches[0].start, 8);
    }

    /// Case sensitivity is the caller's, with no smart-case inference — the rg path
    /// passes `-i` or nothing.
    #[test]
    fn case_sensitivity_is_literal_with_no_smart_case() {
        let sensitive = LineMatcher::new("NEEDLE", false, true).expect("matcher");
        let mut matches = Vec::new();
        collect_note_matches("A.md", "a needle\n", &sensitive, 0, 50, &mut matches);
        assert!(matches.is_empty());
        let insensitive = LineMatcher::new("NEEDLE", false, false).expect("matcher");
        collect_note_matches("A.md", "a needle\n", &insensitive, 0, 50, &mut matches);
        assert_eq!(matches.len(), 1);
    }

    /// An empty pattern matches at every character boundary of every line, including
    /// the end — rg reported 13 zero-width submatches for a 12-byte line — and an
    /// empty note has no lines at all to match.
    #[test]
    fn an_empty_pattern_matches_every_boundary_and_an_empty_note_has_none() {
        let matcher = LineMatcher::new("", false, false).expect("matcher");
        let mut matches = Vec::new();
        collect_note_matches("A.md", "crlf needle\r\n", &matcher, 0, 50, &mut matches);
        assert_eq!(matches[0].submatches.len(), 13);
        assert_eq!(matches[0].submatches[12].start, 12);

        let mut none = Vec::new();
        collect_note_matches("Empty.md", "", &matcher, 0, 50, &mut none);
        assert!(none.is_empty());
    }

    /// `limit` stops the scan mid-note and says so, so the caller can stop reading.
    #[test]
    fn the_limit_stops_mid_note_and_reports_saturation() {
        let matcher = LineMatcher::new("hit", false, false).expect("matcher");
        let mut matches = Vec::new();
        let saturated =
            collect_note_matches("A.md", "hit\nhit\nhit\n", &matcher, 0, 2, &mut matches);
        assert!(saturated);
        assert_eq!(matches.len(), 2);
        // An already-full accumulator opens no further notes' worth of work.
        assert!(collect_note_matches(
            "B.md",
            "hit\n",
            &matcher,
            0,
            2,
            &mut matches
        ));
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn an_invalid_pattern_is_reported_as_a_pattern_error() {
        let error = LineMatcher::new("(unclosed", true, false).expect_err("invalid regex");
        assert!(error.to_string().contains("not a valid regex"), "{error}");
    }
}
