//! Rewriting inbound wikilinks after a note moved.
//!
//! # Why this is a repair pass and not part of the move
//!
//! A rename touches one note; its inbound links live in N others. Writing N notes is N
//! writes, and nothing at this boundary makes them one transaction — so a pass over them
//! can always be interrupted partway.
//!
//! What makes that acceptable is that the pass is **idempotent**: it replaces links that
//! point at the old target, and a note it already rewrote has none left, so re-running is a
//! no-op there while a note it never reached still gets fixed. The failure mode is "some
//! links still point at the old path", which is visible (they are broken links) and
//! repaired by running the same rename again.
//!
//! # Forms that have to be handled
//!
//! All of these resolve to the same note in Obsidian, and a rewrite that misses one leaves
//! a broken link that looks fine in the source:
//!
//! * `[[path]]` and `![[path]]` — the `!` sits outside the brackets, so it survives untouched
//! * `[[path|alias]]` — the alias is the author's display text and must be preserved
//! * `[[path\|alias]]` — the escaped pipe is how an alias is written INSIDE a table cell,
//!   and a rewrite that normalized it to a bare `|` would break the table
//! * `[[path#heading]]` and `[[path#heading|alias]]` — the fragment is preserved
//! * `[[basename]]` — Obsidian's shortest-path form, which resolves when the basename is
//!   unique in the vault. Rewriting it is what makes the pass complete, and also why an
//!   ambiguous destination has to be refused before any of this runs: if the new basename
//!   collides with an existing note, short links in *unrelated* notes silently change
//!   target.

/// One note's links, rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteOutcome {
    pub content: String,
    /// How many link occurrences were changed. Zero means the note was already consistent,
    /// which is the normal result of re-running the pass.
    pub rewritten: usize,
}

/// Strip a trailing `.md`, matching how a wikilink target is written.
fn link_target(note_path: &str) -> &str {
    note_path.strip_suffix(".md").unwrap_or(note_path)
}

fn basename(target: &str) -> &str {
    target.rsplit('/').next().unwrap_or(target)
}

/// Split a wikilink's inner text into target, `#fragment` and alias.
///
/// `\|` is treated as the alias separator as well as `|`, because that is how Obsidian
/// writes an alias inside a table cell, and which one was used has to be preserved on the
/// way out.
fn split_link(inner: &str) -> (String, Option<String>, Option<(bool, String)>) {
    let (target_and_fragment, alias) = match inner.find("\\|") {
        Some(index) => (
            &inner[..index],
            Some((true, inner[index + 2..].to_string())),
        ),
        None => match inner.find('|') {
            Some(index) => (
                &inner[..index],
                Some((false, inner[index + 1..].to_string())),
            ),
            None => (inner, None),
        },
    };
    match target_and_fragment.find('#') {
        Some(index) => (
            target_and_fragment[..index].to_string(),
            Some(target_and_fragment[index..].to_string()),
            alias,
        ),
        None => (target_and_fragment.to_string(), None, alias),
    }
}

/// Rewrite every link in `content` that resolves to `from_path`, pointing it at `to_path`.
///
/// `old_basename_was_unique` decides whether the shortest-path form is rewritten. When the
/// old basename was NOT unique in the vault, a bare `[[Name]]` did not unambiguously mean
/// this note, so touching it would be a guess.
pub fn rewrite_wiki_links(
    content: &str,
    from_path: &str,
    to_path: &str,
    old_basename_was_unique: bool,
) -> RewriteOutcome {
    let from_target = link_target(from_path);
    let to_target = link_target(to_path);
    let from_base = basename(from_target);

    let mut out = String::with_capacity(content.len());
    let mut rewritten = 0;
    let mut rest = content;

    while let Some(start) = rest.find("[[") {
        let Some(length) = rest[start + 2..].find("]]") else {
            break;
        };
        let inner = &rest[start + 2..start + 2 + length];
        out.push_str(&rest[..start]);

        let (target, fragment, alias) = split_link(inner);
        let trimmed = target.trim();
        let matches_full = trimmed == from_target;
        let matches_base = old_basename_was_unique && trimmed == from_base;

        if (matches_full || matches_base) && !inner.contains('[') && !inner.contains(']') {
            // A link written in the short form stays short when the new basename is still
            // usable that way; otherwise it has to become a full path or it would resolve
            // somewhere else, or nowhere.
            let replacement_target = if matches_base && !matches_full {
                basename(to_target)
            } else {
                to_target
            };
            out.push_str("[[");
            out.push_str(replacement_target);
            if let Some(fragment) = &fragment {
                out.push_str(fragment);
            }
            if let Some((escaped, alias)) = &alias {
                out.push_str(if *escaped { "\\|" } else { "|" });
                out.push_str(alias);
            }
            out.push_str("]]");
            rewritten += 1;
        } else {
            out.push_str("[[");
            out.push_str(inner);
            out.push_str("]]");
        }

        rest = &rest[start + 2 + length + 2..];
    }
    out.push_str(rest);

    RewriteOutcome {
        content: out,
        rewritten,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite(content: &str) -> RewriteOutcome {
        rewrite_wiki_links(content, "Old/Note.md", "New/Renamed.md", true)
    }

    #[test]
    fn a_plain_link_is_repointed() {
        let out = rewrite("see [[Old/Note]] here");
        assert_eq!(out.content, "see [[New/Renamed]] here");
        assert_eq!(out.rewritten, 1);
    }

    /// The alias is the author's display text; a rewrite that dropped it would silently
    /// change how the note reads.
    #[test]
    fn an_alias_is_preserved() {
        let out = rewrite("see [[Old/Note|the old name]] here");
        assert_eq!(out.content, "see [[New/Renamed|the old name]] here");
    }

    /// `\|` is the only way to write an alias inside a table cell. Normalizing it to a bare
    /// pipe would split the cell and break the table.
    #[test]
    fn an_escaped_pipe_stays_escaped() {
        let out = rewrite("| col | [[Old/Note\\|Old]] |");
        assert_eq!(out.content, "| col | [[New/Renamed\\|Old]] |");
    }

    #[test]
    fn a_heading_fragment_is_preserved() {
        let out = rewrite("see [[Old/Note#Status]] here");
        assert_eq!(out.content, "see [[New/Renamed#Status]] here");
    }

    #[test]
    fn a_fragment_and_an_alias_survive_together() {
        let out = rewrite("[[Old/Note#Status|status]]");
        assert_eq!(out.content, "[[New/Renamed#Status|status]]");
    }

    /// The `!` is outside the brackets, so an embed is repointed by the same code path.
    #[test]
    fn an_embed_is_repointed() {
        let out = rewrite("![[Old/Note]]");
        assert_eq!(out.content, "![[New/Renamed]]");
    }

    /// Obsidian's shortest-path form, rewritten to the NEW basename so it stays short.
    #[test]
    fn a_shortest_path_link_is_repointed_and_stays_short() {
        let out = rewrite("see [[Note]] here");
        assert_eq!(out.content, "see [[Renamed]] here");
        assert_eq!(out.rewritten, 1);
    }

    /// When the old basename was not unique, a bare `[[Note]]` never unambiguously meant
    /// this note, so rewriting it would be a guess.
    #[test]
    fn a_shortest_path_link_is_left_alone_when_the_old_basename_was_ambiguous() {
        let out = rewrite_wiki_links("see [[Note]] here", "Old/Note.md", "New/Renamed.md", false);
        assert_eq!(out.content, "see [[Note]] here");
        assert_eq!(out.rewritten, 0);
    }

    #[test]
    fn an_unrelated_link_is_untouched() {
        let out = rewrite("see [[Other/Thing]] and [[Old/NoteButLonger]]");
        assert_eq!(out.content, "see [[Other/Thing]] and [[Old/NoteButLonger]]");
        assert_eq!(out.rewritten, 0);
    }

    /// The property that makes the pass safe to interrupt: a second run changes nothing.
    #[test]
    fn the_pass_is_idempotent() {
        let first = rewrite("[[Old/Note]] and [[Old/Note|alias]]");
        assert_eq!(first.rewritten, 2);
        let second = rewrite(&first.content);
        assert_eq!(second.rewritten, 0);
        assert_eq!(second.content, first.content);
    }

    #[test]
    fn several_occurrences_in_one_note_are_all_rewritten() {
        let out = rewrite("[[Old/Note]] x [[Old/Note#H]] x ![[Old/Note|a]]");
        assert_eq!(out.rewritten, 3);
        assert!(!out.content.contains("Old/Note"), "{}", out.content);
    }

    /// A malformed link must not swallow the rest of the note.
    #[test]
    fn an_unterminated_link_leaves_the_tail_intact() {
        let out = rewrite("[[Old/Note]] then [[unterminated");
        assert!(out.content.ends_with("[[unterminated"), "{}", out.content);
        assert!(
            out.content.starts_with("[[New/Renamed]]"),
            "{}",
            out.content
        );
    }

    #[test]
    fn a_nested_bracket_link_is_skipped_like_the_extractor_does() {
        let out = rewrite("[[Old/Note]] and [[bad[link]]]");
        assert!(out.content.contains("[[bad[link]]]"), "{}", out.content);
        assert_eq!(out.rewritten, 1);
    }
}
