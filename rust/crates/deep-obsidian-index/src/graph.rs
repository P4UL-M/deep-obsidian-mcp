use std::collections::{hash_map::Entry, BTreeSet, HashMap, VecDeque};
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum UniquePath {
    One(String),
    Ambiguous,
}

#[derive(Debug)]
struct LinkResolver {
    exact_paths: HashMap<String, String>,
    stems: HashMap<String, UniquePath>,
    titles: HashMap<String, UniquePath>,
}

impl LinkResolver {
    fn new(index: &SearchIndex) -> Self {
        let mut resolver = Self {
            exact_paths: HashMap::new(),
            stems: HashMap::new(),
            titles: HashMap::new(),
        };

        for note in &index.notes {
            resolver
                .exact_paths
                .entry(strip_md_extension(&note.path).to_string())
                .or_insert_with(|| note.path.clone());

            if let Some(stem) = Path::new(&note.path)
                .file_stem()
                .and_then(|value| value.to_str())
            {
                insert_unique_path(&mut resolver.stems, stem.to_lowercase(), &note.path);
            }
            insert_unique_path(&mut resolver.titles, note.title.to_lowercase(), &note.path);
        }

        resolver
    }

    fn resolve(&self, source_path: &str, raw_link: &str) -> Option<String> {
        let clean = raw_link.split('#').next().unwrap_or("").trim();
        if clean.is_empty() {
            return None;
        }

        if let Some(path) = self.exact_paths.get(strip_md_extension(clean)) {
            return Some(path.clone());
        }

        let source_dir = Path::new(source_path)
            .parent()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        let relative_candidate = Path::new(source_dir)
            .join(clean)
            .to_string_lossy()
            .to_string();
        if let Some(path) = self
            .exact_paths
            .get(strip_md_extension(&relative_candidate))
        {
            return Some(path.clone());
        }

        let clean_stem = Path::new(clean)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or(clean)
            .to_lowercase();
        if let Some(UniquePath::One(path)) = self.stems.get(&clean_stem) {
            return Some(path.clone());
        }

        if let Some(UniquePath::One(path)) = self.titles.get(&clean.to_lowercase()) {
            return Some(path.clone());
        }

        None
    }
}

fn insert_unique_path(map: &mut HashMap<String, UniquePath>, key: String, path: &str) {
    match map.entry(key) {
        Entry::Vacant(entry) => {
            entry.insert(UniquePath::One(path.to_string()));
        }
        Entry::Occupied(mut entry) => {
            if entry.get() != &UniquePath::One(path.to_string()) {
                entry.insert(UniquePath::Ambiguous);
            }
        }
    }
}

#[derive(Debug)]
struct GraphAdjacency {
    outgoing_edges: Vec<GraphEdge>,
    outgoing_by_source: HashMap<String, Vec<GraphEdge>>,
    incoming_by_source: HashMap<String, Vec<GraphEdge>>,
}

impl GraphAdjacency {
    fn build(index: &SearchIndex) -> Self {
        let resolver = LinkResolver::new(index);
        let mut outgoing_edges = Vec::new();
        let mut outgoing_by_source: HashMap<String, Vec<GraphEdge>> = HashMap::new();
        let mut incoming_by_source: HashMap<String, Vec<GraphEdge>> = HashMap::new();

        for note in &index.notes {
            for raw_link in &note.links {
                if let Some(target) = resolver.resolve(&note.path, raw_link) {
                    let outgoing = GraphEdge {
                        source: note.path.clone(),
                        target: target.clone(),
                        raw_link: raw_link.clone(),
                    };
                    let incoming = GraphEdge {
                        source: target,
                        target: note.path.clone(),
                        raw_link: raw_link.clone(),
                    };
                    outgoing_by_source
                        .entry(outgoing.source.clone())
                        .or_default()
                        .push(outgoing.clone());
                    incoming_by_source
                        .entry(incoming.source.clone())
                        .or_default()
                        .push(incoming);
                    outgoing_edges.push(outgoing);
                }
            }
        }

        Self {
            outgoing_edges,
            outgoing_by_source,
            incoming_by_source,
        }
    }
}

fn traverse_graph_edges(
    edges: Option<&Vec<GraphEdge>>,
    current_depth: usize,
    limit: usize,
    visited_depths: &mut HashMap<String, usize>,
    visit_order: &mut Vec<String>,
    queue: &mut VecDeque<String>,
    traversed_edges: &mut Vec<GraphEdge>,
) {
    if let Some(edges) = edges {
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
}

pub fn resolve_wiki_link_target(
    index: &SearchIndex,
    source_path: &str,
    raw_link: &str,
) -> Option<String> {
    LinkResolver::new(index).resolve(source_path, raw_link)
}

pub fn get_outgoing_edges(index: &SearchIndex) -> Vec<GraphEdge> {
    GraphAdjacency::build(index).outgoing_edges
}

/// Notes within one wikilink hop (outgoing or incoming) of any note in `anchors`,
/// excluding the anchors themselves. Builds the adjacency once. Used by the hybrid
/// graph-aware re-rank to lightly boost candidates that are link-adjacent to the
/// current top hits (issue #6 item #5).
pub(crate) fn one_hop_neighbor_notes(
    index: &SearchIndex,
    anchors: &BTreeSet<String>,
) -> BTreeSet<String> {
    let adjacency = GraphAdjacency::build(index);
    let mut neighbors = BTreeSet::new();
    for anchor in anchors {
        if let Some(edges) = adjacency.outgoing_by_source.get(anchor) {
            for edge in edges {
                neighbors.insert(edge.target.clone());
            }
        }
        if let Some(edges) = adjacency.incoming_by_source.get(anchor) {
            for edge in edges {
                neighbors.insert(edge.target.clone());
            }
        }
    }
    // A note that is itself a top hit gets no self-bonus.
    for anchor in anchors {
        neighbors.remove(anchor);
    }
    neighbors
}

pub fn backlinks(index: &SearchIndex, note_path: &str, limit: usize) -> Result<Vec<BacklinkMatch>> {
    if index.note(note_path).is_none() {
        return Err(IndexError::NoteNotFound(note_path.to_string()));
    }

    let mut matches = Vec::new();
    let adjacency = GraphAdjacency::build(index);
    for note in &index.notes {
        let matched_links =
            adjacency
                .outgoing_by_source
                .get(&note.path)
                .map_or_else(Vec::new, |edges| {
                    edges
                        .iter()
                        .filter(|edge| edge.target == note_path)
                        .map(|edge| edge.raw_link.clone())
                        .collect::<Vec<_>>()
                });
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

    let adjacency = GraphAdjacency::build(index);

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

        match &direction {
            GraphDirection::Outgoing => traverse_graph_edges(
                adjacency.outgoing_by_source.get(&current),
                current_depth,
                limit,
                &mut visited_depths,
                &mut visit_order,
                &mut queue,
                &mut traversed_edges,
            ),
            GraphDirection::Incoming => traverse_graph_edges(
                adjacency.incoming_by_source.get(&current),
                current_depth,
                limit,
                &mut visited_depths,
                &mut visit_order,
                &mut queue,
                &mut traversed_edges,
            ),
            GraphDirection::Both => {
                traverse_graph_edges(
                    adjacency.outgoing_by_source.get(&current),
                    current_depth,
                    limit,
                    &mut visited_depths,
                    &mut visit_order,
                    &mut queue,
                    &mut traversed_edges,
                );
                if visited_depths.len() < limit.max(1) {
                    traverse_graph_edges(
                        adjacency.incoming_by_source.get(&current),
                        current_depth,
                        limit,
                        &mut visited_depths,
                        &mut visit_order,
                        &mut queue,
                        &mut traversed_edges,
                    );
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
            let title = note_map
                .get(path.as_str())
                .copied()
                .unwrap_or(path.as_str())
                .to_string();
            let depth = *visited_depths.get(&path).unwrap_or(&0);
            GraphNode { path, title, depth }
        })
        .collect::<Vec<_>>();
    let edges = traversed_edges
        .into_iter()
        .filter(|edge| {
            visited_depths.contains_key(&edge.source) && visited_depths.contains_key(&edge.target)
        })
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
        write_fixture(
            &root,
            "Home.md",
            "# Home\n\nSee [[Projects/Brew Service]] and [[Research/Service Contract]].\n",
        );
        write_fixture(
            &root,
            "Projects/Brew Service.md",
            "# Brew Service\n\nReference [[Home]].\n",
        );
        write_fixture(
            &root,
            "Research/Service Contract.md",
            "# Service Contract\n\nReference [[Home]].\n",
        );
        let index = crate::index::build_index(&root, None, None).expect("build index");
        fs::remove_dir_all(root).ok();
        index
    }

    fn directional_index() -> SearchIndex {
        let root = unique_temp_dir("directional");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(&root, "A.md", "# A\n\nPoints to [[B]].\n");
        write_fixture(&root, "B.md", "# B\n\nPoints to [[D]].\n");
        write_fixture(&root, "C.md", "# C\n\nAlso points to [[B]].\n");
        write_fixture(&root, "D.md", "# D\n\nTerminal.\n");
        let index = crate::index::build_index(&root, None, None).expect("build index");
        fs::remove_dir_all(root).ok();
        index
    }

    fn repeated_backlink_index() -> SearchIndex {
        let root = unique_temp_dir("repeated-backlink");
        fs::create_dir_all(&root).expect("temp dir");
        write_fixture(&root, "Home.md", "# Home\n\nAnchor.\n");
        write_fixture(
            &root,
            "Mention.md",
            "# Mention\n\nSee [[Home]] and [[Home#Details]].\n",
        );
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
        assert!(matches
            .iter()
            .any(|entry| entry.path == "Projects/Brew Service.md"));
    }

    #[test]
    fn backlinks_keep_all_matching_raw_links_for_each_source() {
        let index = repeated_backlink_index();
        let matches = backlinks(&index, "Home.md", 20).expect("backlinks");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "Mention.md");
        assert_eq!(matches[0].matched_links, vec!["Home", "Home#Details"]);
    }

    #[test]
    fn graph_traversal_walks_neighbors() {
        let index = sample_index();
        let graph = graph_traverse(&index, "Home.md", GraphDirection::Both, 2, 20).expect("graph");
        assert!(graph.nodes.len() >= 3);
        assert!(graph.edges.len() >= 2);
    }

    #[test]
    fn graph_traversal_respects_direction_with_cached_adjacency() {
        let index = directional_index();
        let incoming =
            graph_traverse(&index, "B.md", GraphDirection::Incoming, 1, 20).expect("incoming");
        let outgoing =
            graph_traverse(&index, "B.md", GraphDirection::Outgoing, 1, 20).expect("outgoing");

        let incoming_paths = incoming
            .nodes
            .into_iter()
            .map(|node| node.path)
            .collect::<Vec<_>>();
        let outgoing_paths = outgoing
            .nodes
            .into_iter()
            .map(|node| node.path)
            .collect::<Vec<_>>();

        assert!(incoming_paths.contains(&"A.md".to_string()));
        assert!(incoming_paths.contains(&"C.md".to_string()));
        assert!(!incoming_paths.contains(&"D.md".to_string()));
        assert!(outgoing_paths.contains(&"D.md".to_string()));
        assert!(!outgoing_paths.contains(&"A.md".to_string()));
    }
}
