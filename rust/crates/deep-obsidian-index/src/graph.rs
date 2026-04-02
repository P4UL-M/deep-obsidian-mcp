use std::collections::{HashMap, VecDeque};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::index::{IndexError, Result, SearchIndex};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNode {
    pub path: String,
    pub title: String,
    pub depth: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    pub raw_link: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Graph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphDirection {
    Incoming,
    Outgoing,
    Both,
}

impl Default for GraphDirection {
    fn default() -> Self {
        GraphDirection::Both
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BacklinkMatch {
    pub path: String,
    pub title: String,
    pub matched_links: Vec<String>,
}

fn strip_md_extension(note_path: &str) -> &str {
    if note_path.to_lowercase().ends_with(".md") {
        &note_path[..note_path.len().saturating_sub(3)]
    } else {
        note_path
    }
}

pub fn resolve_wiki_link_target(index: &SearchIndex, source_path: &str, raw_link: &str) -> Option<String> {
    let clean = raw_link.split('#').next().unwrap_or("").trim();
    if clean.is_empty() {
        return None;
    }

    if let Some(note) = index
        .notes
        .iter()
        .find(|candidate| strip_md_extension(&candidate.path) == strip_md_extension(clean))
    {
        return Some(note.path.clone());
    }

    let source_dir = Path::new(source_path)
        .parent()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let relative_candidate = Path::new(source_dir)
        .join(clean)
        .to_string_lossy()
        .to_string();
    if let Some(note) = index
        .notes
        .iter()
        .find(|candidate| strip_md_extension(&candidate.path) == strip_md_extension(&relative_candidate))
    {
        return Some(note.path.clone());
    }

    let clean_stem = Path::new(clean)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(clean);
    let by_stem: Vec<_> = index
        .notes
        .iter()
        .filter(|note| {
            Path::new(&note.path)
                .file_stem()
                .and_then(|value| value.to_str())
                .map(|stem| stem.eq_ignore_ascii_case(clean_stem))
                .unwrap_or(false)
        })
        .collect();
    if by_stem.len() == 1 {
        return Some(by_stem[0].path.clone());
    }

    let by_title: Vec<_> = index
        .notes
        .iter()
        .filter(|note| note.title.eq_ignore_ascii_case(clean))
        .collect();
    if by_title.len() == 1 {
        return Some(by_title[0].path.clone());
    }

    None
}

pub fn get_outgoing_edges(index: &SearchIndex) -> Vec<GraphEdge> {
    let mut edges = Vec::new();
    for note in &index.notes {
        for raw_link in &note.links {
            if let Some(target) = resolve_wiki_link_target(index, &note.path, raw_link) {
                edges.push(GraphEdge {
                    source: note.path.clone(),
                    target,
                    raw_link: raw_link.clone(),
                });
            }
        }
    }
    edges
}

pub fn backlinks(index: &SearchIndex, note_path: &str, limit: usize) -> Result<Vec<BacklinkMatch>> {
    if index.note(note_path).is_none() {
        return Err(IndexError::NoteNotFound(note_path.to_string()));
    }

    let mut matches = Vec::new();
    for note in &index.notes {
        let matched_links = note
            .links
            .iter()
            .filter(|link| resolve_wiki_link_target(index, &note.path, link.as_str()).as_deref() == Some(note_path))
            .cloned()
            .collect::<Vec<_>>();
        if matched_links.is_empty() {
            continue;
        }

        matches.push(BacklinkMatch {
            path: note.path.clone(),
            title: note.title.clone(),
            matched_links,
        });
        if matches.len() >= limit.max(1) {
            break;
        }
    }

    Ok(matches)
}

pub fn graph_traverse(
    index: &SearchIndex,
    note_path: &str,
    direction: GraphDirection,
    depth: usize,
    limit: usize,
) -> Result<Graph> {
    let note_map: HashMap<&str, &str> = index
        .notes
        .iter()
        .map(|note| (note.path.as_str(), note.title.as_str()))
        .collect();
    if !note_map.contains_key(note_path) {
        return Err(IndexError::NoteNotFound(note_path.to_string()));
    }

    let outgoing = get_outgoing_edges(index);
    let incoming = outgoing
        .iter()
        .map(|edge| GraphEdge {
            source: edge.target.clone(),
            target: edge.source.clone(),
            raw_link: edge.raw_link.clone(),
        })
        .collect::<Vec<_>>();

    let chosen_edges = match direction {
        GraphDirection::Outgoing => outgoing,
        GraphDirection::Incoming => incoming,
        GraphDirection::Both => {
            let mut merged = outgoing;
            merged.extend(incoming);
            merged
        }
    };

    let mut adjacency: HashMap<String, Vec<GraphEdge>> = HashMap::new();
    for edge in chosen_edges {
        adjacency.entry(edge.source.clone()).or_default().push(edge);
    }

    let mut visited_depths: HashMap<String, usize> = HashMap::new();
    let mut visit_order: Vec<String> = Vec::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    visited_depths.insert(note_path.to_string(), 0);
    visit_order.push(note_path.to_string());
    queue.push_back(note_path.to_string());
    let mut traversed_edges = Vec::new();

    while let Some(current) = queue.pop_front() {
        let current_depth = *visited_depths.get(&current).unwrap_or(&0);
        if current_depth >= depth.max(1) {
            continue;
        }

        if let Some(edges) = adjacency.get(&current) {
            for edge in edges {
                traversed_edges.push(edge.clone());
                if !visited_depths.contains_key(&edge.target) {
                    visited_depths.insert(edge.target.clone(), current_depth + 1);
                    visit_order.push(edge.target.clone());
                    queue.push_back(edge.target.clone());
                    if visited_depths.len() >= limit.max(1) {
                        break;
                    }
                }
            }
        }
        if visited_depths.len() >= limit.max(1) {
            break;
        }
    }

    let nodes = visit_order
        .into_iter()
        .map(|path| {
            let title = note_map.get(path.as_str()).copied().unwrap_or(path.as_str()).to_string();
            let depth = *visited_depths.get(&path).unwrap_or(&0);
            GraphNode { path, title, depth }
        })
        .collect::<Vec<_>>();
    let edges = traversed_edges
        .into_iter()
        .filter(|edge| visited_depths.contains_key(&edge.source) && visited_depths.contains_key(&edge.target))
        .collect::<Vec<_>>();

    Ok(Graph { nodes, edges })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("deep-obsidian-graph-{label}-{nanos}-{suffix}"))
    }

    fn write_fixture(root: &Path, relative: &str, content: &str) {
        let absolute = root.join(relative);
        if let Some(parent) = absolute.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&absolute, content).expect("write fixture");
    }

    fn sample_index() -> SearchIndex {
        let root = unique_temp_dir("sample");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(&root, "Home.md", "# Home\n\nSee [[Projects/Brew Service]] and [[Research/Service Contract]].\n");
        write_fixture(&root, "Projects/Brew Service.md", "# Brew Service\n\nReference [[Home]].\n");
        write_fixture(&root, "Research/Service Contract.md", "# Service Contract\n\nReference [[Home]].\n");
        let index = crate::index::build_index(&root, None, None).expect("build index");
        fs::remove_dir_all(root).ok();
        index
    }

    #[test]
    fn resolve_links_prefers_exact_and_relative_matches() {
        let index = sample_index();
        assert_eq!(
            resolve_wiki_link_target(&index, "Home.md", "Projects/Brew Service"),
            Some("Projects/Brew Service.md".to_string())
        );
        assert_eq!(
            resolve_wiki_link_target(&index, "Projects/Brew Service.md", "Home"),
            Some("Home.md".to_string())
        );
    }

    #[test]
    fn backlinks_return_inbound_matches() {
        let index = sample_index();
        let matches = backlinks(&index, "Home.md", 20).expect("backlinks");
        assert!(matches.iter().any(|entry| entry.path == "Projects/Brew Service.md"));
    }

    #[test]
    fn graph_traversal_walks_neighbors() {
        let index = sample_index();
        let graph = graph_traverse(&index, "Home.md", GraphDirection::Both, 2, 20).expect("graph");
        assert!(graph.nodes.len() >= 3);
        assert!(graph.edges.len() >= 2);
    }
}
