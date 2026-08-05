//! Federated recall: one ranked answer assembled from several mounts' independent
//! rankings.
//!
//! ## Why this lives in the server crate and not in the router
//!
//! The router ([`deep_obsidian_backend::VaultRouter`]) knows BACKENDS. Federated recall
//! has to blend two kinds of ranked list: the server's own per-mount SQLite index (for a
//! filesystem or couchdb mount) and a backend's native ranking (for a mount advertising
//! [`Capability::NativeRecall`](deep_obsidian_backend::Capability::NativeRecall)). Only
//! the server sees both — [`crate::runtime::MountRuntimes`] holds the local indexes and
//! the router holds the backends — so a router-level implementation could only federate
//! half the vault. The router therefore keeps refusing
//! [`RecallRequest::Search`](deep_obsidian_backend::RecallRequest::Search) and this
//! module is where the fusion happens.
//!
//! ## The algorithm, exactly
//!
//! 1. Ask every HEALTHY mount for [`candidate_target`] candidates: `max(100, limit × 10)`.
//!    A local-index mount answers that in one query; a native-recall mount answers it a
//!    page at a time and is asked again while it offers a cursor.
//! 2. Fuse with WEIGHTED Reciprocal Rank Fusion. A candidate at 0-BASED rank `r` in a
//!    mount whose weight is `w` contributes `w / (60 + r)`. Contributions are SUMMED per
//!    candidate key, which in practice never adds two terms: logical paths are namespaced
//!    by mount prefix, so the same key cannot appear in two mounts' lists. The sum is
//!    implemented anyway so the fusion is correct rather than correct-by-coincidence.
//! 3. The weight is [`DEFAULT_RECALL_WEIGHT`] unless the mount sets
//!    [`MountConfig::recall_weight`](deep_obsidian_types::MountConfig::recall_weight).
//! 4. Ties break deterministically: fused score DESC, then mount id ASC, then logical
//!    path ASC, then per-mount rank ASC. Every component is total, so the order is a
//!    function of the inputs alone.
//! 5. Adaptive deepening. After each fusion round, the frontier is STABLE when no
//!    unexhausted mount's next unseen candidate could enter the top `limit`: that
//!    candidate's best possible contribution is `w / (60 + next_rank)` (it cannot score
//!    higher, because rank only grows), so the mount is stable once that value is BELOW
//!    the current cutoff — the fused score at position `limit - 1`. Until then the mount
//!    is asked for another page. With fewer than `limit` fused hits there is no cutoff
//!    and every unexhausted mount is unstable by definition. The loop terminates because
//!    `w / (60 + next_rank)` strictly decreases and the global budget
//!    ([`candidate_budget`]) bounds it regardless.
//! 6. A final RERANK over the fused window, by a scorer that knows nothing about mounts —
//!    see [`rerank`]. This is the stage that undoes rank interleaving, and it is not
//!    optional cosmetics: without it, every mount's rank-0 hit scores the identical
//!    `w / (60 + 0)`, no candidate is ever in two lists (paths are namespaced) so no
//!    contribution is ever summed, and the fused order degenerates into a rank-for-rank
//!    interleave decided by the mount-id tie-break. An answer then lands at the position its
//!    own mount holds in that order however good the other mounts' hits are, which caps
//!    achievable MRR at `H_m/m` for `m` mounts — 0.75 for two, 0.61 for three. Fusion picks
//!    WHICH candidates are worth looking at; the rerank decides their order.
//! 7. A mount that errors does not fail the query: it is dropped from fusion and named,
//!    so the caller is told the answer is partial instead of being handed a short list
//!    that looks complete.
//!
//! ## No cross-backend content flows
//!
//! The constraint is about what crosses a BACKEND boundary: no note text, snippet or
//! embedding from one backend is ever handed to another. Fusion upholds it trivially — it
//! consumes [`CandidateKey`]s (a logical path and a chunk index) plus a rank and a mount id,
//! and nothing else, so an Algolia mount and a local vault meet only as ranks.
//!
//! The rerank reads the fused candidates' text and vectors, and that is NOT a violation: the
//! reading and the scoring both happen in the SERVER, over content the server already holds
//! to render snippets into the payload. Nothing is sent anywhere. Concretely, a candidate's
//! semantic score comes from the vector already stored in ITS OWN mount's index (compared
//! against one query vector the server embedded once), and a candidate with no stored vector
//! is embedded server-side through the configured pipeline. No mount is ever asked to score
//! another mount's content, which is the property that would actually be at stake.
//!
//! ## Determinism
//!
//! [`federate`] visits mounts in MOUNT-ID order, not config order. That matters: fetching
//! one mount's next page raises the cutoff, which can make another mount stable, so
//! config order would otherwise decide how many candidates each mount contributed. Since
//! the loop is driven by a cutoff computed once per ROUND and the within-round order is
//! canonical, permuting the mount table changes nothing observable — not the fused hits,
//! not the per-mount candidate counts.
//!
//! [`rerank`] preserves that: it is a pure function of the candidate set and its signals,
//! every comparator component is total (`total_cmp`, and a NaN signal is treated as no
//! signal rather than propagated), and the ranked lists it builds break ties by candidate
//! index — which is the fused order, itself order-invariant. So the whole pipeline replays
//! identically whatever order the mount table is written in.

use std::collections::BTreeMap;

/// The RRF damping constant. 60 is the value from the original Cormack et al. RRF paper
/// and the one the local hybrid ranker already uses, so a mount's contribution curve is
/// the same shape inside a mount and across mounts.
pub const FEDERATION_RRF_K: f64 = 60.0;

/// The weight of a mount that does not configure one.
pub const DEFAULT_RECALL_WEIGHT: f64 = 1.0;

/// Floor on how many candidates each mount is asked for.
const FEDERATION_MIN_CANDIDATES_PER_MOUNT: usize = 100;

/// How far past `limit` each mount is oversampled.
const FEDERATION_CANDIDATE_OVERSAMPLE: usize = 10;

/// Floor on the total candidate budget.
const FEDERATION_MIN_BUDGET: usize = 500;

/// How far past `limit` the total candidate budget is oversampled.
const FEDERATION_BUDGET_OVERSAMPLE: usize = 50;

/// How many candidates each mount is asked for per page: `max(100, limit × 10)`.
///
/// Oversampling is what makes fusion able to promote a hit that one mount ranked outside
/// its own top `limit`: with only `limit` candidates per mount the fused list could never
/// contain anything a mount did not already consider its best.
pub fn candidate_target(limit: usize) -> usize {
    limit
        .max(1)
        .saturating_mul(FEDERATION_CANDIDATE_OVERSAMPLE)
        .max(FEDERATION_MIN_CANDIDATES_PER_MOUNT)
}

/// The ceiling on candidates fetched across ALL mounts for one query:
/// `max(500, limit × 50)`.
///
/// A bound on work, not on correctness: reaching it is reported
/// ([`FederationOutcome::budget_reached`]) precisely because a deepening loop stopped
/// early may have left a better hit unseen, and a caller must be able to tell that from
/// a frontier that went stable on its own.
///
/// The exact invariant is that NO page is requested once this many candidates have already
/// arrived. The page in flight when the ceiling is crossed is still counted, so
/// [`FederationOutcome::candidates_fetched`] can exceed the budget by less than one page —
/// bounding the total instead would mean either discarding candidates already paid for or
/// asking for a fractional page.
pub fn candidate_budget(limit: usize) -> usize {
    limit
        .max(1)
        .saturating_mul(FEDERATION_BUDGET_OVERSAMPLE)
        .max(FEDERATION_MIN_BUDGET)
}

/// What makes two candidates the same candidate: a LOGICAL vault path and a chunk index.
///
/// Not the path alone. `hybrid_search` ranks CHUNKS and a note legitimately appears
/// several times in one answer, so collapsing by path would change the payload's shape
/// (and silently drop a note's second-best passage). Across mounts the distinction is
/// moot — paths are namespaced — so this choice only affects fusion WITHIN a mount, where
/// keeping chunks distinct is what the scoped payload already does.
pub type CandidateKey = (String, usize);

/// One page of a mount's ranked candidates.
pub struct CandidatePage {
    /// Candidate keys in the mount's own rank order, best first.
    pub keys: Vec<CandidateKey>,
    /// True when the mount has nothing after this page.
    ///
    /// A SHORT page is not by itself the end: a backend may return fewer hits than asked
    /// for and still offer a cursor. Only this flag ends the deepening for a mount, which
    /// is why a source that can neither fill a page nor promise more must set it — see
    /// [`federate`]'s empty-page guard for the loop-safety net.
    pub exhausted: bool,
}

/// Where one round's pages come from.
///
/// A trait rather than a closure because the fetch borrows the source mutably for the
/// duration of its future (a native mount advances a cursor), which a `FnMut` returning a
/// future cannot express.
#[allow(async_fn_in_trait)]
pub trait CandidateSource {
    /// The next page for the mount at `list_index`, asking for at most `page_size`
    /// candidates.
    async fn next_page(
        &mut self,
        list_index: usize,
        page_size: usize,
    ) -> Result<CandidatePage, String>;
}

/// One mount's contribution, as fusion sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct MountList {
    pub mount_id: String,
    /// This mount's RRF weight. Validated positive and finite by config.
    pub weight: f64,
    /// Every candidate seen so far, in the mount's own rank order. Position IS the
    /// 0-based rank.
    pub keys: Vec<CandidateKey>,
    /// True when this mount reported that its candidates are ALL of them. The honesty
    /// carrier the payload renders — see
    /// [`RecallSearchResponse::exhausted`](deep_obsidian_backend::RecallSearchResponse::exhausted).
    pub exhausted: bool,
    /// Loop bookkeeping: this mount will not be asked again.
    ///
    /// # Why this is not the same field as `exhausted`
    ///
    /// A LOCAL-index mount can be closed without being exhausted, and the difference is
    /// caller-visible. Its candidate pool is derived from the limit its one query was
    /// issued with, so re-querying at a larger limit produces a DIFFERENT ranking rather
    /// than a continuation — there is no cursor to deepen with. When such a mount fills
    /// its candidate page exactly, the honest report is `exhausted: false` ("there were
    /// more, and this is not all of them") together with `closed: true` ("and I cannot
    /// fetch them"). Collapsing the two would either claim completeness the mount never
    /// promised, or spin the loop asking a source that has nothing left to give.
    pub closed: bool,
    /// The failure that stopped this mount, if any. A mount with an error is excluded
    /// from further rounds and named in the payload.
    pub error: Option<String>,
}

impl MountList {
    /// A mount that has been asked nothing yet.
    pub fn new(mount_id: impl Into<String>, weight: f64) -> Self {
        Self {
            mount_id: mount_id.into(),
            weight,
            keys: Vec::new(),
            exhausted: false,
            closed: false,
            error: None,
        }
    }

    /// The largest fused contribution this mount's NEXT unseen candidate could make.
    ///
    /// Its rank is at least `keys.len()` (0-based) and rank only grows, so this is an
    /// upper bound on everything the mount has left — which is what makes the stability
    /// test sound rather than a guess.
    fn best_possible_next(&self) -> f64 {
        self.weight / (FEDERATION_RRF_K + self.keys.len() as f64)
    }

    /// True when this mount can still be asked for candidates.
    fn is_open(&self) -> bool {
        !self.closed && self.error.is_none()
    }
}

/// One fused hit: which mount produced it, where it sat in that mount's list, and the
/// scores that placed it here.
#[derive(Debug, Clone, PartialEq)]
pub struct FusedHit {
    pub mount_id: String,
    pub key: CandidateKey,
    /// The 0-based rank this candidate held in its mount's own list.
    pub mount_rank: usize,
    /// The weighted-RRF score from the FUSION stage. Comparable only within one federated
    /// response.
    pub score: f64,
    /// The score from the final RERANK stage, when one ran. `None` means the answer is in
    /// pure fusion order — see [`RerankStage`].
    ///
    /// Kept alongside `score` rather than replacing it because the two answer different
    /// questions: `score` says where fusion put this hit, `rerank_score` says where a
    /// mount-independent scorer put it, and the second is only interpretable next to the
    /// first.
    pub rerank_score: Option<f64>,
}

/// The result of one federated query.
#[derive(Debug, Clone, PartialEq)]
pub struct FederationOutcome {
    /// The fused top-`limit`, best first.
    pub hits: Vec<FusedHit>,
    /// Every mount's final state, in mount-id order.
    pub mounts: Vec<MountList>,
    /// How many candidates were fetched in total, across every mount and page.
    pub candidates_fetched: usize,
    /// True when the loop stopped because [`candidate_budget`] was reached.
    pub budget_reached: bool,
    /// True when the loop stopped with the frontier NOT stable — i.e. some mount could
    /// still have produced a hit that belongs in the top `limit`. Only ever true together
    /// with `budget_reached`; reported separately because it is the part that affects the
    /// ANSWER rather than the work done.
    pub frontier_unstable: bool,
}

impl FederationOutcome {
    /// The ids of mounts whose answer is missing or incomplete, in mount-id order.
    pub fn missing_mounts(&self) -> Vec<&str> {
        self.mounts
            .iter()
            .filter(|mount| mount.error.is_some())
            .map(|mount| mount.mount_id.as_str())
            .collect()
    }
}

/// Weighted Reciprocal Rank Fusion over per-mount ranked lists, truncated to `limit`.
///
/// Pure: no IO, no ordering dependence on the caller's mount order (the tie-break is
/// total over mount id and path). Separated from [`federate`] so the ranking math is
/// testable without any index or backend.
pub fn fuse(lists: &[MountList], limit: usize) -> Vec<FusedHit> {
    // `(fused score, best mount id, best rank)` per key. A key cannot appear in two
    // mounts' lists today (paths are namespaced), but the accumulation is written as a
    // sum so that stops being an assumption fusion depends on.
    let mut accumulated: BTreeMap<CandidateKey, (f64, String, usize)> = BTreeMap::new();
    for list in lists {
        if list.error.is_some() {
            continue;
        }
        for (rank, key) in list.keys.iter().enumerate() {
            let contribution = list.weight / (FEDERATION_RRF_K + rank as f64);
            match accumulated.get_mut(key) {
                Some(entry) => {
                    entry.0 += contribution;
                    // Attribute the hit to the mount that ranked it best; a rank tie goes
                    // to the lexicographically smaller mount id, matching the sort below.
                    if rank < entry.2 || (rank == entry.2 && list.mount_id < entry.1) {
                        entry.1 = list.mount_id.clone();
                        entry.2 = rank;
                    }
                }
                None => {
                    accumulated.insert(key.clone(), (contribution, list.mount_id.clone(), rank));
                }
            }
        }
    }

    let mut hits: Vec<FusedHit> = accumulated
        .into_iter()
        .map(|(key, (score, mount_id, mount_rank))| FusedHit {
            mount_id,
            key,
            mount_rank,
            score,
            rerank_score: None,
        })
        .collect();
    // The contractual tie-break, in order: score DESC, mount id ASC, logical path ASC,
    // per-mount rank ASC. `total_cmp` rather than `partial_cmp`: a NaN would otherwise
    // make the comparator non-total and the sort order unspecified.
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.mount_id.cmp(&right.mount_id))
            .then_with(|| left.key.0.cmp(&right.key.0))
            .then_with(|| left.mount_rank.cmp(&right.mount_rank))
            .then_with(|| left.key.1.cmp(&right.key.1))
    });
    hits.truncate(limit.max(1));
    hits
}

/// The fused score a new candidate must beat to enter the top `limit`.
///
/// `None` when fewer than `limit` hits have been fused: there is room in the answer, so
/// ANY further candidate belongs in it and no mount can be stable yet.
///
/// Indexed at `limit - 1` rather than read off the END of `hits`, because `hits` may be a
/// wider RERANK WINDOW than the answer (see [`federate_with_window`]). Taking the last
/// element would then quote a score from outside the answer, lower the cutoff, and make the
/// loop deepen for candidates that could never have changed anything.
fn frontier_cutoff(hits: &[FusedHit], limit: usize) -> Option<f64> {
    let limit = limit.max(1);
    if hits.len() < limit {
        return None;
    }
    hits.get(limit - 1).map(|hit| hit.score)
}

/// Indices of the mounts that must be asked for another page, in list order.
///
/// See the module docs, point 5. Exposed for the unit tests, which assert the stability
/// condition directly rather than inferring it from a fetch count.
pub fn unstable_mounts(lists: &[MountList], hits: &[FusedHit], limit: usize) -> Vec<usize> {
    let cutoff = frontier_cutoff(hits, limit);
    lists
        .iter()
        .enumerate()
        .filter(|(_, list)| list.is_open())
        .filter(|(_, list)| match cutoff {
            None => true,
            // `>=`, not `>`: the requirement is that the candidate COULD displace the
            // current cutoff, and an exact tie is decided by the mount-id/path tie-break,
            // which can go either way. Deepening on the tie is the answer-preserving
            // choice; skipping it would drop a hit that the tie-break would have kept.
            Some(cutoff) => list.best_possible_next() >= cutoff,
        })
        .map(|(index, _)| index)
        .collect()
}

// ---------------------------------------------------------------------------
// The final rerank
// ---------------------------------------------------------------------------

/// Floor on how many fused candidates the rerank is given.
const FEDERATION_RERANK_MIN_WINDOW: usize = 50;

/// How many fused candidates the rerank sees: `max(limit, 50)`.
///
/// It must be at least `limit`, or the rerank could not reorder the answer at all. Wider than
/// `limit` is what lets it PROMOTE a candidate fusion placed just outside — the case rank
/// interleaving creates, where a mount's best hit lands one position lower per competing
/// mount. 50 is the recall tools' own maximum `limit`, so the window is never wider than a
/// single answer could be, and it covers interleaving across far more mounts than a vault has.
///
/// The window costs no extra FETCHING (see [`federate_with_window`]); it costs one stored-vector
/// lookup per candidate, plus one batched embedding call for candidates that have no stored
/// vector.
pub fn rerank_window(limit: usize) -> usize {
    limit.max(FEDERATION_RERANK_MIN_WINDOW)
}

/// Which stage produced the order a caller is reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerankStage {
    /// Pure fusion order. Either the rerank is switched off, or there was no semantic signal
    /// to rerank with — [`RerankOutcome::degraded`] distinguishes those.
    None,
    /// Reranked by a mount-independent semantic + lexical scorer.
    SemanticAndLexical,
}

impl RerankStage {
    /// How the payload names this stage.
    pub fn as_str(self) -> &'static str {
        match self {
            RerankStage::None => "none",
            RerankStage::SemanticAndLexical => "semantic+lexical",
        }
    }
}

/// What the rerank did, and whether anything was lost doing it.
#[derive(Debug, Clone, PartialEq)]
pub struct RerankOutcome {
    pub stage: RerankStage,
    /// True when the rerank was WANTED and could not run — the query could not be embedded,
    /// so the answer is in fusion order and its top-1 is subject to rank interleaving.
    ///
    /// # Why this is separate from `stage == None`
    ///
    /// "We could not rerank" and "there is nothing to rerank with" are different facts. A
    /// deployment with no embedding backend configured has a lexical-only pipeline by
    /// choice; nothing is missing and the answer is exactly what it should be. A deployment
    /// WITH one whose backend is down has lost the ordering signal it normally has, and a
    /// caller comparing today's answer with yesterday's needs to know.
    pub degraded: bool,
    /// Why, when `degraded`.
    pub reason: Option<String>,
    /// How many candidates carried a semantic score. `0` with `stage: None` on a lexical-only
    /// deployment is the ordinary case, not a fault.
    pub semantic_signals: usize,
}

impl RerankOutcome {
    /// The rerank did not run, and nothing was lost by that.
    pub fn not_applicable() -> Self {
        Self {
            stage: RerankStage::None,
            degraded: false,
            reason: None,
            semantic_signals: 0,
        }
    }

    /// The rerank was wanted and could not run.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            stage: RerankStage::None,
            degraded: true,
            reason: Some(reason.into()),
            semantic_signals: 0,
        }
    }
}

/// Per-candidate rerank signals, parallel to the fused hit list.
///
/// # Why the signals arrive from outside
///
/// Gathering them needs an index handle, a SQLite connection and possibly an HTTP call —
/// none of which belong in the ranking math. Keeping this a pure function of
/// `(candidates, signals)` is what makes the tie-break, the NaN handling and the
/// order-independence testable without a vault, and what makes the determinism claim
/// checkable at all.
#[derive(Debug, Clone, PartialEq)]
pub struct RerankSignals {
    /// Semantic score per candidate, on the local index's own scale. `None` for a candidate
    /// with no vector — that candidate is simply ABSENT from the semantic list, which under
    /// RRF contributes nothing, exactly as a document missing from one retriever's list
    /// already does.
    pub semantic: Vec<Option<f64>>,
    /// Lexical score per candidate, computed over the candidate set as its own corpus.
    pub lexical: Vec<f64>,
}

/// Rerank the fused candidates with a mount-independent scorer, then truncate to `limit`.
///
/// # The combination is the LOCAL hybrid ranker's, deliberately
///
/// Two ranked lists (semantic, lexical) fused by Reciprocal Rank Fusion at the same
/// `RRF_K = 60` the local index uses. That is not a coincidence to be tidied up: the gate this
/// stage exists to satisfy compares a federated answer against a UNIFIED single-vault answer
/// produced by exactly that formula, so mirroring it is what makes the two comparable. The
/// rank convention here is 1-BASED, matching the local ranker; the fusion stage above is
/// 0-based, matching the RFC. Both are internally consistent and neither convention changes
/// any ordering — it shifts every contribution by the same denominator step.
///
/// # What this stage does NOT reproduce
///
/// The local ranker also applies a small graph-proximity bonus to candidates one wikilink hop
/// from its top notes. That cannot be reproduced across mounts: a link from a note on one
/// mount to a note on another is not an edge in EITHER index, so a fused-set bonus would fire
/// for intra-mount pairs and never for cross-mount ones — asymmetric by construction, and
/// biased towards whichever mount happens to hold both ends. It is left out rather than
/// approximated, and it is the first place to look if a federated ranking sits slightly below
/// its unified baseline.
///
/// # Ordering
///
/// Rerank score DESC, then the FUSION score DESC, then the same total tie-break chain
/// [`fuse`] uses. Keeping the fused score as the first tie-break means the rerank refines the
/// fusion rather than replacing it: two candidates the scorer cannot separate stay in the
/// order fusion chose.
pub fn rerank(hits: &mut Vec<FusedHit>, signals: &RerankSignals, limit: usize) -> RerankOutcome {
    let limit = limit.max(1);
    if hits.is_empty() {
        return RerankOutcome::not_applicable();
    }
    debug_assert_eq!(signals.semantic.len(), hits.len());
    debug_assert_eq!(signals.lexical.len(), hits.len());

    // A NaN anywhere would make the comparator below non-total and the sort order
    // unspecified, so it is treated as "no signal" rather than propagated.
    let semantic: Vec<Option<f64>> = (0..hits.len())
        .map(|index| {
            signals
                .semantic
                .get(index)
                .copied()
                .flatten()
                .filter(|score| score.is_finite())
        })
        .collect();
    // A candidate with NO lexical evidence is ABSENT from the lexical list, not present with
    // a score of zero.
    //
    // # This distinction is the whole rerank
    //
    // `ranked_by` breaks ties by candidate index, and the candidate order it is given is the
    // FUSED order — which is decided by the mount-id tie-break. So admitting zero-scoring
    // candidates would hand rank 1 of the lexical list to whichever mount sorts first,
    // regardless of whether its hit contains a single query term, and the rerank would
    // faithfully reproduce the interleaving bias it exists to remove. It also matches the
    // local ranker, whose `bm25_search` returns only positively-scoring documents: a document
    // containing none of the query's terms is not at the bottom of its BM25 list, it is not
    // in it.
    let lexical: Vec<Option<f64>> = (0..hits.len())
        .map(|index| {
            signals
                .lexical
                .get(index)
                .copied()
                .filter(|score| score.is_finite() && *score > 0.0)
        })
        .collect();

    let semantic_signals = semantic.iter().filter(|score| score.is_some()).count();
    if semantic_signals == 0 {
        // Nothing to rerank WITH. The lexical list alone would re-order the answer using a
        // candidate-set-local BM25 and no dense signal, which is a different ranking from
        // either the fusion order or the unified baseline — worse than leaving fusion's
        // order alone, and it would claim a rerank happened.
        hits.truncate(limit);
        return RerankOutcome::not_applicable();
    }

    // Rank each signal independently, best first, and give every candidate its RRF
    // contribution from the lists it appears in. A candidate absent from a list gets no term
    // from it -- the same rule `fuse` and the local ranker both follow.
    let mut score = vec![0.0_f64; hits.len()];
    for (rank, index) in ranked_by(&semantic).into_iter().enumerate() {
        score[index] += 1.0 / (FEDERATION_RRF_K + (rank + 1) as f64);
    }
    for (rank, index) in ranked_by(&lexical).into_iter().enumerate() {
        score[index] += 1.0 / (FEDERATION_RRF_K + (rank + 1) as f64);
    }

    for (index, hit) in hits.iter_mut().enumerate() {
        hit.rerank_score = Some(score[index]);
    }
    hits.sort_by(|left, right| {
        let left_score = left.rerank_score.unwrap_or(0.0);
        let right_score = right.rerank_score.unwrap_or(0.0);
        right_score
            .total_cmp(&left_score)
            // Fusion's own order is the first tie-break: the rerank REFINES fusion, so two
            // candidates it cannot separate keep the order fusion gave them.
            .then_with(|| right.score.total_cmp(&left.score))
            .then_with(|| left.mount_id.cmp(&right.mount_id))
            .then_with(|| left.key.0.cmp(&right.key.0))
            .then_with(|| left.mount_rank.cmp(&right.mount_rank))
            .then_with(|| left.key.1.cmp(&right.key.1))
    });
    hits.truncate(limit);
    RerankOutcome {
        stage: RerankStage::SemanticAndLexical,
        degraded: false,
        reason: None,
        semantic_signals,
    }
}

/// Candidate indices ordered by a signal, best first, dropping candidates with no signal.
///
/// Ties break by candidate INDEX, which is the fused order — so the ranked list is a total
/// function of its input and does not depend on the sort's stability.
fn ranked_by(signal: &[Option<f64>]) -> Vec<usize> {
    let mut ordered: Vec<(usize, f64)> = signal
        .iter()
        .enumerate()
        .filter_map(|(index, score)| score.map(|score| (index, score)))
        .collect();
    ordered.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    ordered.into_iter().map(|(index, _)| index).collect()
}

/// The canonical order [`federate`] drives: mount id ascending.
///
/// Returned as a PERMUTATION rather than applied in place because the caller holds
/// per-mount state (an index snapshot, a backend handle, a cursor) that has to move with
/// it, and `list_index` in [`CandidateSource::next_page`] indexes the reordered table.
pub fn canonical_order(mounts: &[MountList]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..mounts.len()).collect();
    order.sort_by(|&left, &right| mounts[left].mount_id.cmp(&mounts[right].mount_id));
    order
}

/// Run the federated query: fetch, fuse, deepen until the frontier is stable or the
/// candidate budget is spent.
///
/// `mounts` MUST already be in [`canonical_order`] and `source`'s `list_index` refers to
/// positions in it; the caller reorders both together so its per-mount state stays
/// aligned. `federate` re-sorts defensively so a caller that forgot cannot make the
/// output depend on config order.
pub async fn federate<S: CandidateSource>(
    mounts: Vec<MountList>,
    limit: usize,
    source: &mut S,
) -> FederationOutcome {
    federate_with_window(mounts, limit, limit, source).await
}

/// [`federate`], keeping the top `window` fused hits rather than the top `limit`.
///
/// # Why the window and the limit are different numbers
///
/// The frontier's stability is defined against `limit` — the cutoff is the fused score at
/// position `limit - 1`, because that is the score a new candidate has to beat to change the
/// ANSWER. That definition does not change here.
///
/// What changes is how many fused hits come back. A final rerank
/// ([`rerank`]) can only reorder candidates it is given, so handing it exactly `limit`
/// candidates would let it fix the order of the answer but never promote a candidate that
/// fusion placed just outside it — and "just outside" is precisely where rank interleaving
/// pushes a good hit when several mounts each offer an equally-ranked one. So the caller asks
/// for a wider window, reranks it, and truncates to `limit` itself.
///
/// Fetching is unaffected: no extra pages are read for the window, it only stops discarding
/// candidates the deepening loop already paid for.
pub async fn federate_with_window<S: CandidateSource>(
    mut mounts: Vec<MountList>,
    limit: usize,
    window: usize,
    source: &mut S,
) -> FederationOutcome {
    // Defensive, and cheap: a handful of mounts. If the caller already sorted, this is a
    // no-op; if it did not, the fused output is still order-independent.
    debug_assert!(
        mounts
            .windows(2)
            .all(|pair| pair[0].mount_id <= pair[1].mount_id),
        "federate expects mounts in canonical_order"
    );
    mounts.sort_by(|left, right| left.mount_id.cmp(&right.mount_id));

    let limit = limit.max(1);
    let page_size = candidate_target(limit);
    let budget = candidate_budget(limit);
    let mut fetched = 0_usize;
    let mut budget_reached = false;

    // Round 0 asks every mount; subsequent rounds ask only the unstable ones.
    let mut to_fetch: Vec<usize> = (0..mounts.len()).collect();
    loop {
        for index in to_fetch {
            // Checked BEFORE the fetch, not after: a check afterwards would always spend
            // one page more than the budget allows, and with a large `page_size` that is
            // not a rounding difference.
            if fetched >= budget {
                budget_reached = true;
                break;
            }
            match source.next_page(index, page_size).await {
                Ok(page) => {
                    let mount = &mut mounts[index];
                    fetched += page.keys.len();
                    // `exhausted` is the SOURCE's own claim and is recorded verbatim;
                    // `closed` is the loop's. An EMPTY page closes the mount whatever it
                    // claims about a cursor: without that, a source that keeps offering a
                    // cursor it cannot honour would spin forever, and "I have more" plus
                    // "here is none of it" is not a state the loop can progress from. It
                    // does NOT overwrite `exhausted`, because a mount that truncated and
                    // said so must keep saying so in the payload.
                    mount.exhausted = page.exhausted;
                    mount.closed = page.exhausted || page.keys.is_empty();
                    mount.keys.extend(page.keys);
                }
                Err(error) => {
                    // Partial, named, and never asked again. Failing the whole query here
                    // would turn one unreachable mount into no answer at all.
                    mounts[index].error = Some(error);
                }
            }
        }

        // Fused to the WINDOW so a rerank has candidates to promote from; stability is
        // still judged against `limit`, which is the frontier the algorithm defines.
        let hits = fuse(&mounts, window.max(limit));
        let unstable = unstable_mounts(&mounts, &hits, limit);
        if unstable.is_empty() {
            return FederationOutcome {
                hits,
                mounts,
                candidates_fetched: fetched,
                budget_reached,
                frontier_unstable: false,
            };
        }
        if budget_reached || fetched >= budget {
            return FederationOutcome {
                hits,
                mounts,
                candidates_fetched: fetched,
                budget_reached: true,
                frontier_unstable: true,
            };
        }
        to_fetch = unstable;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic mount whose full ranked list is known up front and handed out in fixed
    /// pages.
    ///
    /// # Why the scenario tests use this rather than real indexes
    ///
    /// [`candidate_target`] is at least 100 candidates per mount, and the deterministic
    /// eval corpus has about a dozen notes per mount — so against real indexes every mount
    /// is exhausted on its first page and the deepening loop, the budget ceiling and the
    /// tie-break never run. Driving them from synthetic lists is the only way to exercise
    /// them at all, and it is exact rather than approximate: the page boundaries, the
    /// ranks and the weights are all stated by the test.
    ///
    /// A short page with `exhausted: false` is a legitimate backend answer (see
    /// [`CandidatePage::exhausted`]), which is what lets `page` be small here while the
    /// caller asks for `candidate_target`.
    struct PagedSource {
        /// Full ranked list per mount, in canonical (mount-id) order.
        lists: Vec<Vec<CandidateKey>>,
        /// How many candidates each page hands out.
        page: usize,
        /// How many candidates each mount has already handed out.
        served: Vec<usize>,
        /// Mounts that answer with an error instead of a page.
        failing: Vec<usize>,
        /// Every `(list_index, page_size)` ask, in order. The fetch trace.
        asks: Vec<(usize, usize)>,
    }

    impl PagedSource {
        fn new(lists: Vec<Vec<CandidateKey>>, page: usize) -> Self {
            let served = vec![0; lists.len()];
            Self {
                lists,
                page,
                served,
                failing: Vec::new(),
                asks: Vec::new(),
            }
        }

        fn failing(mut self, list_index: usize) -> Self {
            self.failing.push(list_index);
            self
        }
    }

    impl CandidateSource for PagedSource {
        async fn next_page(
            &mut self,
            list_index: usize,
            page_size: usize,
        ) -> Result<CandidatePage, String> {
            self.asks.push((list_index, page_size));
            if self.failing.contains(&list_index) {
                return Err(format!("mount {list_index} is unreachable"));
            }
            let list = &self.lists[list_index];
            let start = self.served[list_index];
            let end = (start + self.page.min(page_size)).min(list.len());
            self.served[list_index] = end;
            Ok(CandidatePage {
                keys: list[start..end].to_vec(),
                exhausted: end >= list.len(),
            })
        }
    }

    fn key(path: &str) -> CandidateKey {
        (path.to_string(), 0)
    }

    fn keys(paths: &[&str]) -> Vec<CandidateKey> {
        paths.iter().map(|path| key(path)).collect()
    }

    fn paths(hits: &[FusedHit]) -> Vec<&str> {
        hits.iter().map(|hit| hit.key.0.as_str()).collect()
    }

    /// A synthetic vault of `count` notes per mount, so a list is longer than one page.
    fn synthetic(mount: &str, count: usize) -> Vec<CandidateKey> {
        (0..count)
            .map(|position| (format!("{mount}/Note {position:03}.md"), 0))
            .collect()
    }

    // -----------------------------------------------------------------------
    // The fusion math
    // -----------------------------------------------------------------------

    #[test]
    fn equal_weights_interleave_the_two_mounts_rank_for_rank() {
        let alpha = MountList {
            keys: keys(&["Alpha/A.md", "Alpha/B.md"]),
            exhausted: true,
            closed: true,
            ..MountList::new("alpha", 1.0)
        };
        let beta = MountList {
            keys: keys(&["Beta/A.md", "Beta/B.md"]),
            exhausted: true,
            closed: true,
            ..MountList::new("beta", 1.0)
        };
        let hits = fuse(&[alpha, beta], 4);
        // Rank 0 of both mounts scores 1/60 exactly, so the tie-break (mount id ASC)
        // decides — and it decides the same way every time, which is the point.
        assert_eq!(
            paths(&hits),
            vec!["Alpha/A.md", "Beta/A.md", "Alpha/B.md", "Beta/B.md"]
        );
    }

    #[test]
    fn a_heavier_mount_outranks_an_equally_ranked_lighter_one() {
        // `zzz` sorts LAST, so if it wins the ordering it can only be the weight: the
        // mount-id tie-break would have put it second.
        let light = MountList {
            keys: keys(&["Light/A.md"]),
            exhausted: true,
            closed: true,
            ..MountList::new("aaa-light", 1.0)
        };
        let heavy = MountList {
            keys: keys(&["Heavy/A.md"]),
            exhausted: true,
            closed: true,
            ..MountList::new("zzz-heavy", 2.0)
        };
        let hits = fuse(&[light.clone(), heavy.clone()], 2);
        assert_eq!(paths(&hits), vec!["Heavy/A.md", "Light/A.md"]);
        assert_eq!(hits[0].score, 2.0 / 60.0);
        assert_eq!(hits[1].score, 1.0 / 60.0);

        // And with equal weights the same table orders by mount id instead, which is what
        // proves the assertion above is about the weight and not about the paths.
        let unweighted = fuse(
            &[
                light,
                MountList {
                    weight: 1.0,
                    ..heavy
                },
            ],
            2,
        );
        assert_eq!(paths(&unweighted), vec!["Light/A.md", "Heavy/A.md"]);
    }

    #[test]
    fn a_two_point_zero_weight_lifts_a_deeper_hit_over_a_lighter_mounts_better_one() {
        // Weight 2.0 at rank 1 scores 2/61 = 0.0328; weight 1.0 at rank 0 scores 1/60 =
        // 0.0167. So the heavy mount's SECOND-best hit outranks the light mount's best.
        let light = MountList {
            keys: keys(&["Light/Best.md"]),
            exhausted: true,
            closed: true,
            ..MountList::new("light", 1.0)
        };
        let heavy = MountList {
            keys: keys(&["Heavy/Best.md", "Heavy/Second.md"]),
            exhausted: true,
            closed: true,
            ..MountList::new("heavy", 2.0)
        };
        assert_eq!(
            paths(&fuse(&[light, heavy], 3)),
            vec!["Heavy/Best.md", "Heavy/Second.md", "Light/Best.md"]
        );
    }

    #[test]
    fn a_failed_mount_contributes_nothing_to_fusion() {
        let healthy = MountList {
            keys: keys(&["Ok/A.md"]),
            exhausted: true,
            closed: true,
            ..MountList::new("healthy", 1.0)
        };
        let broken = MountList {
            keys: keys(&["Broken/A.md"]),
            error: Some("unreachable".to_string()),
            ..MountList::new("broken", 1.0)
        };
        // `broken` sorts first and holds a rank-0 candidate, so including it would put
        // `Broken/A.md` at the top. Its error excludes it entirely.
        assert_eq!(paths(&fuse(&[broken, healthy], 5)), vec!["Ok/A.md"]);
    }

    #[test]
    fn chunks_of_one_note_stay_distinct_hits() {
        let mount = MountList {
            keys: vec![("Note.md".to_string(), 3), ("Note.md".to_string(), 0)],
            exhausted: true,
            closed: true,
            ..MountList::new("only", 1.0)
        };
        let hits = fuse(&[mount], 5);
        assert_eq!(hits.len(), 2, "keying by path alone would collapse these");
        // Rank order is preserved: chunk 3 was the better hit.
        assert_eq!(hits[0].key, ("Note.md".to_string(), 3));
    }

    // -----------------------------------------------------------------------
    // The stability condition
    // -----------------------------------------------------------------------

    #[test]
    fn a_mount_is_unstable_while_the_answer_is_still_short() {
        let mount = MountList {
            keys: keys(&["A/1.md"]),
            ..MountList::new("a", 1.0)
        };
        let lists = vec![mount];
        let hits = fuse(&lists, 5);
        assert_eq!(hits.len(), 1);
        // One hit for a limit of 5: there is room, so nothing can be stable yet.
        assert_eq!(unstable_mounts(&lists, &hits, 5), vec![0]);
    }

    #[test]
    fn a_mount_whose_next_candidate_cannot_reach_the_cutoff_is_stable() {
        // `deep` has already produced 200 candidates, so its next one is worth
        // 1/260 = 0.0038. `shallow` fills the whole top-2 with 1/60 and 1/61, so the
        // cutoff is 0.0164 -- far above what `deep` has left.
        let deep = MountList {
            keys: synthetic("Deep", 200),
            ..MountList::new("deep", 1.0)
        };
        let shallow = MountList {
            keys: keys(&["Shallow/A.md", "Shallow/B.md"]),
            exhausted: true,
            closed: true,
            ..MountList::new("aaa-shallow", 1.0)
        };
        let lists = vec![shallow, deep];
        // limit 2. Both mounts hold a rank-0 candidate worth 1/60, and `aaa-shallow` wins
        // that tie on mount id, so the cutoff is `deep`'s own rank-0 score.
        let hits = fuse(&lists, 2);
        assert_eq!(paths(&hits), vec!["Shallow/A.md", "Deep/Note 000.md"]);
        assert!(unstable_mounts(&lists, &hits, 2).is_empty());

        // Raise the limit past what the two mounts together can supply (202 candidates for
        // a limit of 250) and there is no cutoff at all, so `deep` is unstable again --
        // the same table, the same ranks, a different answer size.
        assert_eq!(unstable_mounts(&lists, &fuse(&lists, 250), 250), vec![1]);
    }

    #[test]
    fn an_exhausted_or_failed_mount_is_never_unstable() {
        let exhausted = MountList {
            keys: keys(&["A/1.md"]),
            exhausted: true,
            closed: true,
            ..MountList::new("a-exhausted", 1.0)
        };
        let failed = MountList {
            error: Some("gone".to_string()),
            ..MountList::new("b-failed", 1.0)
        };
        let lists = vec![exhausted, failed];
        // The answer is short, so the "there is room" branch would call everything
        // unstable if it did not check `is_open` first.
        assert!(unstable_mounts(&lists, &fuse(&lists, 20), 20).is_empty());
    }

    // -----------------------------------------------------------------------
    // The deepening loop
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn deepening_finds_a_best_answer_beyond_a_mounts_first_page() {
        // `beta` hands out 3 candidates per page and its 7th candidate (rank 6) is the one
        // that belongs in a top-10 answer. Without deepening only ranks 0..3 would ever be
        // fused and the hit would be invisible.
        let mut beta_list = synthetic("Beta", 6);
        beta_list.push(key("Beta/Buried Answer.md"));
        let mut source = PagedSource::new(vec![synthetic("Alpha", 2), beta_list], 3);
        let mounts = vec![MountList::new("alpha", 1.0), MountList::new("beta", 1.0)];

        let outcome = federate(mounts, 10, &mut source).await;
        assert!(
            paths(&outcome.hits).contains(&"Beta/Buried Answer.md"),
            "deepening must reach rank 6 of the second mount: {:?}",
            paths(&outcome.hits)
        );
        // It really took more than one round for that mount.
        let beta_asks = source.asks.iter().filter(|(index, _)| *index == 1).count();
        assert!(beta_asks >= 3, "beta was asked {beta_asks} times");
        assert!(!outcome.frontier_unstable);
        assert!(!outcome.budget_reached);
    }

    #[tokio::test]
    async fn an_exhausted_mount_is_never_asked_again() {
        // `alpha` is exhausted after its first (short) page while `beta` keeps paging.
        let mut source = PagedSource::new(vec![synthetic("Alpha", 2), synthetic("Beta", 30)], 3);
        let mounts = vec![MountList::new("alpha", 1.0), MountList::new("beta", 1.0)];
        let outcome = federate(mounts, 20, &mut source).await;
        assert_eq!(
            source.asks.iter().filter(|(index, _)| *index == 0).count(),
            1,
            "alpha said it was exhausted on page 1"
        );
        assert!(outcome.mounts[0].exhausted);
        assert_eq!(outcome.mounts[0].keys.len(), 2);
    }

    #[tokio::test]
    async fn budget_exhaustion_terminates_and_says_the_frontier_is_unstable() {
        /// A mount that pages forever and keeps re-serving the same three notes.
        ///
        /// # Why the budget needs a source this pathological to be reachable
        ///
        /// A single well-behaved mount stabilizes as soon as it has `limit` candidates: its
        /// cutoff is then `w/(60 + limit - 1)` and its next candidate is worth
        /// `w/(60 + limit)`, which is strictly smaller. So the budget is not reachable by
        /// asking a cooperative backend for more pages — it is the guard against a backend
        /// whose pagination does not converge, which is exactly what a repeating page is.
        struct RepeatingPages {
            pages: usize,
        }
        impl CandidateSource for RepeatingPages {
            async fn next_page(
                &mut self,
                _list_index: usize,
                _page_size: usize,
            ) -> Result<CandidatePage, String> {
                self.pages += 1;
                Ok(CandidatePage {
                    keys: keys(&["Loop/A.md", "Loop/B.md", "Loop/C.md"]),
                    exhausted: false,
                })
            }
        }
        let mut source = RepeatingPages { pages: 0 };
        // limit 50 => budget 2500. Only three DISTINCT notes ever arrive, so the fused list
        // never fills to 50, there is never a cutoff, and the mount is unstable forever.
        let outcome = federate(vec![MountList::new("loop", 1.0)], 50, &mut source).await;
        assert!(outcome.budget_reached, "the budget must stop this loop");
        assert!(
            outcome.frontier_unstable,
            "it stopped with the frontier still open"
        );
        // The budget bounds when fetching STOPS, so the page already in flight is counted:
        // the total lands within one page of the ceiling, never further.
        assert!(
            outcome.candidates_fetched >= candidate_budget(50)
                && outcome.candidates_fetched < candidate_budget(50) + 3,
            "fetched {} against budget {}",
            outcome.candidates_fetched,
            candidate_budget(50)
        );
        // The answer is still the honest three notes, deduplicated by candidate key.
        assert_eq!(
            paths(&outcome.hits),
            vec!["Loop/A.md", "Loop/B.md", "Loop/C.md"]
        );
        assert!(source.pages > 1, "the loop really did deepen");
    }

    #[tokio::test]
    async fn a_source_that_offers_a_cursor_but_no_candidates_does_not_spin() {
        /// Always claims there is more, always returns nothing. The pathological source
        /// the empty-page guard exists for.
        struct EmptyForever;
        impl CandidateSource for EmptyForever {
            async fn next_page(
                &mut self,
                _list_index: usize,
                _page_size: usize,
            ) -> Result<CandidatePage, String> {
                Ok(CandidatePage {
                    keys: Vec::new(),
                    exhausted: false,
                })
            }
        }
        let outcome = federate(vec![MountList::new("empty", 1.0)], 5, &mut EmptyForever).await;
        assert!(outcome.hits.is_empty());
        // CLOSED, but never `exhausted`: the source never claimed completeness and the
        // loop must not invent the claim on its behalf.
        assert!(outcome.mounts[0].closed);
        assert!(!outcome.mounts[0].exhausted);
        assert!(!outcome.budget_reached);
    }

    #[tokio::test]
    async fn one_unavailable_mount_leaves_the_others_answer_intact_and_named() {
        let mut source =
            PagedSource::new(vec![synthetic("Alpha", 4), synthetic("Beta", 4)], 4).failing(1);
        let mounts = vec![MountList::new("alpha", 1.0), MountList::new("beta", 1.0)];
        let outcome = federate(mounts, 10, &mut source).await;
        assert_eq!(outcome.missing_mounts(), vec!["beta"]);
        assert_eq!(paths(&outcome.hits), paths(&outcome.hits.clone()));
        assert!(
            paths(&outcome.hits)
                .iter()
                .all(|path| path.starts_with("Alpha/")),
            "{:?}",
            paths(&outcome.hits)
        );
        // The surviving mount's own ordering is untouched by the failure.
        assert_eq!(
            paths(&outcome.hits),
            vec![
                "Alpha/Note 000.md",
                "Alpha/Note 001.md",
                "Alpha/Note 002.md",
                "Alpha/Note 003.md"
            ]
        );
    }

    // -----------------------------------------------------------------------
    // Deterministic replay
    // -----------------------------------------------------------------------

    /// Permuting the mount table changes nothing observable, THROUGH THE RERANK.
    ///
    /// The whole pipeline, not just fusion: `federate_with_window` then `rerank`. Extending
    /// this test rather than adding a sibling is deliberate — the invariance claim is about one
    /// code path, and two tests each covering half of it would not establish it.
    #[tokio::test]
    async fn permuting_the_mount_table_changes_nothing_observable() {
        // Three mounts of different depths and weights, fed in every order. The fused
        // hits, the per-mount candidate counts and the total fetched must all match.
        let lists = |order: [usize; 3]| {
            let all = [
                (MountList::new("alpha", 1.0), synthetic("Alpha", 9)),
                (MountList::new("beta", 2.0), synthetic("Beta", 25)),
                (MountList::new("gamma", 1.5), synthetic("Gamma", 4)),
            ];
            let mut mounts = Vec::new();
            let mut candidates = Vec::new();
            for index in order {
                let (mount, list) = all[index].clone();
                mounts.push(mount);
                candidates.push(list);
            }
            (mounts, candidates)
        };

        let mut reference: Option<FederationOutcome> = None;
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let (mounts, candidates) = lists(order);
            // The caller reorders its own per-mount state with the SAME permutation, which
            // is what `canonical_order` is for.
            let permutation = canonical_order(&mounts);
            let mounts: Vec<MountList> = permutation.iter().map(|&i| mounts[i].clone()).collect();
            let candidates: Vec<Vec<CandidateKey>> =
                permutation.iter().map(|&i| candidates[i].clone()).collect();
            let mut source = PagedSource::new(candidates, 5);
            let mut outcome = federate_with_window(mounts, 8, 20, &mut source).await;
            // Rerank the same window with signals derived from the CANDIDATE KEY, so the
            // signals are a property of the candidate rather than of its position -- which is
            // what a real scorer's signals are, and what makes the invariance claim about the
            // whole pipeline rather than about fusion alone.
            let signals = RerankSignals {
                semantic: outcome
                    .hits
                    .iter()
                    .map(|hit| Some(signal_for(&hit.key.0)))
                    .collect(),
                lexical: outcome
                    .hits
                    .iter()
                    .map(|hit| signal_for(&hit.key.0) * 0.5)
                    .collect(),
            };
            let stage = rerank(&mut outcome.hits, &signals, 8);
            assert_eq!(stage.stage, RerankStage::SemanticAndLexical);
            match &reference {
                None => reference = Some(outcome),
                Some(expected) => assert_eq!(
                    &outcome, expected,
                    "config order {order:?} produced a different federated answer"
                ),
            }
        }
        let outcome = reference.expect("at least one run");
        // Non-vacuity: every mount contributed, the answer really was reranked, and the top hit
        // is the one the SIGNALS chose rather than the one fusion's mount-id tie-break chose.
        assert!(outcome.mounts.iter().all(|mount| !mount.keys.is_empty()));
        assert!(outcome.hits.iter().all(|hit| hit.rerank_score.is_some()));
        let best = outcome
            .hits
            .iter()
            .max_by(|left, right| signal_for(&left.key.0).total_cmp(&signal_for(&right.key.0)))
            .expect("a best candidate");
        assert_eq!(outcome.hits[0].key, best.key);
    }

    /// A deterministic per-candidate signal keyed on the candidate's PATH.
    ///
    /// Derived from the path rather than from the candidate's position so that permuting the
    /// mount table cannot change a candidate's signal — otherwise the invariance test would be
    /// asserting that a position-dependent scorer is position-independent, which is vacuous.
    fn signal_for(path: &str) -> f64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in path.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        (hash % 10_000) as f64 / 10_000.0
    }

    // -----------------------------------------------------------------------
    // The final rerank
    // -----------------------------------------------------------------------

    #[test]
    fn the_rerank_promotes_a_candidate_fusion_ranked_last() {
        // Three candidates in fusion order A, B, C. Both rerank signals say C is best and A
        // is worst -- exactly the interleaving case, where fusion's order came from mount ids
        // rather than from relevance.
        let mut hits = vec![
            FusedHit {
                mount_id: "aaa".to_string(),
                key: key("A.md"),
                mount_rank: 0,
                score: 1.0 / 60.0,
                rerank_score: None,
            },
            FusedHit {
                mount_id: "bbb".to_string(),
                key: key("B.md"),
                mount_rank: 0,
                score: 1.0 / 60.0,
                rerank_score: None,
            },
            FusedHit {
                mount_id: "ccc".to_string(),
                key: key("C.md"),
                mount_rank: 0,
                score: 1.0 / 60.0,
                rerank_score: None,
            },
        ];
        let signals = RerankSignals {
            semantic: vec![Some(0.1), Some(0.2), Some(0.9)],
            lexical: vec![0.1, 0.2, 0.9],
        };
        let outcome = rerank(&mut hits, &signals, 3);
        assert_eq!(outcome.stage, RerankStage::SemanticAndLexical);
        assert_eq!(outcome.semantic_signals, 3);
        assert!(!outcome.degraded);
        assert_eq!(paths(&hits), vec!["C.md", "B.md", "A.md"]);
        // The rerank score is the RRF of two rank-1 contributions for the winner.
        let expected = 2.0 / (FEDERATION_RRF_K + 1.0);
        assert!((hits[0].rerank_score.unwrap() - expected).abs() < 1e-12);
    }

    #[test]
    fn the_rerank_truncates_to_the_limit_after_reordering() {
        // A window of 5, an answer of 2: the two best AFTER reranking survive, not the two
        // best before it. That is the whole point of reranking a wider window.
        let mut hits: Vec<FusedHit> = ["A.md", "B.md", "C.md", "D.md", "E.md"]
            .iter()
            .enumerate()
            .map(|(index, path)| FusedHit {
                mount_id: format!("m{index}"),
                key: key(path),
                mount_rank: 0,
                score: 1.0 / (60.0 + index as f64),
                rerank_score: None,
            })
            .collect();
        let signals = RerankSignals {
            semantic: vec![Some(0.1), Some(0.1), Some(0.1), Some(0.9), Some(0.8)],
            lexical: vec![0.1, 0.1, 0.1, 0.9, 0.8],
        };
        rerank(&mut hits, &signals, 2);
        assert_eq!(paths(&hits), vec!["D.md", "E.md"]);
    }

    #[test]
    fn a_candidate_with_no_semantic_signal_is_absent_from_that_list_not_zeroed() {
        // `B` has no vector. It still ranks on the lexical list, where it is best, so it must
        // not be pushed below a candidate that lost BOTH lists -- "absent" and "worst" are
        // different, exactly as they are in `fuse`.
        let mut hits: Vec<FusedHit> = ["A.md", "B.md", "C.md"]
            .iter()
            .map(|path| FusedHit {
                mount_id: "m".to_string(),
                key: key(path),
                mount_rank: 0,
                score: 1.0 / 60.0,
                rerank_score: None,
            })
            .collect();
        let signals = RerankSignals {
            semantic: vec![Some(0.5), None, Some(0.1)],
            lexical: vec![0.2, 0.9, 0.1],
        };
        let outcome = rerank(&mut hits, &signals, 3);
        assert_eq!(outcome.semantic_signals, 2);
        // Semantic list: [A, C]. Lexical list: [B, A, C]. So
        //   A = 1/61 + 1/62 = 0.032522   (best on semantic, second on lexical)
        //   C = 1/62 + 1/63 = 0.032002   (second on semantic, third on lexical)
        //   B = 1/61        = 0.016393   (best on lexical, ABSENT from semantic)
        let a = 1.0 / (FEDERATION_RRF_K + 1.0) + 1.0 / (FEDERATION_RRF_K + 2.0);
        let c = 1.0 / (FEDERATION_RRF_K + 2.0) + 1.0 / (FEDERATION_RRF_K + 3.0);
        let b = 1.0 / (FEDERATION_RRF_K + 1.0);
        assert_eq!(paths(&hits), vec!["A.md", "C.md", "B.md"]);
        assert!((hits[0].rerank_score.unwrap() - a).abs() < 1e-12);
        assert!((hits[1].rerank_score.unwrap() - c).abs() < 1e-12);
        assert!((hits[2].rerank_score.unwrap() - b).abs() < 1e-12);

        // Non-vacuity, and the whole point of "absent" rather than "zeroed": had B's missing
        // score been read as 0.0 it would have entered the semantic list at rank 3 and scored
        // 1/61 + 1/63 = 0.032266, landing it between A and C. The order would have been
        // A, B, C -- so the two readings are distinguishable, and this asserts the right one.
        let b_if_zeroed = 1.0 / (FEDERATION_RRF_K + 1.0) + 1.0 / (FEDERATION_RRF_K + 3.0);
        assert!(b_if_zeroed > c && b_if_zeroed < a);
    }

    #[test]
    fn no_semantic_signal_at_all_leaves_the_fusion_order_untouched() {
        // A lexical-only deployment: nothing was lost, so this is NOT degraded, and the
        // answer stays in fusion order rather than being re-sorted by a candidate-set-local
        // BM25 that would agree with neither fusion nor the unified baseline.
        let mut hits: Vec<FusedHit> = ["A.md", "B.md", "C.md"]
            .iter()
            .enumerate()
            .map(|(index, path)| FusedHit {
                mount_id: "m".to_string(),
                key: key(path),
                mount_rank: index,
                score: 1.0 / (60.0 + index as f64),
                rerank_score: None,
            })
            .collect();
        let signals = RerankSignals {
            semantic: vec![None, None, None],
            lexical: vec![0.1, 0.9, 0.5],
        };
        let outcome = rerank(&mut hits, &signals, 3);
        assert_eq!(outcome.stage, RerankStage::None);
        assert!(
            !outcome.degraded,
            "nothing was lost on a lexical-only index"
        );
        assert_eq!(paths(&hits), vec!["A.md", "B.md", "C.md"]);
        assert!(hits.iter().all(|hit| hit.rerank_score.is_none()));
    }

    #[test]
    fn a_nan_signal_is_treated_as_no_signal_rather_than_propagated() {
        // A NaN would make the comparator non-total and the sort order unspecified.
        let mut hits: Vec<FusedHit> = ["A.md", "B.md"]
            .iter()
            .map(|path| FusedHit {
                mount_id: "m".to_string(),
                key: key(path),
                mount_rank: 0,
                score: 1.0 / 60.0,
                rerank_score: None,
            })
            .collect();
        let signals = RerankSignals {
            semantic: vec![Some(f64::NAN), Some(0.4)],
            lexical: vec![f64::NAN, 0.5],
        };
        let outcome = rerank(&mut hits, &signals, 2);
        assert_eq!(outcome.semantic_signals, 1);
        assert_eq!(paths(&hits), vec!["B.md", "A.md"]);
        assert!(hits.iter().all(|hit| hit.rerank_score.unwrap().is_finite()));
    }

    #[test]
    fn equal_rerank_scores_keep_the_fusion_order() {
        // The rerank REFINES fusion: a scorer that cannot separate two candidates must not
        // reshuffle them.
        let mut hits = vec![
            FusedHit {
                mount_id: "zzz".to_string(),
                key: key("Better.md"),
                mount_rank: 0,
                score: 2.0 / 60.0,
                rerank_score: None,
            },
            FusedHit {
                mount_id: "aaa".to_string(),
                key: key("Worse.md"),
                mount_rank: 0,
                score: 1.0 / 60.0,
                rerank_score: None,
            },
        ];
        // Identical signals for both candidates, so only the fused score can separate them --
        // and it must, even though the mount-id tie-break ('aaa' < 'zzz') would say otherwise.
        let signals = RerankSignals {
            semantic: vec![Some(0.5), Some(0.5)],
            lexical: vec![0.5, 0.5],
        };
        rerank(&mut hits, &signals, 2);
        assert_eq!(paths(&hits), vec!["Better.md", "Worse.md"]);
    }

    #[test]
    fn the_rerank_window_is_never_narrower_than_the_answer() {
        assert_eq!(rerank_window(8), FEDERATION_RERANK_MIN_WINDOW);
        assert_eq!(rerank_window(50), 50);
        assert_eq!(rerank_window(200), 200);
        // A window narrower than `limit` could not reorder the answer at all.
        for limit in [1_usize, 8, 50, 200] {
            assert!(rerank_window(limit) >= limit);
        }
    }

    #[tokio::test]
    async fn a_wider_window_keeps_more_candidates_without_fetching_more() {
        let mut narrow = PagedSource::new(vec![synthetic("Alpha", 40)], 40);
        let narrow_outcome =
            federate_with_window(vec![MountList::new("alpha", 1.0)], 5, 5, &mut narrow).await;
        let mut wide = PagedSource::new(vec![synthetic("Alpha", 40)], 40);
        let wide_outcome =
            federate_with_window(vec![MountList::new("alpha", 1.0)], 5, 30, &mut wide).await;

        assert_eq!(narrow_outcome.hits.len(), 5);
        assert_eq!(wide_outcome.hits.len(), 30);
        // Same fetching: the window discards fewer candidates, it does not ask for more.
        assert_eq!(
            narrow_outcome.candidates_fetched,
            wide_outcome.candidates_fetched
        );
        // And the wider window's first five are the narrow window's five, unchanged.
        assert_eq!(narrow_outcome.hits, wide_outcome.hits[..5]);
    }

    // -----------------------------------------------------------------------
    // Constants
    // -----------------------------------------------------------------------

    #[test]
    fn the_candidate_target_and_budget_are_the_documented_formulas() {
        assert_eq!(candidate_target(8), 100, "max(100, 8 * 10)");
        assert_eq!(candidate_target(20), 200, "max(100, 20 * 10)");
        assert_eq!(candidate_budget(8), 500, "max(500, 8 * 50)");
        assert_eq!(candidate_budget(50), 2500, "max(500, 50 * 50)");
        // Never zero, whatever a caller passes.
        assert_eq!(candidate_target(0), FEDERATION_MIN_CANDIDATES_PER_MOUNT);
        assert_eq!(candidate_budget(0), FEDERATION_MIN_BUDGET);
    }
}
