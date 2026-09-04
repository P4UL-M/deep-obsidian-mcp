//! Applying a batch of edits to one note's text.
//!
//! # Two ways to address a region, one operation
//!
//! "Replace this exact text" and "declare this section's body" are not different
//! operations — they are two ways to name the region a write touches. Literal addressing
//! costs what the change costs and can reach anything (frontmatter, preamble prose, a
//! heading line, a span crossing section boundaries). Structural addressing lets a caller
//! declare a section without having read it, and create one that does not exist yet.
//!
//! Keeping both in one batch is what makes an edit atomic: a frontmatter property and the
//! body block it replaces move together or not at all.
//!
//! # Refusing rather than guessing
//!
//! Both forms refuse an ambiguous target instead of picking the first match. The existing
//! `update_note_section` takes the first heading whose title matches, which on a note with
//! a duplicated heading silently edits the wrong one — and a duplicated heading is exactly
//! the note someone is trying to repair. A refusal that names the candidate lines costs
//! one round trip; a wrong edit costs the content.

use deep_obsidian_core::{extract_heading_sections, extract_shallow_heading_sections};

/// One requested change, addressed either literally or structurally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditSpec {
    /// Replace an exact substring. `new` may be empty, which deletes.
    Literal {
        old: String,
        new: String,
        /// Replace every occurrence rather than requiring exactly one.
        replace_all: bool,
        /// Replace only the Nth occurrence, 1-based. Mutually exclusive with
        /// `replace_all`; the tool layer rejects the combination before we see it.
        occurrence: Option<usize>,
    },
    /// Replace a heading section's body, or create the section.
    Section {
        heading: String,
        level: usize,
        content: String,
        /// When false (the default), the replaced range stops at the next heading of any
        /// level, so nested subsections survive. When true they are part of the replaced
        /// region and `content` must restate any the caller wants to keep.
        include_subsections: bool,
        create_if_missing: bool,
        /// Placement when creating: insert after / before this heading rather than at the
        /// end of the note.
        after: Option<String>,
        before: Option<String>,
    },
}

/// What one edit did, for the response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedEdit {
    /// Index into the request's `edits` array.
    pub index: usize,
    /// 1-based line the edit landed on in the note as it stood when the edit ran.
    pub line: usize,
    /// `"replaced"`, `"deleted"`, `"updated"` or `"created"`.
    pub action: &'static str,
}

/// 1-based line number of a byte offset.
fn line_of_offset(content: &str, offset: usize) -> usize {
    content[..offset].matches('\n').count() + 1
}

/// Byte offsets of every non-overlapping occurrence of `needle`, scanning left to right.
fn occurrences(haystack: &str, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = haystack[cursor..].find(needle) {
        let absolute = cursor + relative;
        found.push(absolute);
        cursor = absolute + needle.len();
    }
    found
}

/// Whether the note opens with a delimited frontmatter block.
///
/// Structural only: it says the `---` fences are intact, not that what sits between them
/// is valid YAML. There is no YAML parser in this dependency tree, and a hand-rolled
/// validity lint would false-refuse legitimate documents (block scalars, nested maps,
/// quoted keys containing colons) — a false refusal on a write is worse than the gap.
/// What this does catch is a literal edit near the top of a note eating a fence, which is
/// the failure mode literal addressing actually introduces.
fn has_frontmatter_fences(content: &str) -> bool {
    let mut lines = content.split('\n');
    if lines.next().map(str::trim_end) != Some("---") {
        return false;
    }
    lines.any(|line| line.trim_end() == "---")
}

/// Apply every edit in order, or none of them.
///
/// Edits run against the evolving text, so a later edit may target something an earlier
/// one introduced. The caller gets the final content and a per-edit record; nothing is
/// written here.
pub fn apply_edits(
    content: &str,
    edits: &[EditSpec],
) -> Result<(String, Vec<AppliedEdit>), String> {
    let had_fences = has_frontmatter_fences(content);
    let mut working = content.to_string();
    let mut applied = Vec::with_capacity(edits.len());

    for (index, edit) in edits.iter().enumerate() {
        let record = match edit {
            EditSpec::Literal {
                old,
                new,
                replace_all,
                occurrence,
            } => apply_literal(&mut working, index, old, new, *replace_all, *occurrence)?,
            EditSpec::Section {
                heading,
                level,
                content: body,
                include_subsections,
                create_if_missing,
                after,
                before,
            } => apply_section(
                &mut working,
                index,
                heading,
                *level,
                body,
                *include_subsections,
                *create_if_missing,
                after.as_deref(),
                before.as_deref(),
            )?,
        };
        applied.push(record);
    }

    if had_fences && !has_frontmatter_fences(&working) {
        return Err(
            "refusing the batch: the note opened with a `---` frontmatter block and the \
             edits would leave it unterminated. Frontmatter that loses a fence stops being \
             parsed as properties at all, silently, so this is refused rather than written. \
             Re-check the edit that touches the top of the note."
                .to_string(),
        );
    }

    Ok((working, applied))
}

fn apply_literal(
    working: &mut String,
    index: usize,
    old: &str,
    new: &str,
    replace_all: bool,
    occurrence: Option<usize>,
) -> Result<AppliedEdit, String> {
    if old.is_empty() {
        return Err(format!(
            "edits[{index}]: `old` must not be empty; there is no such thing as replacing \
             nothing. To insert text, include enough surrounding context in `old` to say \
             where it goes, or address the region with `heading` instead."
        ));
    }

    let found = occurrences(working, old);
    if found.is_empty() {
        return Err(format!(
            "edits[{index}]: `old` was not found in the note. It must match verbatim, \
             including indentation and line breaks."
        ));
    }

    let action = if new.is_empty() {
        "deleted"
    } else {
        "replaced"
    };

    if replace_all {
        let line = line_of_offset(working, found[0]);
        *working = working.replace(old, new);
        return Ok(AppliedEdit {
            index,
            line,
            action,
        });
    }

    let target = match occurrence {
        Some(nth) => {
            let position = nth.checked_sub(1).ok_or_else(|| {
                format!("edits[{index}]: `occurrence` is 1-based, so 0 is not a position.")
            })?;
            *found.get(position).ok_or_else(|| {
                format!(
                    "edits[{index}]: asked for occurrence {nth} but `old` occurs {} time(s), \
                     on line(s) {}.",
                    found.len(),
                    render_lines(working, &found)
                )
            })?
        }
        None if found.len() > 1 => {
            return Err(format!(
                "edits[{index}]: `old` occurs {} times, on lines {}. Refusing rather than \
                 guessing which one you meant: widen `old` with surrounding context to make \
                 it unique, or set `occurrence` to pick one, or `replaceAll` to change every \
                 one.",
                found.len(),
                render_lines(working, &found)
            ));
        }
        None => found[0],
    };

    let line = line_of_offset(working, target);
    working.replace_range(target..target + old.len(), new);
    Ok(AppliedEdit {
        index,
        line,
        action,
    })
}

fn render_lines(content: &str, offsets: &[usize]) -> String {
    offsets
        .iter()
        .map(|offset| line_of_offset(content, *offset).to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[allow(clippy::too_many_arguments)]
fn apply_section(
    working: &mut String,
    index: usize,
    heading: &str,
    level: usize,
    body: &str,
    include_subsections: bool,
    create_if_missing: bool,
    after: Option<&str>,
    before: Option<&str>,
) -> Result<AppliedEdit, String> {
    let sections = if include_subsections {
        extract_heading_sections(working)
    } else {
        extract_shallow_heading_sections(working)
    };

    let matches: Vec<_> = sections
        .iter()
        .filter(|section| section.title == heading)
        .collect();

    if matches.len() > 1 {
        return Err(format!(
            "edits[{index}]: the note has {} headings titled {heading:?}, on lines {}. \
             Refusing rather than editing the first one: a duplicated heading is usually \
             the defect being repaired, so guessing would edit the wrong copy. Address it \
             with `old`/`new` and enough context to disambiguate.",
            matches.len(),
            matches
                .iter()
                .map(|section| section.start_line.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    if let Some(section) = matches.first() {
        let lines: Vec<&str> = working.split('\n').collect();
        let heading_line = lines[section.start_line - 1];
        let mut rebuilt: Vec<String> = lines[..section.start_line - 1]
            .iter()
            .map(|line| (*line).to_string())
            .collect();
        rebuilt.push(heading_line.to_string());
        let body_lines = trimmed_block(body);
        if !body_lines.is_empty() {
            rebuilt.push(String::new());
            rebuilt.extend(body_lines);
        }
        rebuilt.extend(
            lines[section.end_line.min(lines.len())..]
                .iter()
                .map(|line| (*line).to_string()),
        );
        let line = section.start_line;
        *working = rebuilt.join("\n");
        return Ok(AppliedEdit {
            index,
            line,
            action: "updated",
        });
    }

    if !create_if_missing {
        return Err(format!(
            "edits[{index}]: no heading titled {heading:?} in the note. Set \
             `createIfMissing` to add it."
        ));
    }

    // One creation path for all three placements. `insert_at` is the index in `lines`
    // that the new section is spliced in front of; appending is just "past the last line".
    let lines: Vec<&str> = working.split('\n').collect();
    let insert_at = match (after, before) {
        (Some(anchor_title), _) => {
            let anchor = find_anchor(&sections, anchor_title, index)?;
            anchor.end_line.min(lines.len())
        }
        (None, Some(anchor_title)) => {
            let anchor = find_anchor(&sections, anchor_title, index)?;
            anchor.start_line - 1
        }
        (None, None) => lines.len(),
    };

    let mut rebuilt: Vec<String> = lines[..insert_at]
        .iter()
        .map(|line| (*line).to_string())
        .collect();
    while rebuilt.last().is_some_and(|line| line.trim().is_empty()) {
        rebuilt.pop();
    }
    if !rebuilt.is_empty() {
        rebuilt.push(String::new());
    }
    rebuilt.push(format!(
        "{} {}",
        "#".repeat(level.clamp(1, 6)),
        heading.trim()
    ));
    // Captured here rather than derived afterwards: the heading's line is simply where it
    // was just pushed.
    let line = rebuilt.len();
    let body_lines = trimmed_block(body);
    if !body_lines.is_empty() {
        rebuilt.push(String::new());
        rebuilt.extend(body_lines);
    }
    rebuilt.push(String::new());
    rebuilt.extend(lines[insert_at..].iter().map(|line| (*line).to_string()));
    while rebuilt.last().is_some_and(|line| line.trim().is_empty()) {
        rebuilt.pop();
    }
    rebuilt.push(String::new());
    *working = rebuilt.join("\n");
    Ok(AppliedEdit {
        index,
        line,
        action: "created",
    })
}

/// The heading a created section is placed relative to.
fn find_anchor<'a>(
    sections: &'a [deep_obsidian_core::HeadingSection],
    title: &str,
    index: usize,
) -> Result<&'a deep_obsidian_core::HeadingSection, String> {
    sections
        .iter()
        .find(|section| section.title == title)
        .ok_or_else(|| {
            format!(
                "edits[{index}]: cannot place the new section: no heading titled \
                 {title:?} to anchor it to."
            )
        })
}

/// A replacement body with its blank edges trimmed, matching how
/// `update_or_create_note_section` normalizes one.
fn trimmed_block(body: &str) -> Vec<String> {
    let mut lines: Vec<String> = body.split('\n').map(|line| line.to_string()).collect();
    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal(old: &str, new: &str) -> EditSpec {
        EditSpec::Literal {
            old: old.to_string(),
            new: new.to_string(),
            replace_all: false,
            occurrence: None,
        }
    }

    fn section(heading: &str, body: &str) -> EditSpec {
        EditSpec::Section {
            heading: heading.to_string(),
            level: 2,
            content: body.to_string(),
            include_subsections: false,
            create_if_missing: false,
            after: None,
            before: None,
        }
    }

    #[test]
    fn a_literal_edit_replaces_one_occurrence_and_reports_its_line() {
        let content = "alpha\nbeta\ngamma\n";
        let (out, applied) = apply_edits(content, &[literal("beta", "BETA")]).expect("applies");
        assert_eq!(out, "alpha\nBETA\ngamma\n");
        assert_eq!(applied[0].line, 2);
        assert_eq!(applied[0].action, "replaced");
    }

    #[test]
    fn an_empty_replacement_deletes_and_is_reported_as_such() {
        let (out, applied) =
            apply_edits("keep\ndrop\n", &[literal("drop\n", "")]).expect("applies");
        assert_eq!(out, "keep\n");
        assert_eq!(applied[0].action, "deleted");
    }

    /// The interlock. A duplicated target is refused with the candidate lines, because
    /// picking one is how the wrong copy gets edited.
    #[test]
    fn an_ambiguous_literal_target_is_refused_with_its_line_numbers() {
        let content = "## Same\nbody\n\n## Same\nbody\n";
        let error = apply_edits(content, &[literal("## Same", "## Other")]).expect_err("refused");
        assert!(error.contains("occurs 2 times"), "{error}");
        assert!(error.contains("on lines 1, 4"), "{error}");
        assert!(error.contains("occurrence"), "{error}");
    }

    #[test]
    fn occurrence_picks_one_of_several() {
        let content = "x\nx\nx\n";
        let edit = EditSpec::Literal {
            old: "x".to_string(),
            new: "y".to_string(),
            replace_all: false,
            occurrence: Some(2),
        };
        let (out, applied) = apply_edits(content, &[edit]).expect("applies");
        assert_eq!(out, "x\ny\nx\n");
        assert_eq!(applied[0].line, 2);
    }

    #[test]
    fn replace_all_changes_every_occurrence() {
        let edit = EditSpec::Literal {
            old: "x".to_string(),
            new: "y".to_string(),
            replace_all: true,
            occurrence: None,
        };
        let (out, _) = apply_edits("x\nx\n", &[edit]).expect("applies");
        assert_eq!(out, "y\ny\n");
    }

    #[test]
    fn an_out_of_range_occurrence_reports_how_many_there_are() {
        let edit = EditSpec::Literal {
            old: "x".to_string(),
            new: "y".to_string(),
            replace_all: false,
            occurrence: Some(5),
        };
        let error = apply_edits("x\nx\n", &[edit]).expect_err("refused");
        assert!(error.contains("occurs 2 time(s)"), "{error}");
    }

    #[test]
    fn a_missing_literal_target_is_refused() {
        let error = apply_edits("alpha\n", &[literal("absent", "x")]).expect_err("refused");
        assert!(error.contains("was not found"), "{error}");
    }

    /// The whole point of the shallow boundary: a subsection under the target survives.
    #[test]
    fn a_section_edit_preserves_nested_subsections_by_default() {
        let content = "## Target\nold body\n### Nested\nkeep me\n## After\ntail\n";
        let (out, applied) =
            apply_edits(content, &[section("Target", "new body")]).expect("applies");
        assert!(out.contains("new body"), "{out}");
        assert!(out.contains("### Nested"), "{out}");
        assert!(out.contains("keep me"), "{out}");
        assert!(!out.contains("old body"), "{out}");
        assert_eq!(applied[0].action, "updated");
    }

    /// Opting in restates the old behaviour, and the subsection goes with the body.
    #[test]
    fn include_subsections_replaces_the_whole_subtree() {
        let content = "## Target\nold body\n### Nested\nkeep me\n## After\ntail\n";
        let edit = EditSpec::Section {
            heading: "Target".to_string(),
            level: 2,
            content: "new body".to_string(),
            include_subsections: true,
            create_if_missing: false,
            after: None,
            before: None,
        };
        let (out, _) = apply_edits(content, &[edit]).expect("applies");
        assert!(out.contains("new body"), "{out}");
        assert!(!out.contains("### Nested"), "{out}");
        assert!(out.contains("## After"), "{out}");
    }

    #[test]
    fn a_duplicated_heading_is_refused_rather_than_edited() {
        let content = "## Dup\none\n## Dup\ntwo\n";
        let error = apply_edits(content, &[section("Dup", "x")]).expect_err("refused");
        assert!(error.contains("2 headings titled"), "{error}");
        assert!(error.contains("on lines 1, 3"), "{error}");
    }

    #[test]
    fn a_missing_section_is_refused_unless_create_is_asked_for() {
        let error = apply_edits("# Note\n", &[section("Absent", "x")]).expect_err("refused");
        assert!(error.contains("createIfMissing"), "{error}");
    }

    #[test]
    fn create_if_missing_appends_at_the_end_by_default() {
        let edit = EditSpec::Section {
            heading: "New".to_string(),
            level: 2,
            content: "body".to_string(),
            include_subsections: false,
            create_if_missing: true,
            after: None,
            before: None,
        };
        let (out, applied) = apply_edits("# Note\n\nintro\n", &[edit]).expect("applies");
        assert!(out.contains("## New"), "{out}");
        assert!(out.contains("body"), "{out}");
        assert_eq!(applied[0].action, "created");
    }

    /// Placement is why `after` exists: appending at the end puts a section out of the
    /// order a template defines.
    #[test]
    fn after_places_a_created_section_where_the_template_wants_it() {
        let content = "## First\na\n## Third\nc\n";
        let edit = EditSpec::Section {
            heading: "Second".to_string(),
            level: 2,
            content: "b".to_string(),
            include_subsections: false,
            create_if_missing: true,
            after: Some("First".to_string()),
            before: None,
        };
        let (out, _) = apply_edits(content, &[edit]).expect("applies");
        let first = out.find("## First").expect("first");
        let second = out.find("## Second").expect("second");
        let third = out.find("## Third").expect("third");
        assert!(first < second && second < third, "{out}");
    }

    #[test]
    fn before_places_a_created_section_ahead_of_its_anchor() {
        let content = "## First\na\n## Third\nc\n";
        let edit = EditSpec::Section {
            heading: "Second".to_string(),
            level: 2,
            content: "b".to_string(),
            include_subsections: false,
            create_if_missing: true,
            after: None,
            before: Some("Third".to_string()),
        };
        let (out, _) = apply_edits(content, &[edit]).expect("applies");
        let second = out.find("## Second").expect("second");
        let third = out.find("## Third").expect("third");
        assert!(second < third, "{out}");
    }

    #[test]
    fn an_unknown_placement_anchor_is_refused() {
        let edit = EditSpec::Section {
            heading: "New".to_string(),
            level: 2,
            content: "b".to_string(),
            include_subsections: false,
            create_if_missing: true,
            after: Some("Nowhere".to_string()),
            before: None,
        };
        let error = apply_edits("## First\na\n", &[edit]).expect_err("refused");
        assert!(error.contains("no heading titled"), "{error}");
    }

    /// Mixing addressing modes in one batch is the case that justifies a batch at all:
    /// a frontmatter property and the body block it replaces move together.
    #[test]
    fn a_batch_mixes_literal_and_section_edits_atomically() {
        let content = "---\nstatus: active\n---\n\n# Note\n\n## Status\n\nold\n";
        let edits = vec![
            literal("status: active", "date: 2026-07-02\nstatus: accepted"),
            section("Status", "new"),
        ];
        let (out, applied) = apply_edits(content, &edits).expect("applies");
        assert!(out.contains("date: 2026-07-02"), "{out}");
        assert!(out.contains("status: accepted"), "{out}");
        assert!(out.contains("new"), "{out}");
        assert!(!out.contains("old"), "{out}");
        assert_eq!(applied.len(), 2);
    }

    #[test]
    fn a_failing_edit_discards_the_whole_batch() {
        let content = "alpha\nbeta\n";
        let edits = vec![literal("alpha", "ALPHA"), literal("absent", "x")];
        apply_edits(content, &edits).expect_err("refused");
        // Nothing is written here, so the proof is that the caller gets an Err and no
        // content: there is no partial value to observe.
    }

    #[test]
    fn later_edits_see_what_earlier_ones_wrote() {
        let edits = vec![literal("one", "two"), literal("two", "three")];
        let (out, _) = apply_edits("one\n", &edits).expect("applies");
        assert_eq!(out, "three\n");
    }

    /// The guard literal addressing makes necessary: reaching the top of a note means you
    /// can eat a fence, and frontmatter that loses one stops being parsed silently.
    #[test]
    fn an_edit_that_unterminates_the_frontmatter_is_refused() {
        let content = "---\nstatus: active\n---\n\nbody\n";
        let error = apply_edits(content, &[literal("---\n\nbody", "\nbody")]).expect_err("refused");
        assert!(error.contains("unterminated"), "{error}");
    }

    #[test]
    fn a_note_without_frontmatter_is_not_subject_to_the_fence_guard() {
        let content = "# Note\n\nbody\n";
        let (out, _) = apply_edits(content, &[literal("body", "text")]).expect("applies");
        assert_eq!(out, "# Note\n\ntext\n");
    }

    /// What `update_note_section`'s `target: "preamble"` used to do, now reachable by
    /// literal addressing — and reachable more precisely, since it edits the prose without
    /// having to restate the whole preamble.
    #[test]
    fn preamble_prose_is_editable_between_the_frontmatter_and_the_first_heading() {
        let content = "---\ntitle: Note\n---\n\nold intro prose\n\n## Section\n\nbody\n";
        let (out, applied) = apply_edits(content, &[literal("old intro prose", "new intro prose")])
            .expect("applies");
        assert!(out.starts_with("---\ntitle: Note\n---\n"), "{out}");
        assert!(out.contains("new intro prose"), "{out}");
        assert!(out.contains("## Section"), "{out}");
        assert_eq!(applied[0].line, 5);
    }

    #[test]
    fn an_empty_old_is_refused_with_an_explanation() {
        let error = apply_edits("x\n", &[literal("", "y")]).expect_err("refused");
        assert!(error.contains("must not be empty"), "{error}");
    }
}
