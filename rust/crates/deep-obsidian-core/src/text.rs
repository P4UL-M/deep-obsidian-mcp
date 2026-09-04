use std::collections::HashMap;

use crate::vault::ChunkSection;

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "how", "in", "into", "is",
    "it", "of", "on", "or", "that", "the", "this", "to", "with",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadingSection {
    pub level: usize,
    pub title: String,
    pub slug: String,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSection {
    pub id: String,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

fn split_lines(content: &str) -> Vec<String> {
    content
        .split('\n')
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect()
}

fn is_stopword(token: &str) -> bool {
    STOPWORDS.iter().any(|candidate| *candidate == token)
}

fn is_heading_line(line: &str) -> bool {
    let level = line.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&level) {
        return false;
    }
    line.chars().nth(level).is_some_and(|ch| ch.is_whitespace())
}

fn strip_block_id_suffix(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_end();
    let caret = trimmed.rfind('^')?;
    let id = trimmed[caret + 1..].trim();
    if id.is_empty() || !id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-') {
        return None;
    }
    Some((trimmed[..caret].trim_end(), id))
}

pub fn tokenize(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0usize;

    while index < chars.len() {
        let ch = chars[index];
        if !ch.is_ascii_alphanumeric() {
            index += 1;
            continue;
        }

        let start = index;
        index += 1;
        while index < chars.len() {
            let next = chars[index];
            if next.is_ascii_alphanumeric() || next == '-' || next == '_' {
                index += 1;
            } else {
                break;
            }
        }

        let token: String = chars[start..index].iter().collect();
        if token.len() > 1 && !is_stopword(&token) {
            tokens.push(token);
        }
    }

    tokens
}

pub fn count_terms(text: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for token in tokenize(text) {
        *counts.entry(token).or_insert(0) += 1;
    }
    counts
}

pub fn token_count(term_counts: &HashMap<String, usize>) -> usize {
    term_counts.values().sum()
}

pub fn vector_norm(term_counts: &HashMap<String, usize>) -> f64 {
    term_counts
        .values()
        .map(|value| (*value as f64) * (*value as f64))
        .sum::<f64>()
        .sqrt()
}

pub fn normalize_heading_slug(text: &str) -> String {
    let filtered: String = text
        .trim()
        .to_lowercase()
        .chars()
        .filter(|ch| {
            !matches!(
                ch,
                '`' | '*'
                    | '_'
                    | '~'
                    | '['
                    | ']'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '<'
                    | '>'
                    | '#'
                    | '!'
                    | '?'
                    | '.'
                    | ','
                    | ':'
                    | ';'
                    | '\''
                    | '"'
                    | '\\'
                    | '/'
            )
        })
        .collect();

    filtered
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
        .split('-')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .trim_matches('-')
        .to_string()
}

pub fn frontmatter_title(content: &str) -> Option<String> {
    if !content.starts_with("---\n") {
        return None;
    }

    for line in content.split('\n').skip(1) {
        let line = line.trim_end_matches('\r');
        if line == "---" {
            break;
        }
        if line
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("title:"))
        {
            return Some(
                line[6..]
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string(),
            );
        }
    }

    None
}

pub fn heading_title(content: &str) -> Option<String> {
    split_lines(content).into_iter().find_map(|line| {
        line.strip_prefix("# ")
            .map(|title| title.trim().to_string())
    })
}

pub fn note_title(path_stem: &str, content: &str) -> String {
    frontmatter_title(content)
        .or_else(|| heading_title(content))
        .unwrap_or_else(|| path_stem.to_string())
}

pub fn extract_wiki_links(content: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut remaining = content;

    while let Some(start) = remaining.find("[[") {
        remaining = &remaining[start + 2..];
        let Some(end) = remaining.find("]]") else {
            break;
        };

        let raw = &remaining[..end];
        if !raw.contains('[') && !raw.contains(']') {
            let link = raw.split('|').next().unwrap_or("").trim();
            if !link.is_empty() {
                links.push(link.to_string());
            }
        }
        remaining = &remaining[end + 2..];
    }

    links
}

/// Section ranges that extend over nested subsections: a section ends at the next
/// heading of level **<= its own**, so an `##` section contains every `###` beneath it.
///
/// This is the boundary `note_outline` and the `obsidian://heading` resource need — both
/// present a heading together with its subtree, and both freeze it in golden snapshots.
/// For a range that stops at the next heading of *any* level, see
/// [`extract_shallow_heading_sections`].
pub fn extract_heading_sections(content: &str) -> Vec<HeadingSection> {
    collect_heading_sections(content, |candidate_level, own_level| {
        candidate_level <= own_level
    })
}

/// Section ranges that stop at the next heading of **any** level, so an `##` section
/// excludes the `###` subsections nested under it.
///
/// This is the boundary a *destructive* section write wants. Replacing a range produced by
/// [`extract_heading_sections`] deletes the nested subsections along with the body, which
/// is silent data loss whenever the caller's replacement text does not restate them.
///
/// Both functions share [`collect_heading_sections`] so they cannot drift on slug
/// generation, fence-blindness, or the trailing-line accounting described there.
pub fn extract_shallow_heading_sections(content: &str) -> Vec<HeadingSection> {
    collect_heading_sections(content, |_candidate_level, _own_level| true)
}

/// The shared scan behind both public section functions.
///
/// `stops_section(candidate_level, own_level)` decides whether a later heading terminates
/// the section being measured. The two callers differ only in that predicate.
///
/// Two conventions are load-bearing and must not be "cleaned up":
///
/// * **Fence-blind.** A `# foo` line inside a fenced code block reads as a heading. The
///   index crate has a fence-aware chunker for its own purposes; this family does not.
/// * **Trailing-line accounting.** [`split_lines`] yields a trailing empty element for
///   newline-terminated content, so the fallback `end_line = lines.len()` can exceed the
///   last visible line. Callers slice `lines[start_line - 1..end_line]`, and a destructive
///   caller feeds `end_line` straight into a tail splice, so changing this shifts
///   round-trip formatting.
fn collect_heading_sections(
    content: &str,
    stops_section: impl Fn(usize, usize) -> bool,
) -> Vec<HeadingSection> {
    let lines = split_lines(content);
    let mut headings = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        let level = line.chars().take_while(|ch| *ch == '#').count();
        if !(1..=6).contains(&level) {
            continue;
        }
        if !line.chars().nth(level).is_some_and(|ch| ch.is_whitespace()) {
            continue;
        }

        let title = line[level..].trim().to_string();
        headings.push((
            level,
            title.clone(),
            normalize_heading_slug(&title),
            index + 1,
        ));
    }

    headings
        .iter()
        .enumerate()
        .map(|(index, (level, title, slug, start_line))| {
            let mut end_line = lines.len();
            for next in headings.iter().skip(index + 1) {
                if stops_section(next.0, *level) {
                    end_line = next.3 - 1;
                    break;
                }
            }
            HeadingSection {
                level: *level,
                title: title.clone(),
                slug: slug.clone(),
                start_line: *start_line,
                end_line,
                text: lines[*start_line - 1..end_line].join("\n"),
            }
        })
        .collect()
}

pub fn extract_block_sections(content: &str) -> Vec<BlockSection> {
    let lines = split_lines(content);
    let mut blocks = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        let Some((inline_text, id)) = strip_block_id_suffix(line) else {
            continue;
        };

        if !inline_text.trim().is_empty() {
            blocks.push(BlockSection {
                id: id.to_string(),
                start_line: index + 1,
                end_line: index + 1,
                text: inline_text.trim().to_string(),
            });
            continue;
        }

        let mut start_line = index;
        while start_line > 0 {
            let previous = &lines[start_line - 1];
            if previous.trim().is_empty() || is_heading_line(previous) {
                break;
            }
            start_line -= 1;
        }

        blocks.push(BlockSection {
            id: id.to_string(),
            start_line: start_line + 1,
            end_line: index + 1,
            text: lines[start_line..index].join("\n").trim().to_string(),
        });
    }

    blocks
}

pub fn flatten_chunks(chunks: &[ChunkSection]) -> Vec<(usize, usize, usize, String)> {
    chunks
        .iter()
        .map(|chunk| {
            (
                chunk.chunk_index,
                chunk.start_line,
                chunk.end_line,
                chunk.text.clone(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_matches_expected_rules() {
        assert_eq!(
            tokenize("How to Build API_v2 and x"),
            vec!["build", "api_v2"]
        );
    }

    #[test]
    fn frontmatter_title_prefers_case_insensitive_title_key() {
        let content = "---\nTitle: \"Example Note\"\n---\n# Ignored\n";
        assert_eq!(frontmatter_title(content), Some("Example Note".to_string()));
    }

    #[test]
    fn heading_slug_removes_punctuation_and_collapses_hyphens() {
        assert_eq!(
            normalize_heading_slug("  Hello, world: /Rust/  "),
            "hello-world-rust"
        );
    }

    #[test]
    fn extract_wiki_links_skips_brackets_inside_targets() {
        let content = "See [[Alpha|A]] and [[bad[link]]] and [[Beta]].";
        assert_eq!(extract_wiki_links(content), vec!["Alpha", "Beta"]);
    }

    #[test]
    fn extract_heading_sections_preserves_ranges() {
        let content = "# One\ntext\n## Two\nmore\n";
        let sections = extract_heading_sections(content);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].start_line, 1);
        assert_eq!(sections[0].end_line, 5);
        assert_eq!(sections[0].text, "# One\ntext\n## Two\nmore\n");
        assert_eq!(sections[1].slug, "two");
        assert_eq!(sections[1].end_line, 5);
    }

    /// The two boundaries must disagree on a heading that actually has subsections.
    ///
    /// The pre-existing `extract_heading_sections_preserves_ranges` case uses same-level
    /// neighbours (`# One` then `## Two` with nothing after), where both rules yield the
    /// same end line. That made a boundary change invisible to the suite, so this case
    /// pins the difference explicitly.
    #[test]
    fn shallow_and_deep_boundaries_disagree_on_nested_subsections() {
        let content = "# Root\nintro\n## Child\nbody\n### Grandchild\ndeep\n## Sibling\ntail\n";

        let deep = extract_heading_sections(content);
        let shallow = extract_shallow_heading_sections(content);
        assert_eq!(deep.len(), 4);
        assert_eq!(shallow.len(), 4);

        // `# Root` spans everything under it: no later heading is at level <= 1, so the
        // deep scan falls back to `lines.len()` — 9, not 8, because `split_lines` yields a
        // trailing empty element for the newline-terminated content. The shallow scan
        // instead stops at `## Child` on line 3.
        assert_eq!(deep[0].title, "Root");
        assert_eq!(deep[0].end_line, 9);
        assert_eq!(shallow[0].end_line, 2);

        // `## Child` is the discriminating case: deep swallows `### Grandchild`, shallow
        // stops before it.
        assert_eq!(deep[1].title, "Child");
        assert_eq!(deep[1].end_line, 6);
        assert!(deep[1].text.contains("### Grandchild"));
        assert_eq!(shallow[1].title, "Child");
        assert_eq!(shallow[1].end_line, 4);
        assert!(!shallow[1].text.contains("### Grandchild"));

        // A leaf heading is identical under both rules.
        assert_eq!(deep[2].title, "Grandchild");
        assert_eq!(deep[2].end_line, shallow[2].end_line);
        assert_eq!(deep[3].title, "Sibling");
        assert_eq!(deep[3].end_line, shallow[3].end_line);
    }

    /// Slugs, levels and titles come from the shared scan, so they cannot drift between
    /// the two boundaries. Only `end_line`/`text` may differ.
    #[test]
    fn shallow_boundary_shares_slug_and_level_derivation() {
        let content = "# Root Note\n## Étape 1 — Cadrage\nbody\n### Detail\nmore\n";
        let deep = extract_heading_sections(content);
        let shallow = extract_shallow_heading_sections(content);

        assert_eq!(deep.len(), shallow.len());
        for (deep_section, shallow_section) in deep.iter().zip(shallow.iter()) {
            assert_eq!(deep_section.level, shallow_section.level);
            assert_eq!(deep_section.title, shallow_section.title);
            assert_eq!(deep_section.slug, shallow_section.slug);
            assert_eq!(deep_section.start_line, shallow_section.start_line);
        }
    }

    /// The deep boundary is frozen by `note_outline`'s golden snapshot and by the
    /// `obsidian://heading` resource body. Guard it here too, next to its sibling, so a
    /// future edit to the shared scan cannot silently change it.
    #[test]
    fn deep_boundary_is_unchanged_by_the_shared_scan() {
        let content = "# One\ntext\n## Two\nmore\n";
        let sections = extract_heading_sections(content);
        assert_eq!(sections[0].end_line, 5);
        assert_eq!(sections[0].text, "# One\ntext\n## Two\nmore\n");
    }

    #[test]
    fn extract_block_sections_supports_inline_and_indented_blocks() {
        let content =
            "Alpha ^a1\n\nParagraph one\nParagraph two\n^b2\n\n# Heading\nHeading text\n^c3\n";
        let sections = extract_block_sections(content);
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].text, "Alpha");
        assert_eq!(sections[1].text, "Paragraph one\nParagraph two");
        assert_eq!(sections[2].text, "Heading text");
    }
}
