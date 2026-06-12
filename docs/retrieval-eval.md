# Retrieval Evaluation

Two layers measure retrieval quality for the upcoming pipeline overhaul (heading
chunking, query encoding, RRF fusion, small-to-big, graph re-rank):

1. **Deterministic CI eval** — the automated yardstick. Hermetic, no Ollama.
2. **Manual real-model protocol** — a before/after eyeball against a real
   Ollama-backed index, for the semantic-quality items a fake embedder cannot model
   (especially the query-instruction / query-encoding change).

---

## 1. Deterministic CI eval (the yardstick)

File: `rust/crates/deep-obsidian-index/tests/retrieval_eval.rs`

```bash
# Run it (fast, deterministic, no network):
cargo test -p deep-obsidian-index --test retrieval_eval

# See the per-query + aggregate report:
cargo test -p deep-obsidian-index --test retrieval_eval -- --nocapture --test-threads=1
```

**How it works.** A fixture vault (~11 small notes: multi-heading, fenced code, a
table, wikilinks, disjoint-vocab paraphrase pairs, distinctive identifiers) is built
into a real embedding-backed index whose embedding backend points at an in-process
**fake OpenAI-compatible server**. That server returns a deterministic `text -> vector`
projection (FNV-1a hashed, synonym-folded, L2-normalized bag-of-words). Both build-time
chunk embedding and query-time embedding hit the same function, so dense ranking is
meaningful yet fully reproducible. A gold query set (~12 queries: BM25 exact-term wins,
dense paraphrase wins, a fusion case, cross-linked answers) is scored with recall@k
(k=1,3,5) and MRR at the note-path level, and the hybrid aggregates are asserted `>=`
committed baselines (`BASELINE_*` consts in the test).

**Bumping the baseline.** Only after an intentional ranking improvement lands: re-run
the test 2-3x to confirm the new (higher) numbers are stable, then raise the matching
`BASELINE_*` const. Never lower a baseline to make a regression pass.

This layer cannot model: real embedding semantics, the effect of query-instruction
prefixes, or true cross-lingual / world-knowledge paraphrase. That is what layer 2 is
for.

---

## 2. Manual real-model protocol (real Ollama)

Goal: a quick **before vs. after** top-k comparison on a real embedding model, run by
hand around a pipeline change. Eyeball, don't automate — the point is to catch semantic
regressions the deterministic eval is blind to.

### Prerequisites

- A running Ollama with an embedding model pulled, e.g.:
  ```bash
  ollama pull nomic-embed-text
  # Ollama serves an OpenAI-compatible endpoint at http://localhost:11434/v1
  ```
- A real vault to point at (your own, or a representative copy).

### Step A — build a real embedding-backed index

Run the server (or the doctor/probe path) against the vault with the embedding backend
configured. The relevant global flags are `--vault`, `--embedding-provider`,
`--embedding-model`, `--embedding-base-url`:

```bash
cargo run -p deep-obsidian-cli -- \
  --vault /path/to/vault \
  --embedding-provider openai-compatible \
  --embedding-model nomic-embed-text \
  --embedding-base-url http://localhost:11434/v1 \
  serve
```

First serve triggers an index build that embeds every chunk via Ollama. Confirm the
backend is `embedding` (not `sparse`) — e.g. via `doctor` / `print-config`:

```bash
cargo run -p deep-obsidian-cli -- \
  --vault /path/to/vault \
  --embedding-provider openai-compatible \
  --embedding-model nomic-embed-text \
  --embedding-base-url http://localhost:11434/v1 \
  doctor
```

### Step B — run the fixed query list against the search tools

Retrieval is exposed through the MCP tool `hybrid_search` (it takes a `query` and an
optional `limit`, plus `bm25Weight`/`semanticWeight` to isolate the BM25-only or
semantic-only ranking). Drive it through whatever MCP client you use (the Deep Obsidian
MCP integration, or a raw JSON-RPC client over the stdio/http transport). For each query,
capture the **top-5** paths from the default `hybrid_search` and from a semantic-only run
(`bm25Weight:0`).

Use the **same fixed query list** before and after the change so the comparison is
apples-to-apples. Suggested list (adapt the nouns to your vault, keep the intent):

| # | Intent | Example query |
|---|--------|---------------|
| 1 | Exact identifier / proper noun (BM25 anchor) | a distinctive name or code from one note |
| 2 | Paraphrase, disjoint vocabulary (dense) | restate a note's idea using none of its words |
| 3 | Cross-lingual / synonym paraphrase (dense) | ask in different phrasing than the source |
| 4 | Multi-concept question spanning two linked notes | a question whose answer is split across `[[links]]` |
| 5 | Code / command lookup | describe what a fenced code block does |
| 6 | Tabular fact | ask for a value that lives in a table row |
| 7 | Broad-topic recall | a general topic many notes touch |
| 8 | Long natural-language question | a full-sentence question, not keywords |

### Step C — what to eyeball

For each query, compare **before** vs **after** top-5:

- **Relevance of rank 1** — did the obviously-correct note stay at or move to #1?
- **Paraphrase queries (2, 3, 8)** — these are the query-encoding payoff. The right
  note should appear higher after a query-instruction / encoding improvement. Watch for
  it *dropping out of* top-5 (a regression the deterministic eval cannot see).
- **Exact-term query (1)** — must NOT regress: the exact match should stay #1. A common
  failure mode of dense-heavy changes is burying exact hits.
- **Cross-link query (4)** — both the question note and the answer note should surface
  (this is where graph re-rank should help post-overhaul).
- **No new garbage** — note any clearly-irrelevant result that newly appears in top-5.

Record a one-line verdict per query (better / same / worse) and a short note on any
worse case. If anything regresses, reproduce it in the deterministic eval as a new gold
query where feasible, so CI catches it next time.
