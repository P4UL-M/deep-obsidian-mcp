//! Retrieval-quality eval harness (deterministic, CI-runnable).
//!
//! This is the measurement baseline for the upcoming retrieval-pipeline overhaul
//! (heading chunking, query encoding, RRF fusion, small-to-big, graph re-rank). It
//! proves equal-or-better retrieval and catches silent regressions WITHOUT a live
//! Ollama by driving the OpenAI-compatible embedding HTTP path with a deterministic
//! text->vector function.
//!
//! How it stays hermetic and reproducible:
//!   * A fake embedding server (reimplemented here because the in-crate test server
//!     lives in a `#[cfg(test)]` module a `tests/` file cannot see) serves vectors
//!     for an arbitrary number of requests on a detached thread; the process exit
//!     reaps it.
//!   * `pseudo_embedding` maps text -> a fixed-dim, L2-normalized vector using a
//!     hand-rolled FNV-1a hash over `index::tokenize` tokens. It is bag-of-words, so
//!     it is reproducible and shares BM25's vocabulary. A small SYNONYM table folds
//!     a handful of disjoint-surface paraphrases onto shared buckets so a genuine
//!     "dense paraphrase win" exists (dense aligns where BM25 cannot).
//!   * The index is built with an `EmbeddingConfig` pointing at the fake server, so
//!     BOTH build-time chunk embedding AND query-time embedding hit the same
//!     deterministic function. No production code is touched.
//!
//! Metrics are scored at the NOTE-PATH level (the fixtures are each well under the
//! 80-line default chunk window, so every note is a single chunk; chunk-level and
//! path-level coincide). Recall@k (k=1,3,5) and MRR are computed over a gold query
//! set and asserted `>=` committed baselines (see `BASELINE_*` below).

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use deep_obsidian_index::embeddings::{self, EmbeddingConfig, EmbeddingProvider};
use deep_obsidian_index::index::{build_index_from_snapshots, collect_snapshots, tokenize};
use deep_obsidian_index::search::{
    bm25_search_with_options, hybrid_search_with_options, related_notes,
    semantic_search_with_options, RankingOptions, SearchMatch,
};

// ---------------------------------------------------------------------------
// Committed baseline.
//
// These are the CURRENT aggregate metric values, recorded from the deterministic
// harness below. The asserts require the live harness to be `>=` each baseline, so
// any ranking change that regresses retrieval fails CI, while an improvement passes.
//
// HOW / WHEN TO BUMP: only raise a baseline after an intentional ranking improvement
// lands and you have re-run `cargo test -p deep-obsidian-index retrieval_eval` 2-3x
// to confirm the new (higher) number is stable. Never lower a baseline to make a
// regression pass -- investigate the regression instead. The printed per-query +
// aggregate report (run with `--nocapture`) is the source of truth for new values.
// ---------------------------------------------------------------------------
// Recorded 2026-06 from the deterministic harness (stable across repeated runs).
// This layer is primarily a SILENT-REGRESSION CATCHER: recall@3/@5 are pinned at 1.0,
// and the only sub-1.0 source is the disjoint-vocab car paraphrase (labeled note ranks
// #2 behind an equally-valid twin). Headroom for *demonstrating* an improvement is
// therefore limited here -- the real-model protocol in docs/retrieval-eval.md is where
// query-encoding gains are observed. Equal-or-better is what these baselines enforce.
const BASELINE_HYBRID_RECALL_AT_1: f64 = 0.917;
const BASELINE_HYBRID_RECALL_AT_3: f64 = 1.000;
const BASELINE_HYBRID_RECALL_AT_5: f64 = 1.000;
const BASELINE_HYBRID_MRR: f64 = 0.958;

/// Tolerance for the equal-or-better comparison. The baselines above are rounded
/// decimals of exact fractions (e.g. 11/12 = 0.91666...), so a strict `>=` against
/// the rounded literal can spuriously fail; we require `measured + EPSILON >= baseline`
/// (i.e. equal-or-better, modulo rounding). It is far smaller than the smallest
/// possible metric step (1/12 over this gold set), so it never masks a real regression.
const BASELINE_EPSILON: f64 = 1e-3;

// Dimensionality of the deterministic pseudo-embedding. Small but >1 so cosine
// ranking is meaningful; fixed so the vec0 `float[DIM]` table is consistent.
const EMBEDDING_DIM: usize = 64;

// ---------------------------------------------------------------------------
// Deterministic pseudo-semantic embedding (test-only; no production code touched).
// ---------------------------------------------------------------------------

/// Synonym folding: paraphrases with DISJOINT surface vocabulary are mapped onto a
/// shared canonical token so they land in the same hash bucket. This is what gives
/// the harness a real "dense paraphrase win": BM25 (query-term overlap) cannot match
/// a zero-overlap paraphrase, but the folded dense vector aligns on the shared
/// concept. Keep this tiny and intentional.
fn canonical_token(token: &str) -> &str {
    match token {
        "car" | "automobile" | "vehicle" | "motorcar" => "concept_car",
        "physician" | "doctor" | "clinician" => "concept_physician",
        "ailment" | "illness" | "malady" | "sickness" => "concept_illness",
        "remedy" | "cure" | "treatment" => "concept_remedy",
        other => other,
    }
}

/// FNV-1a (64-bit) over bytes. Fixed constants => process-independent and stable
/// across runs (unlike `std`'s `RandomState`-seeded hashers).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Deterministic text -> fixed-dim L2-normalized vector.
///
/// Bag-of-words token-frequency projection: each (synonym-folded) token contributes
/// its frequency to a hashed bucket, with a hashed sign so distinct tokens are not
/// all positively correlated. Semantically-similar fixture texts (shared/folded
/// tokens) get similar vectors. The zero-token case returns a fixed unit vector so
/// every input yields exactly `EMBEDDING_DIM` finite values (the query-time
/// dimension check rejects anything else).
fn pseudo_embedding(text: &str) -> Vec<f64> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for token in tokenize(text) {
        *counts.entry(canonical_token(&token).to_string()).or_insert(0) += 1;
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
        // Fixed, non-zero unit vector for token-less inputs.
        let mut fallback = vec![0.0_f64; EMBEDDING_DIM];
        fallback[0] = 1.0;
        return fallback;
    }
    vector.iter().map(|value| value / norm).collect()
}

// ---------------------------------------------------------------------------
// Fake embedding server (OpenAI-compatible /embeddings), deterministic + unbounded.
// ---------------------------------------------------------------------------

fn spawn_pseudo_embedding_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind eval embedding server");
    let address = listener.local_addr().expect("server address");
    // Detached: serves an arbitrary number of requests (build batches + queries).
    // Process exit reaps the thread; we never join it.
    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => handle_request(stream),
                Err(_) => break,
            }
        }
    });
    format!("http://{address}")
}

fn handle_request(mut stream: TcpStream) {
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
    let payload: serde_json::Value = serde_json::from_slice(body).expect("json request");
    let inputs = payload
        .get("input")
        .and_then(serde_json::Value::as_array)
        .expect("input array")
        .iter()
        .map(|value| value.as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();

    let data = inputs
        .iter()
        .enumerate()
        .map(|(index, text)| {
            serde_json::json!({
                "index": index,
                "embedding": pseudo_embedding(text),
            })
        })
        .collect::<Vec<_>>();
    let response_body = serde_json::json!({ "data": data }).to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        response_body.len(),
        response_body
    );
    let _ = stream.write_all(response.as_bytes());
}

// ---------------------------------------------------------------------------
// Fixture vault.
// ---------------------------------------------------------------------------

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("deep-obsidian-eval-{label}-{nanos}-{suffix}"))
}

fn write_fixture(root: &Path, relative: &str, content: &str) {
    let absolute = root.join(relative);
    if let Some(parent) = absolute.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(&absolute, content).expect("write fixture");
}

/// ~11 small synthetic notes covering the overhaul's target cases:
///   * multi-heading notes (`#`/`##`/`###`)
///   * fenced code blocks and a table
///   * cross-linked notes (`[[wikilinks]]`)
///   * near-duplicate / paraphrase content with DISJOINT surface vocab (dense recall)
///   * distinctive exact-term identifiers / proper nouns (BM25 exact hits)
fn write_fixture_vault(root: &Path) {
    // Multi-heading note with a fenced code block. Distinctive identifier
    // `Zephyrus7` and proper noun `Quaalbrook` give BM25 exact-term anchors.
    write_fixture(
        root,
        "Engineering/Zephyrus.md",
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
    );

    // Cross-linked downstream note.
    write_fixture(
        root,
        "Engineering/Ingestion Pipeline.md",
        "# Ingestion Pipeline\n\
         \n\
         ## Stages\n\
         The pipeline batches records and writes them to the warehouse.\n\
         \n\
         ## Backpressure\n\
         When the queue saturates the pipeline applies backpressure.\n\
         \n\
         Upstream is [[Engineering/Zephyrus]].\n",
    );

    // A table plus a unique error code `ERR_4471` for an exact-term BM25 win.
    write_fixture(
        root,
        "Ops/Error Codes.md",
        "# Error Codes\n\
         \n\
         | Code | Meaning | Action |\n\
         | --- | --- | --- |\n\
         | ERR_4471 | disk quota exceeded | free space |\n\
         | ERR_5582 | auth token expired | refresh token |\n\
         \n\
         Escalate persistent failures to the on-call rotation.\n",
    );

    // Paraphrase pair A1/A2: SAME meaning, DISJOINT surface vocabulary (synonyms
    // folded by `canonical_token`). A2 is the dense-recall target for a query phrased
    // like A1 -- BM25 cannot bridge the vocab gap, dense can.
    write_fixture(
        root,
        "Library/Automobile Maintenance.md",
        "# Automobile Maintenance\n\
         \n\
         Routine upkeep of an automobile keeps the vehicle dependable.\n\
         Inspect the motorcar regularly to avoid breakdowns.\n",
    );
    write_fixture(
        root,
        "Library/Car Care Basics.md",
        "# Car Care Basics\n\
         \n\
         Looking after your car keeps the automobile reliable over the years.\n\
         A well maintained vehicle rarely strands you on the road.\n",
    );

    // Paraphrase pair B1/B2 in a medical register, again disjoint surface vocab.
    write_fixture(
        root,
        "Health/Seeing a Doctor.md",
        "# Seeing a Doctor\n\
         \n\
         When an illness lingers, visit a doctor for a proper diagnosis.\n\
         A physician can prescribe the right remedy for the ailment.\n",
    );
    write_fixture(
        root,
        "Health/Clinician Visits.md",
        "# Clinician Visits\n\
         \n\
         A clinician evaluates the sickness and recommends a treatment.\n\
         Trust the physician to choose an effective cure for the malady.\n",
    );

    // Fusion case: the answer note shares the query's broad vocabulary, while a decoy
    // note holds a single rare exact keyword that BM25 over-weights via IDF. Hybrid
    // must combine dense (broad vocab) and BM25 (exact term) to rank the answer.
    write_fixture(
        root,
        "Notes/Garden Planning.md",
        "# Garden Planning\n\
         \n\
         Plan the garden layout, prepare the soil, choose seeds, and water beds daily.\n\
         A good garden plan balances sunlight, drainage, and seasonal planting.\n",
    );
    write_fixture(
        root,
        "Notes/Compost Trivia.md",
        "# Compost Trivia\n\
         \n\
         The rare term Bokashi appears here once and nowhere else in the vault.\n",
    );

    // Distinctive proper-noun note for an unambiguous BM25 exact-term win.
    write_fixture(
        root,
        "People/Octavia Hartwell.md",
        "# Octavia Hartwell\n\
         \n\
         Octavia Hartwell leads the Threnody research group.\n\
         Contact Octavia about the Threnody roadmap.\n",
    );

    // Cross-linked answer: question note links to the note that holds the answer.
    write_fixture(
        root,
        "Wiki/Capital Question.md",
        "# Capital Question\n\
         \n\
         For the seat of government see [[Wiki/Capital Answer]].\n",
    );
    write_fixture(
        root,
        "Wiki/Capital Answer.md",
        "# Capital Answer\n\
         \n\
         The administrative capital city is Lindholm, home to the parliament.\n",
    );

    // Large MULTI-SECTION note for small-to-big retrieval (issue #6 item #4). Its
    // `## Maintenance Protocol` section is intentionally oversized (well past the 512-token
    // `SECTION_CHUNK_TARGET_TOKENS` budget) so the item-#1 chunker MUST split it into
    // several sub-chunks at paragraph boundaries. Sentinels are placed so a hit on a LATER
    // sub-chunk only "sees" the heading line and the first-paragraph sibling sentinel if
    // small-to-big expansion pulled in the whole enclosing section:
    //   * `siblingsentinel_qux42`     -> FIRST paragraph (the heading-bearing sub-chunk)
    //   * `distinctiveterm_zylophone` -> a LATER paragraph (the query target term)
    //   * `adjacentsentinel_grobnak7` -> a DIFFERENT section (must NOT be captured)
    // All identifiers are unique nonsense disjoint from every existing gold query so the
    // note cannot perturb the committed aggregate baseline. The filler uses long, distinct,
    // non-stopword tokens because the chunker sizes sections by `tokenize`d token count
    // (short words / stopwords are dropped), so raw word count would understate the budget.
    // Each paragraph's filler alone exceeds the 512-token target so the packer places every
    // paragraph in its OWN sub-chunk; the heading-bearing first paragraph and the
    // distinctive-term later paragraph therefore land in DIFFERENT sub-chunks.
    let protocol_filler = "calibration telemetry harmonic resonance throughput \
        diagnostic subsystem actuator manifold turbine compressor lubrication \
        bearing tolerance vibration alignment torque hydraulic pneumatic coolant "
        .repeat(40);
    let maintenance_note = format!(
        "# Maintenance Handbook\n\
         \n\
         ## Maintenance Protocol\n\
         The opening guidance establishes the siblingsentinel_qux42 baseline before any work begins.\n\
         {protocol_filler}\n\
         \n\
         The recurring inspection cadence keeps the assembly within published tolerances.\n\
         {protocol_filler}\n\
         \n\
         The escalation step records the distinctiveterm_zylophone signature for the audit trail.\n\
         {protocol_filler}\n\
         \n\
         ## Unrelated Appendix\n\
         This appendix is a separate section and carries the adjacentsentinel_grobnak7 marker.\n\
         It must never appear in the returned text of a Maintenance Protocol chunk hit.\n",
    );
    write_fixture(root, "Manuals/Maintenance Handbook.md", &maintenance_note);
}

// ---------------------------------------------------------------------------
// Gold query set.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct GoldQuery {
    /// What this query is meant to exercise (for the printed report).
    intent: &'static str,
    query: &'static str,
    /// Expected note path (primary relevance label, path-level).
    expected_path: &'static str,
}

fn gold_queries() -> Vec<GoldQuery> {
    vec![
        // BM25 exact-term wins on distinctive identifiers / proper nouns.
        GoldQuery {
            intent: "bm25 exact identifier",
            query: "Zephyrus7 Quaalbrook",
            expected_path: "Engineering/Zephyrus.md",
        },
        GoldQuery {
            intent: "bm25 exact error code",
            query: "ERR_4471 disk quota",
            expected_path: "Ops/Error Codes.md",
        },
        GoldQuery {
            intent: "bm25 exact proper noun",
            query: "Octavia Hartwell Threnody",
            expected_path: "People/Octavia Hartwell.md",
        },
        // Dense-RECALL property (disjoint surface vocab, synonym-folded). NOTE: for
        // this query the labeled `Car Care Basics` ranks #2 dense, behind the
        // equally-valid twin `Automobile Maintenance` (#1) which shares more literal
        // tokens. The defensible, asserted claim is narrow: BM25 cannot retrieve the
        // labeled note AT ALL (zero literal overlap), while dense surfaces it at #2.
        // See the `car_*_rank` assertion below.
        GoldQuery {
            intent: "dense paraphrase (car)",
            query: "keeping a motorcar dependable",
            expected_path: "Library/Car Care Basics.md",
        },
        // Recall/fusion case. NOTE: this is NOT a dense-beats-BM25 case -- BM25 also
        // ranks Clinician Visits #1 (the query shares `clinician`/`cure`/`malady`
        // literally). Kept to exercise medical-register recall in both signals.
        GoldQuery {
            intent: "recall (medical synonyms)",
            query: "a clinician picks a cure for the malady",
            expected_path: "Health/Clinician Visits.md",
        },
        // Fusion: broad-vocab answer vs rare-term decoy.
        GoldQuery {
            intent: "fusion broad-vocab vs rare decoy",
            query: "planning a garden layout with soil and seeds",
            expected_path: "Notes/Garden Planning.md",
        },
        // Cross-linked answer cases.
        GoldQuery {
            intent: "cross-link downstream stages",
            query: "downstream ingestion pipeline stages warehouse",
            expected_path: "Engineering/Ingestion Pipeline.md",
        },
        GoldQuery {
            intent: "cross-link capital answer",
            query: "administrative capital city parliament Lindholm",
            expected_path: "Wiki/Capital Answer.md",
        },
        // Heading / code-block content retrieval.
        GoldQuery {
            intent: "code block bootstrap command",
            query: "bootstrap routine before serving traffic",
            expected_path: "Engineering/Zephyrus.md",
        },
        GoldQuery {
            intent: "table row meaning",
            query: "auth token expired refresh",
            expected_path: "Ops/Error Codes.md",
        },
        // Backpressure concept.
        GoldQuery {
            intent: "concept backpressure queue",
            query: "queue saturates backpressure",
            expected_path: "Engineering/Ingestion Pipeline.md",
        },
        // Paraphrase in the illness register (synonym-folded). Ranks #1 today; kept as
        // a paraphrase-recall guard rather than a known-hard case.
        GoldQuery {
            intent: "paraphrase (illness lingers)",
            query: "what to do when an ailment will not go away",
            expected_path: "Health/Seeing a Doctor.md",
        },
    ]
}

// ---------------------------------------------------------------------------
// Metrics.
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy)]
struct Aggregate {
    recall_at_1: f64,
    recall_at_3: f64,
    recall_at_5: f64,
    mrr: f64,
}

/// Rank (1-based) of the first match whose path equals `expected`, if any.
fn rank_of(results: &[SearchMatch], expected: &str) -> Option<usize> {
    results
        .iter()
        .position(|item| item.path == expected)
        .map(|index| index + 1)
}

fn score(results_per_query: &[(GoldQuery, Vec<SearchMatch>)], label: &str) -> Aggregate {
    let total = results_per_query.len() as f64;
    let mut hits_at_1 = 0.0;
    let mut hits_at_3 = 0.0;
    let mut hits_at_5 = 0.0;
    let mut reciprocal_rank_sum = 0.0;

    println!("\n=== {label} — per-query ===");
    for (gold, results) in results_per_query {
        let rank = rank_of(results, gold.expected_path);
        let rank_label = rank
            .map(|value| value.to_string())
            .unwrap_or_else(|| "miss".to_string());
        if let Some(rank) = rank {
            if rank <= 1 {
                hits_at_1 += 1.0;
            }
            if rank <= 3 {
                hits_at_3 += 1.0;
            }
            if rank <= 5 {
                hits_at_5 += 1.0;
            }
            reciprocal_rank_sum += 1.0 / rank as f64;
        }
        println!(
            "  [{:<32}] rank={:<4} top1={:<28} q={:?}",
            gold.intent,
            rank_label,
            results
                .first()
                .map(|item| item.path.as_str())
                .unwrap_or("<none>"),
            gold.query,
        );
    }

    let aggregate = Aggregate {
        recall_at_1: hits_at_1 / total,
        recall_at_3: hits_at_3 / total,
        recall_at_5: hits_at_5 / total,
        mrr: reciprocal_rank_sum / total,
    };
    println!(
        "--- {label} — aggregate: recall@1={:.3} recall@3={:.3} recall@5={:.3} MRR={:.3} ---",
        aggregate.recall_at_1, aggregate.recall_at_3, aggregate.recall_at_5, aggregate.mrr
    );
    aggregate
}

// ---------------------------------------------------------------------------
// Harness wiring.
// ---------------------------------------------------------------------------

fn eval_config(base_url: String) -> EmbeddingConfig {
    EmbeddingConfig {
        provider: Some(EmbeddingProvider::OpenAiCompatible),
        model: Some("pseudo-eval-model".to_string()),
        base_url: Some(base_url),
        api_key: None,
        max_chars: embeddings::DEFAULT_EMBEDDING_MAX_CHARS,
        batch_size: embeddings::DEFAULT_EMBEDDING_BATCH_SIZE,
        max_input_tokens: embeddings::DEFAULT_EMBEDDING_MAX_INPUT_TOKENS,
        context_tokens: embeddings::DEFAULT_EMBEDDING_CONTEXT_TOKENS,
        chars_per_token: embeddings::DEFAULT_CHARS_PER_TOKEN,
        // Generic (non-qwen3) eval model => no auto-default; queries stay plain so the
        // baseline rankings are unchanged.
        query_instruction: None,
    }
    .normalize()
}

fn options(limit: usize) -> RankingOptions {
    RankingOptions {
        limit,
        ..RankingOptions::default()
    }
}

/// Build the deterministic embedding-backed index over the fixture vault.
fn build_eval_index() -> (PathBuf, deep_obsidian_index::index::SearchIndex) {
    let root = unique_temp_dir("retrieval");
    fs::create_dir_all(&root).expect("temp dir");
    write_fixture_vault(&root);

    let base_url = spawn_pseudo_embedding_server();
    let config = eval_config(base_url);
    let snapshots = collect_snapshots(&root).expect("collect snapshots");
    let index = build_index_from_snapshots(&root, None, snapshots, Some(&config)).expect("build index");
    (root, index)
}

/// Late-interaction (max-sim) note relatedness: a note sharing the source's topical
/// vocabulary must outrank an unrelated note. Dedicated tiny vault (the recall baseline
/// above is untouched). Exercises the per-source-chunk sqlite-vec KNN + max-then-sum
/// aggregation that replaced the dropped note-level dense vector.
#[test]
fn related_notes_late_interaction_ranks_topical_neighbor() {
    let root = unique_temp_dir("related-maxsim");
    fs::create_dir_all(&root).expect("temp dir");
    write_fixture(
        &root,
        "Source.md",
        "# Source\n\nThe quantum flux capacitor stabilizes the warp lattice resonance.\n",
    );
    // Shares the source's distinctive topical vocabulary.
    write_fixture(
        &root,
        "Neighbor.md",
        "# Neighbor\n\nThe quantum flux capacitor stabilizes the warp lattice resonance field.\n",
    );
    // Shares no topical vocabulary with the source.
    write_fixture(
        &root,
        "Unrelated.md",
        "# Unrelated\n\nSourdough bread recipes rely on wild yeast fermentation and rye.\n",
    );

    let base_url = spawn_pseudo_embedding_server();
    let config = eval_config(base_url);
    let snapshots = collect_snapshots(&root).expect("collect snapshots");
    let index = build_index_from_snapshots(&root, None, snapshots, Some(&config))
        .expect("build index");
    assert_eq!(
        index.semantic_backend,
        deep_obsidian_index::index::SemanticBackend::Embedding
    );

    let related = related_notes(&index, "Source.md").expect("related_notes");
    assert!(
        !related.is_empty(),
        "expected related notes via late interaction"
    );
    assert!(
        related.iter().all(|m| m.path != "Source.md"),
        "the source note must be excluded from its own related set"
    );
    let rank = |path: &str| related.iter().position(|m| m.path == path);
    let neighbor = rank("Neighbor.md").expect("topical neighbour present");
    let unrelated = rank("Unrelated.md");
    assert!(
        unrelated.map_or(true, |u| neighbor < u),
        "the topical neighbour should rank above the unrelated note (neighbor={neighbor:?}, unrelated={unrelated:?})"
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn retrieval_eval_meets_committed_baseline() {
    let (root, index) = build_eval_index();

    // Sanity: the index must actually be embedding-backed (otherwise the dense path
    // silently degrades to sparse term-overlap and the eval is meaningless).
    assert_eq!(
        index.semantic_backend,
        deep_obsidian_index::index::SemanticBackend::Embedding,
        "eval index must use the embedding backend"
    );
    assert_eq!(index.embedding_dimensions, Some(EMBEDDING_DIM));

    let golds = gold_queries();
    let limit = 5;

    let hybrid: Vec<_> = golds
        .iter()
        .cloned()
        .map(|gold| {
            let results = hybrid_search_with_options(&index, gold.query, options(limit))
                .expect("hybrid search");
            (gold, results)
        })
        .collect();
    let bm25: Vec<_> = golds
        .iter()
        .cloned()
        .map(|gold| {
            let results =
                bm25_search_with_options(&index, gold.query, options(limit)).expect("bm25 search");
            (gold, results)
        })
        .collect();
    let semantic: Vec<_> = golds
        .iter()
        .cloned()
        .map(|gold| {
            let results = semantic_search_with_options(&index, gold.query, options(limit))
                .expect("semantic search");
            (gold, results)
        })
        .collect();

    // Print all three so the report shows where dense and BM25 disagree.
    let bm25_aggregate = score(&bm25, "BM25");
    let semantic_aggregate = score(&semantic, "Semantic (dense)");
    let hybrid_aggregate = score(&hybrid, "Hybrid (fusion)");

    // The dense paraphrase queries (disjoint surface vocab) must be a real dense win:
    // dense ranks the paraphrase target, BM25 misses it within k=5.
    let dense_paraphrase_car = "keeping a motorcar dependable";
    let car_target = "Library/Car Care Basics.md";
    let car_dense_rank = rank_of(
        &semantic_search_with_options(&index, dense_paraphrase_car, options(limit))
            .expect("semantic car"),
        car_target,
    );
    let car_bm25_rank = rank_of(
        &bm25_search_with_options(&index, dense_paraphrase_car, options(limit)).expect("bm25 car"),
        car_target,
    );
    assert!(
        car_dense_rank.is_some(),
        "dense must retrieve the paraphrase target within k={limit}"
    );
    assert!(
        car_bm25_rank.is_none() || car_dense_rank.unwrap() < car_bm25_rank.unwrap(),
        "dense must beat BM25 on the disjoint-vocab paraphrase (dense={car_dense_rank:?}, bm25={car_bm25_rank:?})"
    );

    // Baseline assertions (equal-or-better). See BASELINE_* comment for bump policy.
    assert!(
        hybrid_aggregate.recall_at_1 + BASELINE_EPSILON >= BASELINE_HYBRID_RECALL_AT_1,
        "hybrid recall@1 regressed: {:.3} < baseline {:.3}",
        hybrid_aggregate.recall_at_1,
        BASELINE_HYBRID_RECALL_AT_1
    );
    assert!(
        hybrid_aggregate.recall_at_3 + BASELINE_EPSILON >= BASELINE_HYBRID_RECALL_AT_3,
        "hybrid recall@3 regressed: {:.3} < baseline {:.3}",
        hybrid_aggregate.recall_at_3,
        BASELINE_HYBRID_RECALL_AT_3
    );
    assert!(
        hybrid_aggregate.recall_at_5 + BASELINE_EPSILON >= BASELINE_HYBRID_RECALL_AT_5,
        "hybrid recall@5 regressed: {:.3} < baseline {:.3}",
        hybrid_aggregate.recall_at_5,
        BASELINE_HYBRID_RECALL_AT_5
    );
    assert!(
        hybrid_aggregate.mrr + BASELINE_EPSILON >= BASELINE_HYBRID_MRR,
        "hybrid MRR regressed: {:.3} < baseline {:.3}",
        hybrid_aggregate.mrr,
        BASELINE_HYBRID_MRR
    );

    // Hybrid should never be worse than either component on aggregate MRR -- that is
    // the whole point of fusion. (Sanity guard, not a tunable baseline.)
    assert!(
        hybrid_aggregate.mrr + 1e-9 >= bm25_aggregate.mrr.min(semantic_aggregate.mrr),
        "hybrid MRR ({:.3}) fell below the weaker component (bm25={:.3}, semantic={:.3})",
        hybrid_aggregate.mrr,
        bm25_aggregate.mrr,
        semantic_aggregate.mrr
    );

    fs::remove_dir_all(root).ok();
}

/// Small-to-big retrieval (issue #6 item #4): a chunk hit in a multi-section note must
/// RETURN its enclosing heading SECTION (the "big" context), while matching/ranking stays
/// at chunk granularity. Kept SEPARATE from the aggregate baseline test (and out of
/// `gold_queries()`) so it cannot move the committed recall@k/MRR numbers.
#[test]
fn small_to_big_returns_enclosing_section_for_chunk_hit() {
    let (root, index) = build_eval_index();
    let path = "Manuals/Maintenance Handbook.md";
    let limit = 5;

    // The target section is oversized on purpose; confirm the item-#1 chunker actually
    // split it into MULTIPLE sub-chunks, with the heading-bearing sentinel and the
    // distinctive query term in DIFFERENT sub-chunks (otherwise the assertions are vacuous).
    let head_chunk = index
        .chunks
        .iter()
        .find(|chunk| chunk.path == path && chunk.text.contains("siblingsentinel_qux42"))
        .expect("head sub-chunk");
    let term_chunk = index
        .chunks
        .iter()
        .find(|chunk| chunk.path == path && chunk.text.contains("distinctiveterm_zylophone"))
        .expect("term sub-chunk");
    assert_ne!(
        head_chunk.chunk_index, term_chunk.chunk_index,
        "fixture must split the Maintenance Protocol section so heading and term are in different sub-chunks"
    );
    assert!(
        !term_chunk.text.contains("siblingsentinel_qux42")
            && !term_chunk.text.contains("## Maintenance Protocol"),
        "the matched (term) sub-chunk must not already contain the heading or sibling sentinel"
    );

    // Query the LATER paragraph's distinctive term. BM25 (exact-term) must rank the note #1
    // (recall unaffected). The term lives in a non-first sub-chunk, so the matching chunk's
    // OWN text contains neither the heading line nor the first-paragraph sibling sentinel.
    let query = "distinctiveterm_zylophone signature audit";
    for (label, results) in [
        ("bm25", bm25_search_with_options(&index, query, options(limit)).expect("bm25")),
        ("semantic", semantic_search_with_options(&index, query, options(limit)).expect("semantic")),
        ("hybrid", hybrid_search_with_options(&index, query, options(limit)).expect("hybrid")),
    ] {
        assert_eq!(
            rank_of(&results, path),
            Some(1),
            "{label}: distinctive-term query must rank the handbook #1 (recall unaffected)"
        );
        let hit = results
            .iter()
            .find(|item| item.path == path)
            .expect("handbook hit");

        // "big": the returned text is the FULL enclosing section, so it contains the
        // heading line AND the first-paragraph sibling sentinel (which only appears if
        // expansion pulled in the heading-bearing sub-chunk), not just the matching window.
        assert!(
            hit.text.contains("## Maintenance Protocol"),
            "{label}: expanded text must include the section heading line\n--- got ---\n{}",
            hit.text
        );
        assert!(
            hit.text.contains("siblingsentinel_qux42"),
            "{label}: expanded text must include the FIRST-paragraph sibling sentinel (full section, not just the matching sub-chunk)"
        );
        assert!(
            hit.text.contains("distinctiveterm_zylophone"),
            "{label}: expanded text must still contain the matched term"
        );

        // No over-capture: the ADJACENT section's sentinel must be absent (proves we
        // returned the enclosing section, not the whole note nor an 80-line window).
        assert!(
            !hit.text.contains("adjacentsentinel_grobnak7"),
            "{label}: expanded text must NOT bleed into the adjacent section"
        );

        // Non-vacuity: the returned text is strictly larger than the matched sub-chunk's
        // own text, and spans the full section line range (heading sub-chunk .. term
        // sub-chunk), proving expansion to the "big" section genuinely happened.
        assert!(
            hit.text.len() > term_chunk.text.len(),
            "{label}: expanded text must exceed the matched sub-chunk's own text"
        );
        assert!(
            hit.start_line <= head_chunk.start_line && hit.end_line >= term_chunk.end_line,
            "{label}: returned range must cover both the heading and term sub-chunks"
        );
    }

    fs::remove_dir_all(root).ok();
}

#[test]
fn pseudo_embedding_is_deterministic_and_normalized() {
    let a = pseudo_embedding("the quick brown fox jumps");
    let b = pseudo_embedding("the quick brown fox jumps");
    assert_eq!(a, b, "pseudo embedding must be deterministic");
    assert_eq!(a.len(), EMBEDDING_DIM);
    let norm = a.iter().map(|value| value * value).sum::<f64>().sqrt();
    assert!((norm - 1.0).abs() < 1e-9, "vector must be L2-normalized");

    // Synonym folding: disjoint surface vocab maps onto the same vector.
    let car = pseudo_embedding("automobile");
    let vehicle = pseudo_embedding("motorcar");
    assert_eq!(car, vehicle, "synonyms must fold to the same vector");

    // Token-less input yields a fixed unit vector of the right dimension.
    let empty = pseudo_embedding("!!! ??? ...");
    assert_eq!(empty.len(), EMBEDDING_DIM);
    let empty_norm = empty.iter().map(|value| value * value).sum::<f64>().sqrt();
    assert!((empty_norm - 1.0).abs() < 1e-9);
}
