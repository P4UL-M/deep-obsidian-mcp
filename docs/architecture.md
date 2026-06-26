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

## Roadmap

Possible future work:

- headings/blocks/frontmatter-aware chunking in the index itself
- denser graph APIs (shortest path, strongly connected neighbourhoods)
- note-level (not only chunk-level) BM25 + embedding hybrid ranking
- a bundled `sqlite-vec` distribution strategy for environments where extension
  loading is restricted
