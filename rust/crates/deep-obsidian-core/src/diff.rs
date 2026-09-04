//! Line deltas between the content a write replaces and the content it stores.
//!
//! # Why a write reports counts at all
//!
//! A write tool that returns only hashes gives a caller no way to notice that it removed
//! more than it meant to. Both hashes are opaque, both change on any edit, and neither
//! says how much moved. The failure that motivates this is a caller composing a whole
//! document, dropping part of it by accident, and the response looking exactly like a
//! successful surgical edit.
//!
//! Counts are the cheapest signal that catches it, because the caller already knows the
//! magnitude it intended: a one-line change that reports 20 removals is wrong on its face.
//! They deliberately do NOT catch a volume-neutral mistake — five lines replaced by five
//! wrong lines reads as `+5 -5` — which is what [`unified_line_diff`] is for.
//!
//! # Cost
//!
//! [`line_delta`] trims the common prefix and suffix before doing any real work, so the
//! usual case (a small edit inside a large note) collapses to a tiny problem regardless of
//! note size. Only the genuinely changed span reaches the quadratic step, and that step is
//! bounded — see [`MAX_LCS_CELLS`].

/// How many lines changed between two versions of a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LineDelta {
    /// Lines present in the new content that are not part of the common subsequence.
    pub added: usize,
    /// Lines present in the previous content that are not part of the common subsequence.
    pub removed: usize,
}

impl LineDelta {
    /// True when neither side changed, i.e. the write was a no-op on content.
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0
    }
}

/// Ceiling on the LCS table, applied after prefix/suffix trimming.
///
/// A changed span larger than this is reported as a wholesale replacement of the span
/// rather than a line-by-line reconciliation. That is a deliberate honesty tradeoff: the
/// counts stay an upper bound on what moved, which still serves their purpose as a
/// magnitude tripwire, and a pathological input cannot make a write allocate without
/// bound. 4M cells is roughly a 2000x2000 changed span, far past any edit a caller makes
/// on purpose.
const MAX_LCS_CELLS: usize = 4_000_000;

fn split_lines(content: &str) -> Vec<&str> {
    content.split('\n').collect()
}

/// Count added and removed lines between `previous` and `next`.
///
/// `previous` is `None` for a note that did not exist, which reports every line of `next`
/// as added and nothing as removed.
pub fn line_delta(previous: Option<&str>, next: &str) -> LineDelta {
    let Some(previous) = previous else {
        return LineDelta {
            added: split_lines(next).len(),
            removed: 0,
        };
    };
    if previous == next {
        return LineDelta::default();
    }

    let old_lines = split_lines(previous);
    let new_lines = split_lines(next);

    // Trim the unchanged head and tail. For a one-line edit in a thousand-line note this
    // leaves a 1x1 problem, which is the whole point.
    let mut head = 0;
    while head < old_lines.len() && head < new_lines.len() && old_lines[head] == new_lines[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < old_lines.len() - head
        && tail < new_lines.len() - head
        && old_lines[old_lines.len() - 1 - tail] == new_lines[new_lines.len() - 1 - tail]
    {
        tail += 1;
    }

    let old_span = &old_lines[head..old_lines.len() - tail];
    let new_span = &new_lines[head..new_lines.len() - tail];

    if old_span.is_empty() || new_span.is_empty() {
        return LineDelta {
            added: new_span.len(),
            removed: old_span.len(),
        };
    }

    if old_span.len().saturating_mul(new_span.len()) > MAX_LCS_CELLS {
        return LineDelta {
            added: new_span.len(),
            removed: old_span.len(),
        };
    }

    let common = lcs_length(old_span, new_span);
    LineDelta {
        added: new_span.len() - common,
        removed: old_span.len() - common,
    }
}

/// Length of the longest common subsequence, over two rolling rows rather than a full
/// table so the memory cost is linear in the shorter side.
fn lcs_length(old_span: &[&str], new_span: &[&str]) -> usize {
    let mut previous_row = vec![0usize; new_span.len() + 1];
    let mut current_row = vec![0usize; new_span.len() + 1];

    for old_line in old_span {
        for (index, new_line) in new_span.iter().enumerate() {
            current_row[index + 1] = if old_line == new_line {
                previous_row[index] + 1
            } else {
                current_row[index].max(previous_row[index + 1])
            };
        }
        std::mem::swap(&mut previous_row, &mut current_row);
    }

    previous_row[new_span.len()]
}

/// A unified diff of the changed spans, with `context` unchanged lines around each.
///
/// This is what a caller asks for when the counts look wrong and it needs to see which
/// lines moved. It is not produced by default: on a whole-document write the caller
/// already holds the new content, so echoing it back is the response paying for
/// information the caller sent.
pub fn unified_line_diff(previous: Option<&str>, next: &str, context: usize) -> String {
    let previous = previous.unwrap_or("");
    if previous == next {
        return String::new();
    }

    let old_lines = split_lines(previous);
    let new_lines = split_lines(next);
    let script = diff_script(&old_lines, &new_lines);

    let mut out = String::new();
    let mut index = 0;
    while index < script.len() {
        if matches!(script[index], DiffOp::Keep(_)) {
            index += 1;
            continue;
        }

        // Walk back over up to `context` kept lines, then forward to the end of this run
        // of changes plus its trailing context.
        let hunk_start = index.saturating_sub(context);
        let mut hunk_end = index;
        let mut trailing = 0;
        while hunk_end < script.len() {
            match script[hunk_end] {
                DiffOp::Keep(_) => {
                    trailing += 1;
                    if trailing > context * 2 {
                        break;
                    }
                }
                _ => trailing = 0,
            }
            hunk_end += 1;
        }
        let hunk_end = (hunk_end - trailing.min(context)).min(script.len());

        let old_start = script[..hunk_start]
            .iter()
            .filter(|op| matches!(op, DiffOp::Keep(_) | DiffOp::Remove(_)))
            .count();
        let new_start = script[..hunk_start]
            .iter()
            .filter(|op| matches!(op, DiffOp::Keep(_) | DiffOp::Add(_)))
            .count();
        let old_count = script[hunk_start..hunk_end]
            .iter()
            .filter(|op| matches!(op, DiffOp::Keep(_) | DiffOp::Remove(_)))
            .count();
        let new_count = script[hunk_start..hunk_end]
            .iter()
            .filter(|op| matches!(op, DiffOp::Keep(_) | DiffOp::Add(_)))
            .count();

        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            old_start + 1,
            old_count,
            new_start + 1,
            new_count
        ));
        for op in &script[hunk_start..hunk_end] {
            match op {
                DiffOp::Keep(line) => out.push_str(&format!(" {line}\n")),
                DiffOp::Remove(line) => out.push_str(&format!("-{line}\n")),
                DiffOp::Add(line) => out.push_str(&format!("+{line}\n")),
            }
        }

        index = hunk_end.max(index + 1);
    }

    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffOp<'a> {
    Keep(&'a str),
    Remove(&'a str),
    Add(&'a str),
}

/// The full edit script. Only used by [`unified_line_diff`]; [`line_delta`] needs a count,
/// not a script, and reaches it without building the table.
fn diff_script<'a>(old_lines: &[&'a str], new_lines: &[&'a str]) -> Vec<DiffOp<'a>> {
    if old_lines.len().saturating_mul(new_lines.len()) > MAX_LCS_CELLS {
        let mut script: Vec<DiffOp<'a>> =
            old_lines.iter().map(|line| DiffOp::Remove(line)).collect();
        script.extend(new_lines.iter().map(|line| DiffOp::Add(line)));
        return script;
    }

    let mut table = vec![vec![0usize; new_lines.len() + 1]; old_lines.len() + 1];
    for old_index in (0..old_lines.len()).rev() {
        for new_index in (0..new_lines.len()).rev() {
            table[old_index][new_index] = if old_lines[old_index] == new_lines[new_index] {
                table[old_index + 1][new_index + 1] + 1
            } else {
                table[old_index + 1][new_index].max(table[old_index][new_index + 1])
            };
        }
    }

    let mut script = Vec::new();
    let mut old_index = 0;
    let mut new_index = 0;
    while old_index < old_lines.len() && new_index < new_lines.len() {
        if old_lines[old_index] == new_lines[new_index] {
            script.push(DiffOp::Keep(old_lines[old_index]));
            old_index += 1;
            new_index += 1;
        } else if table[old_index + 1][new_index] >= table[old_index][new_index + 1] {
            script.push(DiffOp::Remove(old_lines[old_index]));
            old_index += 1;
        } else {
            script.push(DiffOp::Add(new_lines[new_index]));
            new_index += 1;
        }
    }
    script.extend(
        old_lines[old_index..]
            .iter()
            .map(|line| DiffOp::Remove(line)),
    );
    script.extend(new_lines[new_index..].iter().map(|line| DiffOp::Add(line)));
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_note_reports_every_line_as_added() {
        let delta = line_delta(None, "alpha\nbeta\n");
        // Three, not two: `split` yields the trailing empty element after the final
        // newline, matching how the rest of the crate counts lines.
        assert_eq!(delta.added, 3);
        assert_eq!(delta.removed, 0);
    }

    #[test]
    fn an_unchanged_write_reports_nothing() {
        let delta = line_delta(Some("same\n"), "same\n");
        assert!(delta.is_empty());
    }

    #[test]
    fn a_one_line_substitution_is_one_each_way() {
        let previous = "a\nb\nc\n";
        let next = "a\nB\nc\n";
        assert_eq!(
            line_delta(Some(previous), next),
            LineDelta {
                added: 1,
                removed: 1
            }
        );
    }

    #[test]
    fn a_pure_insertion_removes_nothing() {
        let delta = line_delta(Some("a\nc\n"), "a\nb\nc\n");
        assert_eq!(delta.added, 1);
        assert_eq!(delta.removed, 0);
    }

    #[test]
    fn a_pure_deletion_adds_nothing() {
        let delta = line_delta(Some("a\nb\nc\n"), "a\nc\n");
        assert_eq!(delta.added, 0);
        assert_eq!(delta.removed, 1);
    }

    /// The case the counts exist for: a caller means to move three lines and silently
    /// drops four sections. The magnitude, not the identity, is what gives it away.
    #[test]
    fn a_truncating_rewrite_reports_a_magnitude_the_caller_can_reject() {
        let previous = "---\nstatus: active\n---\n\n# Title\n\n## Context\n\nbody\n\n## Decision\n\nbody\n\n## Options\n\nbody\n\n## Tradeoffs\n\nbody\n";
        // The caller intended only to lift `status` into a different value.
        let truncated = "---\nstatus: accepted\n---\n\n# Title\n\n## Context\n\nbody\n";
        let delta = line_delta(Some(previous), truncated);
        // 22 lines before, 10 after. The common prefix is `---` and the common suffix is
        // `\n\nbody\n`, leaving an 18-vs-6 span whose LCS is 5.
        assert_eq!(delta.removed, 13);
        assert_eq!(delta.added, 1);
    }

    #[test]
    fn a_volume_neutral_substitution_is_not_distinguishable_by_counts_alone() {
        let previous = "one\ntwo\nthree\n";
        let next = "uno\ndos\ntres\n";
        let delta = line_delta(Some(previous), next);
        assert_eq!(delta.added, 3);
        assert_eq!(delta.removed, 3);
        // Which is why the diff exists.
        let diff = unified_line_diff(Some(previous), next, 1);
        assert!(diff.contains("-one"));
        assert!(diff.contains("+uno"));
    }

    #[test]
    fn a_changed_span_past_the_ceiling_is_reported_as_a_whole_replacement() {
        let previous: String = (0..2100).map(|index| format!("old {index}\n")).collect();
        let next: String = (0..2100).map(|index| format!("new {index}\n")).collect();
        let delta = line_delta(Some(&previous), &next);
        // Every content line differs, so trimming only removes the shared trailing empty
        // element, leaving a 2100x2100 span. That is past the ceiling, so the counts
        // short-circuit to the upper bound rather than allocating the table.
        assert_eq!(delta.removed, 2100);
        assert_eq!(delta.added, 2100);
    }

    #[test]
    fn the_unified_diff_carries_a_hunk_header_and_both_sides() {
        let previous = "keep\nold\nkeep2\n";
        let next = "keep\nnew\nkeep2\n";
        let diff = unified_line_diff(Some(previous), next, 1);
        assert!(diff.starts_with("@@ -"), "{diff}");
        assert!(diff.contains("-old"), "{diff}");
        assert!(diff.contains("+new"), "{diff}");
        assert!(diff.contains(" keep"), "{diff}");
    }

    #[test]
    fn the_unified_diff_of_an_unchanged_write_is_empty() {
        assert!(unified_line_diff(Some("same"), "same", 3).is_empty());
    }

    #[test]
    fn a_created_note_diffs_against_nothing() {
        let diff = unified_line_diff(None, "first\n", 3);
        assert!(diff.contains("+first"), "{diff}");
    }
}
