# Architecture & indexing model

Internal notes on how the server stores and retrieves vault data. For user-facing
configuration, see the top-level [CONFIGURATION.md](../CONFIGURATION.md).

## Local index

The server maintains a persistent local SQLite index (default
`<vault>/.deep-obsidian-mcp/index.sqlite`; relocated outside the vault in
packaged mode). It stores:

- note-level and chunk-level sparse term vectors
- note and chunk token counts (for BM25)
- chunk line boundaries
- file snapshots for rebuild detection
- extracted wiki links
- optional note and chunk embeddings

When embeddings are enabled, semantic retrieval runs through `sqlite-vec` rather
than an in-memory scan.

This design gives:

- deterministic retrieval with no required external API
- fast rebuild checks
- subject similarity across notes
- BM25 lexical ranking and hybrid lexical + semantic ranking
- normalized wiki-link graph traversal

The index is kept fresh automatically in the background (see
[automatic reindexing](../CONFIGURATION.md#automatic-reindexing)); `build_index`
forces an explicit rebuild.

## Semantic backends

- **Sparse fallback** — local term vectors, no external dependency.
- **Embedding-backed** — an OpenAI-compatible `/embeddings` endpoint with
  `sqlite-vec` similarity ranking.

## Freshness encoding

`freshness` is opt-in; presets and formulas are documented in
[CONFIGURATION.md](../CONFIGURATION.md#search-freshness). Runtime initialization
converts an existing D-dimensional cache locally, without model calls. There is
**one stored D+2 vector per chunk**, in `chunk_embeddings_vec`, and no permanent
D-dimensional copy or parallel generation table.

The cache is marked schema v7. v6 D-only caches are accepted for local conversion;
older binaries reject v7 rather than accidentally query D+2 data with D coordinates.

For normalized semantic vectors, stored points are `P=[x,z,sqrt(1-z*z)]`, with
`z=2^((mtime-T)/90 days)`. Query points are `Q=[(1-beta)q,beta*g,0]`,
`g=2^((T-now)/90 days)`. Neither augmented vector is renormalized. All stored
points have squared norm 2; zero source vectors receive additional compensation.
Native Euclidean MATCH ranks the weighted semantic/freshness score before LIMIT.
There is no padding and no AVX/NEON build-flag change.

Disabled uses the **same table with beta=0**. Related-note queries likewise set
both temporal query coordinates to zero; cross-index scoring reads only the
first D coordinates. Disabling therefore needs no reconstruction or separate
semantic index. Rankings are semantically equivalent mathematically; float32
rounding can still change near ties. Legacy D-only caches remain readable until
runtime initialization converts them.

`semantic_score` maps the adjusted semantic component through the usual
`1/(1+sqrt(2-2F))` scale. `bm25_score` stays raw; BM25's sortable `score` is adjusted
before sort/truncation. Hybrid `score` remains RRF plus the existing graph bonus.
Federated reranking rebuilds its component lists with the same coefficients,
without adding a date bonus to the fused score. Artifacts are unchanged.

A `freshnessEncoding` metadata value holds semantic dimension, creation time,
anchor (+18 calendar months), and renewal (+16 calendar months). Source refreshes
preserve it and encode changed chunks directly into the same D+2 table. Initial
conversion reads cached D vectors into RAM while replacing the virtual table.
Subsequent renewals rewrite that table in place using bounded memory and one
normal SQLite transaction. Only the first D coordinates are needed to renew;
the embedding model is not called. No build state, persistent cursor, dual-write
protocol or parallel generation is retained. After an interruption, SQLite rolls
back the attempt and the next run restarts it from the beginning.

The hourly maintenance check remains independent of source auto-reindex, and
startup catches up after downtime. Runtime refreshes serialize with maintenance.
Renewal writes the canonical cache and then publishes a query snapshot through
alpha.7's existing runtime snapshot mechanism. Existing readers retain their
committed view; there is no additional freshness-specific serving generation.
A failed snapshot publication is retried by the next maintenance check.
There is **no guarantee that searches remain available during renewal**: they may
wait or return a busy error. Unknown/epoch timestamps are neutral. A future clock
value is initially neutral; a single retry timestamp in the encoding metadata
requests a refresh once that date becomes valid, without shifting the anchor.

At D=4096 the stored payload grows from 16,384 to **16,392 bytes per chunk**
(+8 bytes, about 0.049%). This is the coordinate payload only: SQLite/vec0 overhead,
text and indexes remain, and an ordinary SQLite transaction can need temporary
journal/WAL space during rewriting. This is not a second serving index.

The presets were selected using controlled synthetic corpora.
Real-model quality and global latency remain separate evaluations; deterministic
mock servers verify engine behavior, not linguistic quality.

## Roadmap

Possible future work:

- headings/blocks/frontmatter-aware chunking in the index itself
- denser graph APIs (shortest path, strongly connected neighbourhoods)
- note-level (not only chunk-level) BM25 + embedding hybrid ranking
- a bundled `sqlite-vec` distribution strategy for environments where extension
  loading is restricted
