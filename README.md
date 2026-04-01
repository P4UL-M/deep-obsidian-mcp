# deep-obsidian-mcp

Filesystem-first MCP server for deep Obsidian vault access.

Current capabilities:

- read a full note or a line range
- read deterministic line-based chunks
- find files by substring or regex path search
- search note contents with ripgrep
- build a persistent local SQLite index under `.deep-obsidian-mcp/index.sqlite`
- keep that index fresh automatically with background reindex tasks
- run semantic search over indexed note chunks
- run BM25 lexical search over indexed note chunks
- run hybrid BM25 plus semantic ranking
- detect related notes by subject similarity
- traverse outgoing links and backlinks as a graph
- answer both newline-delimited and framed stdio MCP clients from the same binary
- expose vault, note, heading, and block resources for MCP clients
- provide a long-lived HTTP service mode for background operation and MCP clients that prefer a stable endpoint

This server supports two semantic modes:

- sparse fallback with local term vectors and no external dependency
- embedding-backed mode using an OpenAI-compatible `/embeddings` endpoint, with similarity ranking executed through `sqlite-vec`

## Usage

```bash
npm install
npm run build
node dist/index.js /path/to/obsidian-vault
```

Optional:

```bash
node dist/index.js /path/to/obsidian-vault --index-dir /path/to/index-cache
```

## Service Mode

`deep-obsidian-mcp` can run as a long-lived Streamable HTTP service instead of only as a stdio subprocess.

Start it directly:

```bash
node dist/index.js /path/to/obsidian-vault \
  --transport http \
  --host 127.0.0.1 \
  --port 4100 \
  --mcp-path /mcp \
  --health-path /healthz
```

Or via the wrapper:

```bash
./scripts/run-http-service.sh /path/to/obsidian-vault
```

Embedding settings for service mode can be injected through either:

- `DEEP_OBSIDIAN_EMBEDDING_PROVIDER`
- `DEEP_OBSIDIAN_EMBEDDING_MODEL`
- `DEEP_OBSIDIAN_EMBEDDING_BASE_URL`
- `DEEP_OBSIDIAN_EMBEDDING_API_KEY`

or the generic variables already used by the server:

- `EMBEDDING_PROVIDER`
- `EMBEDDING_MODEL`
- `EMBEDDING_BASE_URL`
- `EMBEDDING_API_KEY`
- `OPENAI_EMBEDDING_MODEL`
- `OPENAI_BASE_URL`
- `OPENAI_API_KEY`

If a model is provided without an explicit provider, the service wrapper/installer assumes `openai-compatible`.

The service mode is intentionally stateless and returns JSON responses over the Streamable HTTP endpoint. That keeps the process long-lived and the index warm, while letting MCP clients connect over HTTP without spawning the server process.

Useful endpoints:

- MCP: `http://127.0.0.1:4100/mcp`
- health: `http://127.0.0.1:4100/healthz`

Quick HTTP probe:

```bash
node scripts/probe_http_service.mjs http://127.0.0.1:4100/mcp
```

## Config-Driven Service Flow

The refactor now includes a first-class service-oriented CLI in the Node codebase.

Available commands:

- `deep-obsidian-mcp setup-service`
- `deep-obsidian-mcp doctor`
- `deep-obsidian-mcp print-config`
- `deep-obsidian-mcp probe`
- `deep-obsidian-mcp serve`

`setup-service` persists normalized JSON config at `~/.config/deep-obsidian-mcp/config.json` by default. Config precedence is:

1. CLI flags
2. config file
3. environment variables
4. defaults

Example flow from the source tree:

```bash
npm install
npm run build
node dist/index.js setup-service --vault ~/Vault
node dist/index.js doctor
node dist/index.js serve
```

See [docs/homebrew-service.md](./docs/homebrew-service.md) for the Homebrew-oriented workflow and [Formula/deep-obsidian-mcp.rb](./Formula/deep-obsidian-mcp.rb) for the current formula scaffold.

## macOS launchd

Install as a user service:

```bash
./scripts/install-launchd-service.sh /path/to/obsidian-vault
```

Example with embeddings enabled:

```bash
export OPENAI_API_KEY=...
export OPENAI_EMBEDDING_MODEL=text-embedding-3-small
./scripts/install-launchd-service.sh /path/to/obsidian-vault
```

Remove it:

```bash
./scripts/uninstall-launchd-service.sh
```

The installer writes a user LaunchAgent plist under `~/Library/LaunchAgents/`, starts it with `launchctl`, and keeps it running across terminal sessions.

This is a transitional path. The Homebrew service flow in [docs/homebrew-service.md](./docs/homebrew-service.md) is the target UX.

Automatic reindexing is enabled by default. The server performs:

- an initial startup build/check
- debounced rebuild checks after vault filesystem changes
- a periodic sync task to catch missed events

You can tune or disable it:

```bash
node dist/index.js /path/to/obsidian-vault \
  --auto-reindex true \
  --reindex-debounce-ms 1500 \
  --reindex-interval-ms 30000
```

Or disable it entirely:

```bash
node dist/index.js /path/to/obsidian-vault --auto-reindex false
```

Explicit stdio mode:

```bash
node dist/index.js /path/to/obsidian-vault --stdio-mode auto
node dist/index.js /path/to/obsidian-vault --stdio-mode framed
node dist/index.js /path/to/obsidian-vault --stdio-mode newline
```

Embedding-backed mode:

```bash
EMBEDDING_PROVIDER=openai-compatible \
EMBEDDING_MODEL=text-embedding-3-small \
OPENAI_API_KEY=... \
node dist/index.js /path/to/obsidian-vault
```

Or with explicit flags:

```bash
node dist/index.js /path/to/obsidian-vault \
  --embedding-provider openai-compatible \
  --embedding-model text-embedding-3-small \
  --embedding-base-url https://api.openai.com/v1 \
  --embedding-api-key "$OPENAI_API_KEY"
```

## MCP Tools

- `vault_info`
- `load_knowledge`
- `recommend_folder`
- `read_file`
- `read_chunk`
- `find_files`
- `grep_search`
- `build_index`
- `bm25_search`
- `semantic_search`
- `hybrid_search`
- `related_notes`
- `backlinks`
- `graph_traverse`
- `upsert_session_note`

`upsert_session_note` accepts either:

- `topic` + `folder` to derive the canonical `Session - <slug>.md` path
- or an explicit vault-relative `path` to update an already-known note deterministically

When `path` is provided, it takes precedence over `topic` and `folder`. This is useful for follow-up updates from clients that already know the exact note created earlier in the conversation.

## MCP Resources

- `obsidian://vault/info`
- `obsidian://note?path=...`
- `obsidian://heading?path=...&slug=...`
- `obsidian://block?path=...&id=...`

## Example Codex Config

Local stdio subprocess:

```toml
[mcp_servers.deep_obsidian]
command = "node"
args = [
  "/absolute/path/to/deep-obsidian-mcp/dist/index.js",
  "/absolute/path/to/your/vault",
  "--stdio-mode",
  "auto",
]
```

If your client runs MCP servers in a sandbox, the vault path must be accessible to that sandbox.

HTTP service clients should point to `http://127.0.0.1:4100/mcp` instead of spawning the binary directly.

For the planned Homebrew setup, `deep-obsidian-mcp print-config` should become the authoritative way to inspect the resolved config, and `deep-obsidian-mcp doctor` should be the first troubleshooting step.

## Current Indexing Model

The local SQLite index stores:

- note-level sparse term vectors
- chunk-level sparse term vectors
- note and chunk token counts for BM25
- chunk line boundaries
- file snapshots for rebuild detection
- extracted wiki links
- optional note and chunk embeddings

When embeddings are enabled, semantic retrieval is executed through `sqlite-vec` rather than an in-memory JavaScript scan.

The server now keeps this SQLite index updated automatically in the background. Manual `build_index` is still available when you want an explicit forced rebuild.

This gives you:

- deterministic retrieval
- no external API dependency
- fast rebuild checks
- subject similarity across notes
- optional true embedding-backed semantic search with `sqlite-vec`
- BM25 lexical ranking
- hybrid lexical plus semantic ranking
- normalized wiki-link graph traversal

## Next Extensions

- support headings, blocks, and frontmatter-aware chunking in the index itself
- support denser graph APIs such as shortest path and strongly connected neighborhoods
- support BM25 plus embedding hybrid ranking at note level, not only chunk level
- add a bundled `sqlite-vec` distribution strategy for environments where extension loading is restricted
