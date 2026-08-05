//! Federated-recall quality gates (deterministic, CI-runnable).
//!
//! The companion to `deep-obsidian-index/tests/retrieval_eval.rs`. That harness asks "does
//! the local ranker still rank well"; this one asks the question federation adds: **does
//! splitting one corpus across several mounts cost retrieval quality?**
//!
//! The design is the same, and deliberately so:
//!
//! * one deterministic corpus, one fixed gold query set with relevance judgments;
//! * a fake OpenAI-compatible embedding server plus a hand-rolled `pseudo_embedding`, so
//!   both build-time chunk embedding and query-time embedding are reproducible without a
//!   live Ollama;
//! * metrics measured, printed, and asserted against a stated bar.
//!
//! What is different is that the BAR IS RELATIVE. There is no committed
//! `BASELINE_FEDERATED_MRR` to bump, because the baseline is measured in the same process,
//! from the same corpus, on the same queries: the whole corpus in ONE filesystem vault,
//! queried through the local index. Anything that improves or regresses the local ranker
//! moves both numbers together, so the gates keep measuring exactly one thing — the cost of
//! federating.
//!
//! # Why the fake server is copied rather than shared
//!
//! A `tests/` file in one crate cannot import a `tests/` file in another, and
//! `retrieval_eval`'s server is not part of any crate's public API. The copy is deliberate;
//! `pseudo_embedding`, `canonical_token` and `fnv1a` are byte-identical to
//! `deep-obsidian-index/tests/retrieval_eval.rs` so the two harnesses' numbers are
//! comparable. If you change one, change both.
//!
//! # The gates
//!
//! Federated, against the unified baseline over the same corpus:
//!
//! * Recall@20 ≥ baseline − 0.02
//! * Recall@50 ≥ baseline − 0.02
//! * MRR ≥ 0.95 × baseline
//! * nDCG@20 ≥ 0.95 × baseline
//! * every must-find query's expected note within its own cutoff
//!
//! They are measured against the FULL pipeline — fusion plus the final rerank — because that is
//! what a client gets. The rerank-off variant is measured separately and not gated.
//!
//! Do not weaken a gate, the baseline, or the corpus split to make a failure pass. A failure
//! here is a real statement about the retrieval, and the printed per-query table (run with
//! `--nocapture`) is where the mechanism shows up.
//!
//! # What these gates found, and what closed them
//!
//! With rank fusion alone the federated MRR was 0.6000 (two mounts) and 0.4687 (three) against
//! a 0.9104 gate, and the failure was structural rather than unlucky: every mount's rank-0 hit
//! scores the identical `w / (60 + 0)`, no candidate is ever in two mounts' lists because
//! logical paths are namespaced, so nothing is ever summed and the fused order degenerates into
//! a rank-for-rank interleave broken by mount id. The ceiling for that rule is `H_m/m` — 0.75
//! and 0.61 — which is BELOW the gate on any corpus. See
//! `rerank_off_is_measured_so_the_rank_fusion_ceiling_stays_visible`, which keeps that number
//! measured.
//!
//! The final rerank closed it: a mount-independent semantic + lexical rescoring of the fused
//! window brings the federated answer to MRR 0.9583 and nDCG@20 0.9692 on both layouts —
//! exactly the unified baseline, to the last decimal. Recall@20/@50 were 1.0000 throughout,
//! before and after, which is what says fusion was always retrieving the right notes and only
//! their ORDER was wrong.
//!
//! # What each gate can and cannot tell you on this corpus
//!
//! Recall@20 and recall@50 are measured, asserted, and CANNOT FAIL here: thirteen notes at
//! `limit` 50 means every note is retrieved by every query, in both layouts. They are kept
//! because they are the gates that will catch a future federation bug that DROPS a mount's
//! notes — which is the failure mode that matters most — but they are not evidence that this
//! slice's ranking is good. MRR and nDCG@20 are the informative gates.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use deep_obsidian_index::index::tokenize;
use deep_obsidian_server::mcp::{handle_request, AppState};
use deep_obsidian_server::mounts::MountBackends;
use deep_obsidian_server::protocol::JsonRpcRequest;
use deep_obsidian_server::runtime::MountRuntimes;
use deep_obsidian_types::{
    AuthConfig, AutoReindexConfig, EmbeddingConfig, EmbeddingProvider, ExperimentalConfig,
    HttpConfig, MountBackendConfig, MountConfig, ResolvedServiceConfig, StdioMode, TransportMode,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Gates
// ---------------------------------------------------------------------------

/// How far federated recall@k may fall below the unified baseline, in absolute points.
const RECALL_TOLERANCE: f64 = 0.02;

/// The fraction of the baseline's MRR and nDCG@20 the federated run must retain.
const RANK_QUALITY_RETENTION: f64 = 0.95;

/// `limit` every eval query is issued with. The tool's maximum, so recall@50 is measurable
/// from a single call and recall@20 is a prefix of the same list.
const EVAL_LIMIT: usize = 50;

/// Dimensionality of the deterministic pseudo-embedding. Identical to `retrieval_eval`'s.
const EMBEDDING_DIM: usize = 64;

// ---------------------------------------------------------------------------
// Deterministic pseudo-semantic embedding (copied from retrieval_eval.rs)
// ---------------------------------------------------------------------------

/// Synonym folding: paraphrases with DISJOINT surface vocabulary map onto a shared
/// canonical token, so a genuine "dense paraphrase win" exists in the corpus below.
fn canonical_token(token: &str) -> &str {
    match token {
        "car" | "automobile" | "vehicle" | "motorcar" => "concept_car",
        "physician" | "doctor" | "clinician" => "concept_physician",
        "ailment" | "illness" | "malady" | "sickness" => "concept_illness",
        "remedy" | "cure" | "treatment" => "concept_remedy",
        other => other,
    }
}

/// FNV-1a (64-bit). Fixed constants, so it is stable across processes.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Deterministic text -> fixed-dim L2-normalized vector.
fn pseudo_embedding(text: &str) -> Vec<f64> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for token in tokenize(text) {
        *counts
            .entry(canonical_token(&token).to_string())
            .or_insert(0) += 1;
    }

    let mut vector = vec![0.0_f64; EMBEDDING_DIM];
    for (token, count) in &counts {
        let hash = fnv1a(token.as_bytes());
        let bucket = (hash % EMBEDDING_DIM as u64) as usize;
        let sign = if (hash >> 63) & 1 == 1 { 1.0 } else { -1.0 };
        vector[bucket] += sign * (*count as f64);
    }

    let norm = vector.iter().map(|value| value * value).sum::<f64>().sqrt();
    if norm == 0.0 {
        let mut fallback = vec![0.0_f64; EMBEDDING_DIM];
        fallback[0] = 1.0;
        return fallback;
    }
    vector.iter().map(|value| value / norm).collect()
}

fn spawn_pseudo_embedding_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind eval embedding server");
    let address = listener.local_addr().expect("server address");
    // Detached: serves an arbitrary number of requests. Process exit reaps it.
    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => handle_embedding_request(stream),
                Err(_) => break,
            }
        }
    });
    format!("http://{address}")
}

fn handle_embedding_request(mut stream: TcpStream) {
    let mut buffer = Vec::new();
    let mut header_end = None;
    while header_end.is_none() {
        let mut chunk = [0_u8; 1024];
        let read = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        buffer.extend_from_slice(&chunk[..read]);
        header_end = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    }
    let header_end = header_end.expect("request headers") + 4;
    let headers = String::from_utf8_lossy(&buffer[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|value| value.trim().parse::<usize>().expect("content length"))
        })
        .expect("content length header");
    while buffer.len() < header_end + content_length {
        let mut chunk = [0_u8; 1024];
        let read = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        buffer.extend_from_slice(&chunk[..read]);
    }

    let body = &buffer[header_end..header_end + content_length];
    let payload: Value = serde_json::from_slice(body).expect("json request");
    let inputs = payload
        .get("input")
        .and_then(Value::as_array)
        .expect("input array")
        .iter()
        .map(|value| value.as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();

    let data = inputs
        .iter()
        .enumerate()
        .map(|(index, text)| {
            json!({
                "index": index,
                "embedding": pseudo_embedding(text),
            })
        })
        .collect::<Vec<_>>();
    let response_body = json!({ "data": data }).to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        response_body.len(),
        response_body
    );
    let _ = stream.write_all(response.as_bytes());
}

// ---------------------------------------------------------------------------
// The corpus, and how it splits
// ---------------------------------------------------------------------------

/// Which mount a note lands on in the split runs.
///
/// # The split is chosen, not arbitrary, and two constraints drive it
///
/// 1. **Wikilink pairs stay whole.** The local ranker applies a graph-proximity rerank over
///    its fused candidate pool, so separating two linked notes deletes that edge from BOTH
///    indexes. The federated run would then lose quality because of the split's shape rather
///    than because of fusion, and the gate would be measuring the wrong thing. So
///    `Engineering/Zephyrus` + `Engineering/Ingestion Pipeline` stay together, and so do
///    `Wiki/Capital Question` + `Wiki/Capital Answer`.
/// 2. **Some answers must live on the MINORITY mount**, or federation is never tested: a
///    split where the root mount answers everything would pass by never needing the other
///    mount at all. Four of the twelve gold queries are answered from `Minority`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    /// The root mount in every run.
    Root,
    /// A second mount, at `Team`. Holds the two paraphrase pairs and the linked Wiki pair.
    Minority,
    /// A third mount, at `Archive`, used only by the three-mount variant. Holds notes with
    /// no wikilinks, so promoting them to their own mount deletes no edge.
    Archive,
}

struct CorpusNote {
    path: &'static str,
    placement: Placement,
    content: String,
}

/// The eval corpus: `retrieval_eval`'s notes, minus its oversized `Maintenance Handbook`
/// fixture (which exercises small-to-big CHUNKING, not federation) plus a small `Runbook` in
/// its place.
///
/// Keeping the text otherwise identical is what makes the two harnesses comparable: this
/// file's unified baseline measures MRR 0.9583 against `retrieval_eval`'s committed
/// `BASELINE_HYBRID_MRR` of 0.958, so two independently written harnesses agree on the local
/// ranker to three decimals. That agreement is the cross-check that a failing federated gate
/// is about federation rather than about this harness.
fn corpus() -> Vec<CorpusNote> {
    let note = |path: &'static str, placement: Placement, content: &str| CorpusNote {
        path,
        placement,
        content: content.to_string(),
    };
    vec![
        // Linked pair, kept together. Distinctive identifiers give BM25 exact anchors.
        note(
            "Engineering/Zephyrus.md",
            Placement::Root,
            "# Zephyrus Service\n\
             \n\
             ## Overview\n\
             The Zephyrus7 daemon at Quaalbrook handles ingestion.\n\
             \n\
             ## Startup\n\
             Run the bootstrap routine before serving traffic.\n\
             \n\
             ```bash\n\
             zephyrus7 --bootstrap --port 8080\n\
             ```\n\
             \n\
             ### Notes\n\
             See [[Engineering/Ingestion Pipeline]] for the downstream stages.\n",
        ),
        note(
            "Engineering/Ingestion Pipeline.md",
            Placement::Root,
            "# Ingestion Pipeline\n\
             \n\
             ## Stages\n\
             The pipeline batches records and writes them to the warehouse.\n\
             \n\
             ## Backpressure\n\
             When the queue saturates the pipeline applies backpressure.\n\
             \n\
             Upstream is [[Engineering/Zephyrus]].\n",
        ),
        note(
            "Ops/Error Codes.md",
            Placement::Root,
            "# Error Codes\n\
             \n\
             | Code | Meaning | Action |\n\
             | --- | --- | --- |\n\
             | ERR_4471 | disk quota exceeded | free space |\n\
             | ERR_5582 | auth token expired | refresh token |\n\
             \n\
             Escalate persistent failures to the on-call rotation.\n",
        ),
        // Paraphrase twins with disjoint surface vocabulary, both on the minority mount.
        note(
            "Library/Automobile Maintenance.md",
            Placement::Minority,
            "# Automobile Maintenance\n\
             \n\
             Routine upkeep of an automobile keeps the vehicle dependable.\n\
             Inspect the motorcar regularly to avoid breakdowns.\n",
        ),
        note(
            "Library/Car Care Basics.md",
            Placement::Minority,
            "# Car Care Basics\n\
             \n\
             Looking after your car keeps the automobile reliable over the years.\n\
             A well maintained vehicle rarely strands you on the road.\n",
        ),
        note(
            "Health/Seeing a Doctor.md",
            Placement::Minority,
            "# Seeing a Doctor\n\
             \n\
             When an illness lingers, visit a doctor for a proper diagnosis.\n\
             A physician can prescribe the right remedy for the ailment.\n",
        ),
        note(
            "Health/Clinician Visits.md",
            Placement::Minority,
            "# Clinician Visits\n\
             \n\
             A clinician evaluates the sickness and recommends a treatment.\n\
             Trust the physician to choose an effective cure for the malady.\n",
        ),
        note(
            "Notes/Garden Planning.md",
            Placement::Archive,
            "# Garden Planning\n\
             \n\
             Plan the garden layout, prepare the soil, choose seeds, and water beds daily.\n\
             A good garden plan balances sunlight, drainage, and seasonal planting.\n",
        ),
        note(
            "Notes/Compost Trivia.md",
            Placement::Archive,
            "# Compost Trivia\n\
             \n\
             The rare term Bokashi appears here once and nowhere else in the vault.\n",
        ),
        note(
            "People/Octavia Hartwell.md",
            Placement::Archive,
            "# Octavia Hartwell\n\
             \n\
             Octavia Hartwell leads the Threnody research group.\n\
             Contact Octavia about the Threnody roadmap.\n",
        ),
        // Linked pair, kept together on the minority mount.
        note(
            "Wiki/Capital Question.md",
            Placement::Minority,
            "# Capital Question\n\
             \n\
             For the seat of government see [[Wiki/Capital Answer]].\n",
        ),
        note(
            "Wiki/Capital Answer.md",
            Placement::Minority,
            "# Capital Answer\n\
             \n\
             The administrative capital city is Lindholm, home to the parliament.\n",
        ),
        note(
            "Manuals/Runbook.md",
            Placement::Root,
            "# Runbook\n\
             \n\
             ## Rotation\n\
             The on-call rotation escalates a paging alert to the duty engineer.\n\
             \n\
             ## Recovery\n\
             Restore the snapshot, replay the write-ahead log, then verify checksums.\n",
        ),
    ]
}

/// How a run distributes the corpus across mounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// The whole corpus in ONE filesystem vault at the root. The baseline.
    Unified,
    /// Root mount + a `Team` mount holding [`Placement::Minority`].
    TwoMounts,
    /// Root mount + `Team` + an `Archive` mount holding [`Placement::Archive`].
    ThreeMounts,
}

impl Layout {
    /// The logical vault path a corpus note appears at under this layout.
    fn logical_path(self, note: &CorpusNote) -> String {
        match (self, note.placement) {
            (Layout::Unified, _) | (_, Placement::Root) => note.path.to_string(),
            (Layout::TwoMounts, Placement::Minority)
            | (Layout::ThreeMounts, Placement::Minority) => format!("Team/{}", note.path),
            // Two mounts only: the archive notes stay on the root mount.
            (Layout::TwoMounts, Placement::Archive) => note.path.to_string(),
            (Layout::ThreeMounts, Placement::Archive) => format!("Archive/{}", note.path),
        }
    }

    /// Which vault directory a note is written into: `""` root, `"Team"`, `"Archive"`.
    fn vault_of(self, note: &CorpusNote) -> &'static str {
        match (self, note.placement) {
            (Layout::Unified, _) | (_, Placement::Root) => "",
            (_, Placement::Minority) if self != Layout::Unified => "Team",
            (Layout::ThreeMounts, Placement::Archive) => "Archive",
            _ => "",
        }
    }
}

// ---------------------------------------------------------------------------
// Gold query set
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct GoldQuery {
    /// What this query exercises, for the printed report.
    intent: &'static str,
    query: &'static str,
    /// The relevant note, as its path appears in the UNIFIED corpus. The harness maps it
    /// through [`Layout::logical_path`] per run.
    expected: &'static str,
    /// When set, the rank the expected note MUST be within, in every layout. A must-find
    /// query is one where an agent that does not see this note in the first handful of
    /// results will act on the wrong information, so a soft aggregate metric is not enough.
    must_find_within: Option<usize>,
}

/// Twelve queries with relevance judgments, four of them must-find.
fn gold_queries() -> Vec<GoldQuery> {
    let query = |intent: &'static str,
                 query: &'static str,
                 expected: &'static str,
                 must_find_within: Option<usize>| GoldQuery {
        intent,
        query,
        expected,
        must_find_within,
    };
    vec![
        query(
            "bm25 exact identifier",
            "Zephyrus7 Quaalbrook",
            "Engineering/Zephyrus.md",
            // A unique identifier: if an exact-token query cannot find its one note, recall
            // is broken outright rather than merely worse.
            Some(5),
        ),
        query(
            "bm25 exact error code",
            "ERR_4471 disk quota",
            "Ops/Error Codes.md",
            Some(5),
        ),
        query(
            "bm25 exact proper noun",
            "Octavia Hartwell Threnody",
            "People/Octavia Hartwell.md",
            Some(5),
        ),
        query(
            "dense paraphrase (car)",
            "keeping a motorcar dependable",
            "Library/Car Care Basics.md",
            None,
        ),
        query(
            "recall (medical synonyms)",
            "a clinician picks a cure for the malady",
            "Health/Clinician Visits.md",
            None,
        ),
        query(
            "fusion broad-vocab vs rare decoy",
            "planning a garden layout with soil and seeds",
            "Notes/Garden Planning.md",
            None,
        ),
        query(
            "cross-link downstream stages",
            "downstream ingestion pipeline stages warehouse",
            "Engineering/Ingestion Pipeline.md",
            None,
        ),
        query(
            // The answer lives on the MINORITY mount, and the query names it exactly. This
            // is the query that fails outright if federation ever answers from one mount.
            "minority-mount exact answer",
            "administrative capital city parliament Lindholm",
            "Wiki/Capital Answer.md",
            Some(5),
        ),
        query(
            "code block bootstrap command",
            "bootstrap routine before serving traffic",
            "Engineering/Zephyrus.md",
            None,
        ),
        query(
            "table row meaning",
            "auth token expired refresh",
            "Ops/Error Codes.md",
            None,
        ),
        query(
            "concept backpressure queue",
            "queue saturates backpressure",
            "Engineering/Ingestion Pipeline.md",
            None,
        ),
        query(
            "paraphrase (illness lingers)",
            "what to do when an ailment will not go away",
            "Health/Seeing a Doctor.md",
            None,
        ),
    ]
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy, PartialEq)]
struct Aggregate {
    recall_at_20: f64,
    recall_at_50: f64,
    mrr: f64,
    ndcg_at_20: f64,
}

/// One query's outcome, kept so the printed report can show WHERE a rank came from.
struct QueryOutcome {
    intent: &'static str,
    query: &'static str,
    expected: String,
    /// 1-based rank of the expected note, or `None` for a miss.
    rank: Option<usize>,
    /// The 0-based rank the expected note held in ITS OWN mount's list, when the payload
    /// reported one. This is the diagnostic that separates "the mount ranked it badly" from
    /// "fusion put another mount's hit in front of it".
    mount_rank: Option<usize>,
    top_path: String,
}

/// Discounted gain of a single relevant document at 1-based `rank`, capped at `k`.
///
/// With exactly one relevant note per query the ideal DCG is 1.0 (that note at rank 1), so
/// nDCG@k reduces to `1 / log2(rank + 1)`.
fn ndcg_at(rank: Option<usize>, k: usize) -> f64 {
    match rank {
        Some(rank) if rank <= k => 1.0 / ((rank + 1) as f64).log2(),
        _ => 0.0,
    }
}

fn score(outcomes: &[QueryOutcome], label: &str) -> Aggregate {
    let total = outcomes.len() as f64;
    let mut hits_at_20 = 0.0;
    let mut hits_at_50 = 0.0;
    let mut reciprocal_rank_sum = 0.0;
    let mut ndcg_sum = 0.0;

    println!("\n=== {label} — per-query ===");
    for outcome in outcomes {
        if let Some(rank) = outcome.rank {
            if rank <= 20 {
                hits_at_20 += 1.0;
            }
            if rank <= 50 {
                hits_at_50 += 1.0;
            }
            reciprocal_rank_sum += 1.0 / rank as f64;
        }
        ndcg_sum += ndcg_at(outcome.rank, 20);
        println!(
            "  [{:<34}] rank={:<5} mountRank={:<5} top1={:<34} q={:?}",
            outcome.intent,
            outcome
                .rank
                .map(|rank| rank.to_string())
                .unwrap_or_else(|| "miss".to_string()),
            outcome
                .mount_rank
                .map(|rank| rank.to_string())
                .unwrap_or_else(|| "-".to_string()),
            outcome.top_path,
            outcome.query,
        );
    }

    let aggregate = Aggregate {
        recall_at_20: hits_at_20 / total,
        recall_at_50: hits_at_50 / total,
        mrr: reciprocal_rank_sum / total,
        ndcg_at_20: ndcg_sum / total,
    };
    println!(
        "--- {label} — aggregate: recall@20={:.4} recall@50={:.4} MRR={:.4} nDCG@20={:.4} ---",
        aggregate.recall_at_20, aggregate.recall_at_50, aggregate.mrr, aggregate.ndcg_at_20
    );
    println!(
        "--- {label} — mean interleaving offset: {:.4} ---",
        interleaving_offset(outcomes)
    );
    aggregate
}

/// How far the expected note sits from where its OWN mount ranked it, averaged over the
/// queries that found it.
///
/// The diagnostic that separates the two possible causes of a bad federated rank. A query
/// whose answer sits at `mountRank` 0 and fused rank 3 was not ranked badly by its mount —
/// something put two foreign hits in front of it. Zero on the unified baseline (there is one
/// mount and `mountRank` is absent), so it reads as "the cost of splitting".
///
/// SIGNED, and negative values are meaningful: the final rerank scores candidates against the
/// query rather than against their position in a mount's list, so it can pull a hit ABOVE the
/// position its own mount gave it — past mounts that ranked their own hits higher. A mean
/// below zero says the rerank is not merely undoing interleaving but out-ranking the per-mount
/// orderings, which is what a mount-independent scorer is supposed to do.
fn interleaving_offset(outcomes: &[QueryOutcome]) -> f64 {
    let measurable: Vec<f64> = outcomes
        .iter()
        .filter_map(|outcome| match (outcome.rank, outcome.mount_rank) {
            (Some(rank), Some(mount_rank)) => Some(rank as f64 - (mount_rank as f64 + 1.0)),
            _ => None,
        })
        .collect();
    if measurable.is_empty() {
        return 0.0;
    }
    measurable.iter().sum::<f64>() / measurable.len() as f64
}

/// The best MRR pure rank blending can reach with `mounts` mounts: the harmonic number
/// `H_m / m`.
///
/// # Why this is worth printing next to a failure
///
/// It is a property of the FUSION RULE, not of this corpus. With `m` mounts, a query whose
/// answer is its own mount's rank-0 hit still lands behind the rank-0 hit of every mount
/// that wins the tie-break — and every mount's rank-0 hit scores exactly `w/(60 + 0)`,
/// because logical paths are namespaced so no candidate ever appears in two lists and no
/// contribution is ever summed. So the answer's fused rank is its mount's position in the
/// tie-break order, and averaging `1/position` over a uniform spread of answers across
/// mounts gives `H_m / m`: 0.75 for two mounts, 0.611 for three, 0.5 for four.
///
/// A measured MRR BELOW this ceiling means the answers are not uniformly spread — which is
/// this corpus's case, since the mount holding most answers (`vault`) sorts LAST in the
/// mount-id tie-break and is therefore demoted on every query it answers.
fn rank_blending_mrr_ceiling(mounts: usize) -> f64 {
    let mounts = mounts.max(1);
    (1..=mounts).map(|rank| 1.0 / rank as f64).sum::<f64>() / mounts as f64
}

/// The best nDCG@20 pure rank blending can reach with `mounts` mounts.
///
/// The same argument as [`rank_blending_mrr_ceiling`], discounted instead of reciprocal:
/// each answer lands at the position its mount holds in the tie-break order, contributing
/// `1 / log2(position + 1)`. 0.8155 for two mounts, 0.7103 for three, 0.6404 for four.
fn rank_blending_ndcg_ceiling(mounts: usize) -> f64 {
    let mounts = mounts.max(1);
    (1..=mounts)
        .map(|rank| 1.0 / ((rank + 1) as f64).log2())
        .sum::<f64>()
        / mounts as f64
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One or more temp vaults plus an index dir, wired into an `AppState`.
struct Harness {
    base: PathBuf,
    layout: Layout,
    state: AppState,
}

fn unique_base(label: &str) -> PathBuf {
    let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "deep-obsidian-fedeval-{label}-{}-{id}",
        std::process::id()
    ))
}

fn write_note(root: &Path, relative: &str, content: &str) {
    let absolute = root.join(relative);
    if let Some(parent) = absolute.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(&absolute, content).expect("write note");
}

fn eval_embedding(base_url: &str) -> EmbeddingConfig {
    EmbeddingConfig {
        provider: Some(EmbeddingProvider::OpenAiCompatible),
        model: Some("pseudo-eval-model".to_string()),
        base_url: Some(base_url.to_string()),
        api_key_ref: None,
        max_chars: None,
        max_input_tokens: None,
        context_tokens: None,
        // Generic (non-qwen3) eval model, so queries stay plain and the rankings match
        // `retrieval_eval`'s.
        query_instruction: None,
    }
}

impl Harness {
    /// Build the corpus under `layout` and bring a server up over it.
    ///
    /// `weights` overrides a mount's `recallWeight` by mount id; empty means every mount
    /// keeps the default 1.0.
    async fn new(label: &str, layout: Layout, weights: &[(&str, f64)]) -> Self {
        Self::build(label, layout, weights, &[], true).await
    }

    /// The same harness with the final rerank switched OFF, i.e. answers in pure rank-fusion
    /// order. What `federatedRerank: false` gives an operator, and the only way to observe the
    /// fusion stage's own behaviour — including `recallWeight`, which is a fusion-stage input.
    async fn without_rerank(label: &str, layout: Layout, weights: &[(&str, f64)]) -> Self {
        Self::build(label, layout, weights, &[], false).await
    }

    /// The same harness with `broken` mounts pointed at directories that do not exist, so
    /// they fail their index refresh and every query against them.
    ///
    /// A missing vault directory rather than a killed process or a network fault: it is the
    /// one unavailability that is reproducible on every platform with no timing in it, and it
    /// reaches the tool layer through exactly the same path a real outage would — the mount's
    /// `fresh_snapshot` returns an error and federation records it.
    async fn with_broken_mounts(
        label: &str,
        layout: Layout,
        weights: &[(&str, f64)],
        broken: &[&str],
    ) -> Self {
        Self::build(label, layout, weights, broken, true).await
    }

    async fn build(
        label: &str,
        layout: Layout,
        weights: &[(&str, f64)],
        broken: &[&str],
        rerank: bool,
    ) -> Self {
        let base = unique_base(label);
        let _ = fs::remove_dir_all(&base);
        let index_dir = base.join("index");
        fs::create_dir_all(&index_dir).expect("index dir");

        let mut vaults: Vec<&'static str> = vec![""];
        for note in corpus() {
            let vault = layout.vault_of(&note);
            if !vaults.contains(&vault) {
                vaults.push(vault);
            }
            let root = if vault.is_empty() {
                base.join("root-vault")
            } else {
                base.join(format!("{vault}-vault"))
            };
            fs::create_dir_all(&root).expect("vault dir");
            write_note(&root, note.path, &note.content);
        }

        let weight_of = |id: &str| {
            weights
                .iter()
                .find(|(candidate, _)| *candidate == id)
                .map(|(_, weight)| *weight)
        };
        let mounts: Vec<MountConfig> = vaults
            .iter()
            .map(|vault| {
                let (id, mount_at, dir) = if vault.is_empty() {
                    ("vault", String::new(), base.join("root-vault"))
                } else {
                    (
                        // Mount ids are the lowercased folder, which also fixes the
                        // canonical fusion order: "archive" < "team" < "vault".
                        if *vault == "Team" { "team" } else { "archive" },
                        vault.to_string(),
                        base.join(format!("{vault}-vault")),
                    )
                };
                let vault_path = if broken.contains(&id) {
                    base.join("no-such-vault").join(id)
                } else {
                    dir
                };
                MountConfig {
                    id: id.to_string(),
                    mount_at,
                    backend: MountBackendConfig::Filesystem {
                        vault_path,
                        index_dir: None,
                    },
                    recall_weight: weight_of(id),
                }
            })
            .collect();

        let config = ResolvedServiceConfig {
            federated_rerank: rerank,
            vault_path: base.join("root-vault"),
            mounts,
            experimental: ExperimentalConfig {
                multi_vault: layout != Layout::Unified,
                ..ExperimentalConfig::default()
            },
            index_dir,
            transport: TransportMode::Http,
            stdio_mode: StdioMode::Auto,
            http: HttpConfig {
                host: "127.0.0.1".to_string(),
                port: 0,
                mcp_path: "/mcp".to_string(),
                health_path: "/healthz".to_string(),
            },
            auto_reindex: AutoReindexConfig {
                enabled: false,
                debounce_ms: 0,
                interval_ms: 0,
            },
            embedding: eval_embedding(&spawn_pseudo_embedding_server()),
            artifact_embedding: EmbeddingConfig::default(),
            auth: AuthConfig::default(),
            config_file_path: None,
        };

        let backends = MountBackends::build(&config);
        let (runtimes, _auto_reindex) = MountRuntimes::bootstrap(&config, &backends)
            .await
            .expect("bootstrap runtimes");
        let state = AppState::with_backends(config, runtimes, &backends);
        let harness = Self {
            base,
            layout,
            state,
        };
        if broken.is_empty() {
            harness.assert_every_mount_is_embedding_backed().await;
        }
        harness
    }

    /// Every mount's index must really be embedding-backed.
    ///
    /// # Why this is asserted rather than assumed
    ///
    /// Without a reachable embedding backend the index silently falls back to a sparse
    /// term-overlap semantic stage. Every query still answers, every gate still computes,
    /// and the numbers describe a retrieval pipeline that is not the one being shipped. A
    /// green test over a degraded index is the one failure mode this harness could have that
    /// nobody would notice, so the propagation of `embedding` into EVERY mount's runtime
    /// config is checked before a single metric is measured.
    async fn assert_every_mount_is_embedding_backed(&self) {
        for entry in self.state.runtimes.entries() {
            let snapshot = entry
                .runtime
                .fresh_snapshot("federation_eval")
                .await
                .expect("index snapshot");
            assert_eq!(
                snapshot.index.semantic_backend,
                deep_obsidian_index::index::SemanticBackend::Embedding,
                "mount '{}' is not embedding-backed: the eval would measure a degraded pipeline",
                entry.id
            );
            assert_eq!(
                snapshot.index.embedding_dimensions,
                Some(EMBEDDING_DIM),
                "mount '{}' embedded at the wrong dimension",
                entry.id
            );
        }
    }

    async fn tool_call(&self, name: &str, arguments: Value) -> Value {
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        });
        let parsed: JsonRpcRequest = serde_json::from_value(payload).expect("request");
        match handle_request(self.state.clone(), parsed).await {
            Ok(Some(response)) => response,
            Ok(None) => panic!("tools/call produced no response"),
            Err(error) => serde_json::to_value(&error).expect("error response"),
        }
    }

    /// The `structuredContent` of a successful tool call.
    async fn structured(&self, name: &str, arguments: Value) -> Value {
        let response = self.tool_call(name, arguments).await;
        response
            .get("result")
            .and_then(|result| result.get("structuredContent"))
            .cloned()
            .unwrap_or_else(|| panic!("expected a successful {name} call, got {response}"))
    }

    /// Run the whole gold set through unscoped `hybrid_search` and score it.
    async fn run_gold_set(&self, label: &str) -> (Aggregate, Vec<QueryOutcome>) {
        let notes = corpus();
        let mut outcomes = Vec::new();
        for gold in gold_queries() {
            let note = notes
                .iter()
                .find(|note| note.path == gold.expected)
                .expect("every gold query names a corpus note");
            let expected = self.layout.logical_path(note);
            let payload = self
                .structured(
                    "hybrid_search",
                    json!({"query": gold.query, "limit": EVAL_LIMIT, "includeText": false}),
                )
                .await;
            let matches = payload["matches"].as_array().cloned().unwrap_or_default();
            // Rank at NOTE level: a note that matched on several chunks occupies several
            // consecutive rows, and the first of them is its rank. Matching
            // `retrieval_eval`, which scores at note-path level for the same reason.
            let mut seen: Vec<&str> = Vec::new();
            let mut rank = None;
            let mut mount_rank = None;
            for item in &matches {
                let path = item["path"].as_str().unwrap_or_default();
                if seen.contains(&path) {
                    continue;
                }
                seen.push(path);
                if path == expected {
                    rank = Some(seen.len());
                    mount_rank = item
                        .get("mountRank")
                        .and_then(Value::as_u64)
                        .map(|r| r as usize);
                    break;
                }
            }
            outcomes.push(QueryOutcome {
                intent: gold.intent,
                query: gold.query,
                expected,
                rank,
                mount_rank,
                top_path: matches
                    .first()
                    .and_then(|item| item["path"].as_str())
                    .unwrap_or("<none>")
                    .to_string(),
            });
        }
        let aggregate = score(&outcomes, label);
        (aggregate, outcomes)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

/// Assert the four gates, reporting the measured numbers on failure.
///
/// Collected into one report rather than four separate asserts so a run that regresses on
/// several metrics says so in one go — chasing them one `cargo test` at a time hides how
/// much moved.
///
/// # If MRR and nDCG@20 are the failures, read this before touching anything
///
/// They are the two TOP-HEAVY metrics, and pure rank blending cannot preserve them across a
/// split corpus. See [`rank_blending_mrr_ceiling`] for the arithmetic: every mount's rank-0
/// hit scores the identical `w/(60 + 0)`, no candidate is ever in two lists (paths are
/// namespaced), so nothing is ever summed and the fused order is a rank-for-rank interleave
/// decided by the mount-id tie-break. The answer to a query is therefore pushed down by one
/// position per mount that is not holding it, however good or bad those mounts' hits are.
///
/// The remedy is NOT in this function, and not in the corpus split. It is the one step the
/// accepted algorithm defers: a final rerank that scores the fused top-`limit` against the
/// query with a SINGLE ranker, which is the only thing that can decide whether mount A's
/// best hit really beats mount B's. That step needs every hit's text in one place, which is
/// the cross-backend content flow this slice is forbidden to introduce. Recall@20 and
/// recall@50 hold at the baseline exactly because they are insensitive to that reordering:
/// the right notes ARE retrieved, and only their order is wrong.
fn assert_gates(label: &str, baseline: Aggregate, federated: Aggregate, mounts: usize) {
    let mut failures: Vec<String> = Vec::new();
    let recall_gate = |name: &str, measured: f64, base: f64, failures: &mut Vec<String>| {
        if measured + 1e-9 < base - RECALL_TOLERANCE {
            failures.push(format!(
                "{name}: federated {measured:.4} < baseline {base:.4} - {RECALL_TOLERANCE}"
            ));
        }
    };
    recall_gate(
        "recall@20",
        federated.recall_at_20,
        baseline.recall_at_20,
        &mut failures,
    );
    recall_gate(
        "recall@50",
        federated.recall_at_50,
        baseline.recall_at_50,
        &mut failures,
    );
    let retention_gate = |name: &str, measured: f64, base: f64, failures: &mut Vec<String>| {
        let bar = RANK_QUALITY_RETENTION * base;
        if measured + 1e-9 < bar {
            failures.push(format!(
                "{name}: federated {measured:.4} < {RANK_QUALITY_RETENTION} x baseline {base:.4} = {bar:.4}"
            ));
        }
    };
    retention_gate("MRR", federated.mrr, baseline.mrr, &mut failures);
    retention_gate(
        "nDCG@20",
        federated.ndcg_at_20,
        baseline.ndcg_at_20,
        &mut failures,
    );
    assert!(
        failures.is_empty(),
        "{label} failed {} of 4 recall gates:\n  {}\n\n\
         baseline  ({mounts} mounts' worth of corpus in ONE vault): recall@20={:.4} recall@50={:.4} MRR={:.4} nDCG@20={:.4}\n\
         federated ({mounts} mounts):                              recall@20={:.4} recall@50={:.4} MRR={:.4} nDCG@20={:.4}\n\n\
         MECHANISM: pure rank blending. Every mount's rank-0 hit scores the same {:.6} \
         (= weight / (60 + 0)), and no candidate is ever in two mounts' lists because logical \
         paths are namespaced -- so no RRF contribution is ever summed and the fused order is a \
         rank-for-rank INTERLEAVE broken by mount id. An answer therefore lands at the position \
         its own mount holds in that tie-break order, however good or bad the other mounts' \
         hits are. Run with --nocapture for the per-query table: a mountRank of 0 next to a \
         fused rank of {mounts} is this mechanism and nothing else.\n\n\
         THE GATES ARE UNREACHABLE, NOT MERELY MISSED. Averaging over a UNIFORM spread of \
         answers across {mounts} mounts -- the best case for this rule, on any corpus -- gives \
         MRR = H_{mounts}/{mounts} = {:.4} (gate {:.4}) and nDCG@20 = {:.4} (gate {:.4}). Both \
         ceilings are BELOW their gate, so no corpus, no split, no mount naming and no weight \
         assignment can pass either one under algorithm step 2 while step 6 (the final rerank) \
         is deferred. Gates 3 and 4 are incompatible with the specified fusion rule; that is a \
         specification inconsistency to resolve, not a measurement to retake.\n\n\
         This run measures {:.4} / {:.4}, below even those ceilings, because the answers are \
         NOT spread evenly: the mount holding most of them sorts last in the mount-id \
         tie-break. That sensitivity is itself the finding -- a fused top-1 that moves when a \
         mount is renamed is a tie-break carrying weight it was never meant to carry.\n\n\
         DO NOT lower a gate, retune the corpus split, rename a mount, or reorder the \
         tie-break to make this pass. Recall@20/@50 sit exactly at the baseline, which says the \
         right notes ARE being retrieved and only their ORDER is wrong. Fixing the order needs \
         one ranker scoring the fused top-limit against the query, which requires every hit's \
         text in one place -- the cross-backend content flow this slice must not introduce.",
        failures.len(),
        failures.join("\n  "),
        baseline.recall_at_20,
        baseline.recall_at_50,
        baseline.mrr,
        baseline.ndcg_at_20,
        federated.recall_at_20,
        federated.recall_at_50,
        federated.mrr,
        federated.ndcg_at_20,
        1.0 / 60.0,
        rank_blending_mrr_ceiling(mounts),
        RANK_QUALITY_RETENTION * baseline.mrr,
        rank_blending_ndcg_ceiling(mounts),
        RANK_QUALITY_RETENTION * baseline.ndcg_at_20,
        federated.mrr,
        federated.ndcg_at_20,
    );
}

/// Every must-find query's expected note must be inside its cutoff.
fn assert_must_find(label: &str, outcomes: &[QueryOutcome]) {
    let notes = corpus();
    let mut failures = Vec::new();
    for (gold, outcome) in gold_queries().iter().zip(outcomes) {
        let Some(cutoff) = gold.must_find_within else {
            continue;
        };
        debug_assert!(notes.iter().any(|note| note.path == gold.expected));
        let within = outcome.rank.is_some_and(|rank| rank <= cutoff);
        if !within {
            failures.push(format!(
                "'{}' ({}): expected {} within rank {cutoff}, got {}",
                gold.query,
                gold.intent,
                outcome.expected,
                outcome
                    .rank
                    .map(|rank| rank.to_string())
                    .unwrap_or_else(|| "miss".to_string()),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{label}: {} must-find query/queries missed their cutoff:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// The gates
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn federated_recall_matches_the_unified_baseline_across_two_mounts() {
    let unified = Harness::new("baseline", Layout::Unified, &[]).await;
    let (baseline, baseline_outcomes) = unified.run_gold_set("BASELINE (one vault)").await;
    assert_must_find("baseline", &baseline_outcomes);

    let split = Harness::new("two-mounts", Layout::TwoMounts, &[]).await;
    let (federated, federated_outcomes) = split.run_gold_set("FEDERATED (two mounts)").await;

    assert_gates("two mounts", baseline, federated, 2);
    assert_must_find("two mounts", &federated_outcomes);
}

#[tokio::test(flavor = "multi_thread")]
async fn federated_recall_matches_the_unified_baseline_across_three_mounts() {
    let unified = Harness::new("baseline3", Layout::Unified, &[]).await;
    let (baseline, _) = unified.run_gold_set("BASELINE (one vault)").await;

    let split = Harness::new("three-mounts", Layout::ThreeMounts, &[]).await;
    let (federated, federated_outcomes) = split.run_gold_set("FEDERATED (three mounts)").await;

    assert_gates("three mounts", baseline, federated, 3);
    assert_must_find("three mounts", &federated_outcomes);
}

/// INFORMATIONAL, not a gate: what the same corpus measures with the final rerank switched off.
///
/// # Why this is measured and printed but not asserted
///
/// It is the number that justifies the rerank existing. Pure rank fusion cannot rank across
/// mounts — every mount's rank-0 hit scores the identical `w / (60 + 0)`, and because logical
/// paths are namespaced no candidate is ever in two mounts' lists, so nothing is ever summed
/// and the fused order collapses into a rank-for-rank interleave broken by mount id. The best
/// MRR that rule can reach, averaged over a uniform spread of answers across `m` mounts, is
/// `H_m/m`: 0.75 for two mounts, 0.61 for three, against gates of ~0.91. Those ceilings sit
/// BELOW the gate, so `federatedRerank: false` cannot pass the recall gates on any corpus.
///
/// Keeping it visible and unasserted is the honest arrangement: an operator can turn the rerank
/// off, so the cost of doing so should be recorded rather than argued about, and pinning a
/// number nobody intends to improve would just be a second baseline to maintain.
#[tokio::test(flavor = "multi_thread")]
async fn rerank_off_is_measured_so_the_rank_fusion_ceiling_stays_visible() {
    let unified = Harness::new("baseline-rrf", Layout::Unified, &[]).await;
    let (baseline, _) = unified.run_gold_set("BASELINE (one vault)").await;

    for (label, layout, mounts) in [
        ("FEDERATED, RERANK OFF (two mounts)", Layout::TwoMounts, 2),
        (
            "FEDERATED, RERANK OFF (three mounts)",
            Layout::ThreeMounts,
            3,
        ),
    ] {
        let split = Harness::without_rerank("rrf-only", layout, &[]).await;
        let (measured, _) = split.run_gold_set(label).await;
        println!(
            "--- {label} — rank-fusion ceilings: MRR H_{mounts}/{mounts}={:.4} nDCG@20={:.4}; \
             gates MRR={:.4} nDCG@20={:.4} ---",
            rank_blending_mrr_ceiling(mounts),
            rank_blending_ndcg_ceiling(mounts),
            RANK_QUALITY_RETENTION * baseline.mrr,
            RANK_QUALITY_RETENTION * baseline.ndcg_at_20,
        );

        // Recall is unaffected -- fusion retrieves the right notes, it just cannot order them.
        // This IS asserted, because it is what makes the rerank a reordering rather than a
        // second retrieval stage: if turning the rerank off changed recall, the rerank would be
        // deciding WHICH notes come back, and that would be a much bigger claim.
        assert!(
            (measured.recall_at_20 - baseline.recall_at_20).abs() < 1e-9
                && (measured.recall_at_50 - baseline.recall_at_50).abs() < 1e-9,
            "{label}: the rerank must not change WHICH notes are retrieved, only their order \
             (recall@20 {:.4} vs baseline {:.4}, recall@50 {:.4} vs {:.4})",
            measured.recall_at_20,
            baseline.recall_at_20,
            measured.recall_at_50,
            baseline.recall_at_50,
        );
        // And the ceiling really is below the gate, which is the claim the whole comment rests
        // on. Asserted so it cannot quietly stop being true if a gate constant moves.
        assert!(
            rank_blending_mrr_ceiling(mounts) < RANK_QUALITY_RETENTION * baseline.mrr,
            "{label}: rank fusion's MRR ceiling must be below the gate, or the rerank is not \
             load-bearing"
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario coverage at the SERVER level
//
// The fusion rule's own scenarios -- the deepening loop, the candidate budget, the tie-break,
// weights over synthetic lists, and determinism under a permuted mount table -- are unit
// tested in `deep-obsidian-server/src/federation.rs`, over hand-built ranked lists. They have
// to be: the per-mount candidate target is at least 100 candidates and this corpus has a
// dozen notes per mount, so against real indexes every mount is exhausted on its first page
// and neither the deepening loop nor the budget ever runs.
//
// What is left for this file is what genuinely needs real mounts, a real index per mount and
// the real MCP payload.
// ---------------------------------------------------------------------------

/// One mount unreachable: partial results, `degraded: true`, the mount named, and the
/// SURVIVING mount's answer unchanged.
///
/// The last part is the one worth the most: a degraded flag that came with a silently
/// reordered or truncated answer would be worse than no flag, because a caller would trust
/// the results it did get.
#[tokio::test(flavor = "multi_thread")]
async fn an_unavailable_mount_degrades_the_answer_and_is_named_without_disturbing_the_rest() {
    let healthy = Harness::new("degraded-healthy", Layout::TwoMounts, &[]).await;
    let broken =
        Harness::with_broken_mounts("degraded-broken", Layout::TwoMounts, &[], &["team"]).await;

    // A query the ROOT mount answers, so the surviving half of the vault has a real answer.
    let query = json!({"query": "Zephyrus7 Quaalbrook", "limit": 50, "includeText": false});
    let payload = broken.structured("hybrid_search", query.clone()).await;

    assert_eq!(payload["degraded"], json!(true), "{payload}");
    assert_eq!(
        payload["missingBackends"],
        json!(["team"]),
        "the unreachable mount must be named: {payload}"
    );
    let reason = payload["degradationReason"]
        .as_str()
        .expect("a degradation reason");
    assert!(
        reason.contains("team") && reason.contains("could not be searched"),
        "the reason must say which mount and what happened: {reason}"
    );
    // The mount is still listed, with an error and nothing to contribute -- not omitted.
    let team = payload["mounts"]
        .as_array()
        .expect("mounts")
        .iter()
        .find(|mount| mount["id"] == json!("team"))
        .expect("the broken mount is still reported");
    assert!(team["error"].is_string(), "{team}");
    assert_eq!(team["candidateCount"], json!(0), "{team}");

    // Nothing from the broken mount leaked in, and the root mount's own hits are exactly the
    // ones it contributed to the healthy run, in the same relative order.
    let broken_paths: Vec<&str> = payload["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .filter_map(|item| item["path"].as_str())
        .collect();
    assert!(
        broken_paths.iter().all(|path| !path.starts_with("Team/")),
        "{broken_paths:?}"
    );
    let healthy_payload = healthy.structured("hybrid_search", query).await;
    let healthy_root_paths: Vec<&str> = healthy_payload["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .filter_map(|item| item["path"].as_str())
        .filter(|path| !path.starts_with("Team/"))
        .collect();
    assert_eq!(
        broken_paths, healthy_root_paths,
        "removing a mount must not change the surviving mount's own ranking"
    );

    // And the healthy run says so: always present, never true without a cause.
    assert_eq!(healthy_payload["degraded"], json!(false));
    assert!(healthy_payload.get("missingBackends").is_none());
}

/// A configured `recallWeight` is reported, and it changes the FUSED order.
///
/// # Why this runs with the rerank OFF
///
/// `recallWeight` is an input to the FUSION stage: it scales a mount's `w / (60 + rank)`
/// contribution, which is what decides the fused order. The final rerank then rescores those
/// candidates against the query and its ordering does not consult the weight at all — by
/// design, since a mount-independent scorer that took a per-mount preference as an input would
/// not be mount-independent. So the weight's effect is observable exactly where it applies, and
/// asserting it through a reranked answer would be asserting that the rerank fails to do its
/// job.
///
/// The pair of runs is the assertion: the SAME query over the SAME corpus, once with equal
/// weights and once with the root mount at 2.0. Asserting only the weighted run would not
/// distinguish the weight from the mount-id tie-break — and here the tie-break runs AGAINST
/// the root mount ('team' sorts before 'vault'), so a root-mount hit reaching rank 1 can only
/// be the weight.
#[tokio::test(flavor = "multi_thread")]
async fn a_configured_recall_weight_is_reported_and_reorders_the_fused_answer() {
    let query = json!({"query": "Zephyrus7 Quaalbrook", "limit": 50, "includeText": false});
    let expected = "Engineering/Zephyrus.md";

    let equal = Harness::without_rerank("weights-equal", Layout::TwoMounts, &[]).await;
    let equal_payload = equal.structured("hybrid_search", query.clone()).await;
    assert_eq!(equal_payload["rerank"], json!("none"), "{equal_payload}");
    let equal_matches = equal_payload["matches"].as_array().expect("matches");
    assert_eq!(
        equal_matches[0]["mountId"],
        json!("team"),
        "with equal weights and no rerank the mount-id tie-break puts 'team' first: \
         {equal_payload}"
    );
    let equal_rank = equal_matches
        .iter()
        .position(|item| item["path"] == json!(expected))
        .expect("the answer is retrieved either way");
    assert!(
        equal_rank > 0,
        "the unweighted run must NOT already rank the answer first, or the weighted run \
         proves nothing: {equal_payload}"
    );
    for mount in equal_payload["mounts"].as_array().expect("mounts") {
        assert_eq!(mount["recallWeight"], json!(1.0), "{mount}");
    }

    let weighted =
        Harness::without_rerank("weights-two", Layout::TwoMounts, &[("vault", 2.0)]).await;
    let weighted_payload = weighted.structured("hybrid_search", query.clone()).await;
    let weighted_matches = weighted_payload["matches"].as_array().expect("matches");
    // 2.0/(60 + 0) beats 1.0/(60 + 0), so the heavier mount's best hit leads.
    assert_eq!(
        weighted_matches[0]["path"],
        json!(expected),
        "a 2.0-weight mount's rank-0 hit must outrank an equally-ranked 1.0 mount's: \
         {weighted_payload}"
    );
    assert_eq!(weighted_matches[0]["mountId"], json!("vault"));
    // Reported, so a caller can see why the order is what it is.
    let mounts = weighted_payload["mounts"].as_array().expect("mounts");
    let vault = mounts
        .iter()
        .find(|mount| mount["id"] == json!("vault"))
        .expect("the root mount");
    assert_eq!(vault["recallWeight"], json!(2.0), "{vault}");
    let team = mounts
        .iter()
        .find(|mount| mount["id"] == json!("team"))
        .expect("the team mount");
    assert_eq!(team["recallWeight"], json!(1.0), "{team}");

    // The weight is reported on a RERANKED answer too -- it explains the candidate set that
    // was reranked, and a caller cannot see it anywhere else.
    let reranked = Harness::new("weights-reranked", Layout::TwoMounts, &[("vault", 2.0)]).await;
    let reranked_payload = reranked.structured("hybrid_search", query).await;
    assert_eq!(
        reranked_payload["rerank"],
        json!("semantic+lexical"),
        "{reranked_payload}"
    );
    let vault = reranked_payload["mounts"]
        .as_array()
        .expect("mounts")
        .iter()
        .find(|mount| mount["id"] == json!("vault"))
        .expect("the root mount")
        .clone();
    assert_eq!(vault["recallWeight"], json!(2.0), "{vault}");
}

/// A weight cannot be zero, negative, or non-finite: config validation refuses it.
///
/// At the CONFIG layer rather than at the fusion call site, because a weight that cannot
/// produce a meaningful ordering is a config mistake — and a server that came up reporting
/// every mount healthy and then silently dropped one from every ranking is the worst possible
/// place to discover it.
#[test]
fn an_unusable_recall_weight_is_refused_by_config_validation() {
    for weight in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let error = deep_obsidian_config::normalize_service_config(
            deep_obsidian_types::ServiceConfigInput {
                mounts: Some(vec![MountConfig {
                    id: "vault".to_string(),
                    mount_at: String::new(),
                    backend: MountBackendConfig::Filesystem {
                        vault_path: PathBuf::from("/tmp/federation-eval-vault"),
                        index_dir: None,
                    },
                    recall_weight: Some(weight),
                }]),
                ..Default::default()
            },
        )
        .expect_err("an unusable recallWeight must not resolve");
        let message = error.to_string();
        assert!(
            message.contains("recallWeight") && message.contains("vault"),
            "the refusal must name the field and the mount, got: {message}"
        );
    }

    // A usable weight resolves and survives normalization unchanged.
    let resolved =
        deep_obsidian_config::normalize_service_config(deep_obsidian_types::ServiceConfigInput {
            mounts: Some(vec![MountConfig {
                id: "vault".to_string(),
                mount_at: String::new(),
                backend: MountBackendConfig::Filesystem {
                    vault_path: PathBuf::from("/tmp/federation-eval-vault"),
                    index_dir: None,
                },
                recall_weight: Some(2.5),
            }]),
            ..Default::default()
        })
        .expect("a positive finite weight resolves");
    assert_eq!(resolved.mounts[0].recall_weight, Some(2.5));
}

/// A mount that CANNOT hold artifacts is reported as skipped, and the answer stays complete.
///
/// The honesty distinction this asserts is the whole point of having two fields:
///
/// * an Algolia mount stores markdown records and has no binary read at all, so it holds no
///   artifacts. Omitting it from `search_artifacts` omits nothing, so it is `skipped` with a
///   reason and `degraded` stays `false`. Calling it a missing backend would train a reader
///   to ignore `missingBackends` — the field that has to mean something when a real mount is
///   down.
/// * the same mount IS part of `hybrid_search`, natively, because ranked search is exactly
///   what it can answer. So "absent from artifacts" is not "absent from recall".
///
/// It lives here rather than in `multi_vault.rs` because it needs a working ARTIFACT
/// embedding backend, and this file already runs one.
#[tokio::test(flavor = "multi_thread")]
async fn a_mount_that_cannot_hold_artifacts_is_skipped_rather_than_reported_missing() {
    let base = unique_base("artifact-skip");
    let _ = fs::remove_dir_all(&base);
    let root_vault = base.join("root-vault");
    let index_dir = base.join("index");
    fs::create_dir_all(&index_dir).expect("index dir");
    fs::create_dir_all(&root_vault).expect("root vault");
    write_note(&root_vault, "Root.md", "# Root\n\nshared charter text.\n");
    // One real ARTIFACT on the root mount. Without it the artifact embedding table is never
    // built, the index never learns its vector dimension, and `search_artifacts` refuses
    // before it can produce a payload -- so the skip reporting below would be untestable.
    // The bytes are arbitrary: the indexer base64s the file and never decodes it.
    fs::create_dir_all(root_vault.join("Assets")).expect("assets dir");
    fs::write(
        root_vault.join("Assets/Diagram.png"),
        b"not really a png, and nothing decodes it",
    )
    .expect("write artifact");

    let embedding_url = spawn_pseudo_embedding_server();
    let (algolia_url, _mock) = deep_obsidian_algolia::mock::spawn_mock().await;
    let secrets = base.join("secrets.json");
    let resolver =
        deep_obsidian_config::secrets::SecretResolver::with_encrypted_file_path(secrets.clone());
    let api_key_ref = deep_obsidian_types::SecretRef::EncryptedFile {
        id: "algolia-api-key".to_string(),
    };
    resolver
        .put(
            &api_key_ref,
            secrecy::SecretString::new("test-key".to_string()),
        )
        .expect("store the fixture api key");

    let config = ResolvedServiceConfig {
        federated_rerank: true,
        vault_path: root_vault.clone(),
        mounts: vec![
            MountConfig {
                id: "vault".to_string(),
                mount_at: String::new(),
                backend: MountBackendConfig::Filesystem {
                    vault_path: root_vault.clone(),
                    index_dir: None,
                },
                recall_weight: None,
            },
            MountConfig {
                id: "shared".to_string(),
                mount_at: "_Shared".to_string(),
                backend: MountBackendConfig::Algolia {
                    app_id: "TESTAPP".to_string(),
                    index_name: "team-wiki".to_string(),
                    api_key_ref,
                    base_url: Some(algolia_url),
                    writable: false,
                    participant_id: Some("paul@test".to_string()),
                    cache: None,
                    retention: None,
                    index_dir: None,
                },
                recall_weight: None,
            },
        ],
        experimental: ExperimentalConfig {
            multi_vault: true,
            algolia_vaults: true,
            ..ExperimentalConfig::default()
        },
        index_dir,
        transport: TransportMode::Http,
        stdio_mode: StdioMode::Auto,
        http: HttpConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            mcp_path: "/mcp".to_string(),
            health_path: "/healthz".to_string(),
        },
        auto_reindex: AutoReindexConfig {
            enabled: false,
            debounce_ms: 0,
            interval_ms: 0,
        },
        embedding: eval_embedding(&embedding_url),
        // The reason this test cannot live in `multi_vault.rs`: without a reachable artifact
        // embedding backend `search_artifacts` errors before it ever builds a payload.
        artifact_embedding: eval_embedding(&embedding_url),
        auth: AuthConfig::default(),
        config_file_path: None,
    };

    let backends = MountBackends::build_with_resolver(&config, &resolver);
    let (runtimes, _auto_reindex) = MountRuntimes::bootstrap(&config, &backends)
        .await
        .expect("bootstrap runtimes");
    let harness = Harness {
        base: base.clone(),
        layout: Layout::TwoMounts,
        state: AppState::with_backends(config, runtimes, &backends),
    };

    let payload = harness
        .structured("search_artifacts", json!({"query": "charter"}))
        .await;
    assert_eq!(payload["federated"], json!(true), "{payload}");
    let shared = payload["mounts"]
        .as_array()
        .expect("mounts")
        .iter()
        .find(|mount| mount["id"] == json!("shared"))
        .expect("the algolia mount is still reported")
        .clone();
    assert_eq!(shared["skipped"], json!(true), "{shared}");
    assert!(
        shared["skippedReason"]
            .as_str()
            .expect("a skip reason")
            .contains("binary"),
        "the reason must say WHY the mount holds no artifacts: {shared}"
    );
    assert!(
        shared.get("error").is_none(),
        "a skip is not an error: {shared}"
    );
    assert_eq!(shared["candidateCount"], json!(0), "{shared}");
    assert_eq!(
        payload["degraded"],
        json!(false),
        "a mount that cannot hold artifacts is not a shortfall: {payload}"
    );
    assert!(
        payload.get("missingBackends").is_none(),
        "a skipped mount must not appear as missing: {payload}"
    );

    // The same mount IS part of federated RECALL, natively -- so "cannot hold artifacts" is
    // not "cannot be searched".
    let recall = harness
        .structured("hybrid_search", json!({"query": "charter"}))
        .await;
    let shared_recall = recall["mounts"]
        .as_array()
        .expect("mounts")
        .iter()
        .find(|mount| mount["id"] == json!("shared"))
        .expect("the algolia mount takes part in recall")
        .clone();
    assert_eq!(shared_recall["source"], json!("native-recall"), "{recall}");
    assert!(shared_recall.get("skipped").is_none(), "{shared_recall}");
    assert_eq!(recall["degraded"], json!(false), "{recall}");
}
