# deep-obsidian-mcp

Filesystem-first MCP server for deep Obsidian vault access.

## Maintenance Status

This repository is Rust-only.

- Use `cargo` from the repository root.
- Use [bin/deep-obsidian-mcp](/Users/paul.mairesse/Documents/Playground/deep-obsidian-mcp/bin/deep-obsidian-mcp) or the compiled Rust binary for local and production use.
- The old Node and TypeScript runtime has been removed from the maintained worktree.

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
- expose MCP prompts for common Obsidian agent workflows
- provide a long-lived HTTP service mode for background operation and MCP clients that prefer a stable endpoint

This server supports two semantic modes:

- sparse fallback with local term vectors and no external dependency
- embedding-backed mode using an OpenAI-compatible `/embeddings` endpoint, with similarity ranking executed through `sqlite-vec`

## Homebrew Quick Start

Install the packaged CLI from the tap:

```bash
brew tap P4UL-M/tap
brew install deep-obsidian-mcp
```

Configure the local service for your vault. For a first install, use the interactive wizard:

```bash
deep-obsidian-mcp setup-service --wizard
```

The wizard asks for the vault path, whether to configure MCP clients, whether to install packaged skills and vault snippets, and whether to enable embeddings.

For a scriptable setup, pass the choices explicitly:

```bash
deep-obsidian-mcp setup-service --vault ~/Vault --mcp --skills --vault-snippets
```

This writes the service config, configures supported MCP clients, installs packaged agent skills, and installs the packaged Obsidian CSS snippets. Add `--dry-run` to preview the changes, or `--overwrite` to replace existing MCP entries, skills, snippets, or service config.

Start and validate the Homebrew service:

```bash
brew services start deep-obsidian-mcp
deep-obsidian-mcp doctor
deep-obsidian-mcp probe
```

Useful endpoints after the service starts:

- MCP: `http://127.0.0.1:4100/mcp`
- health: `http://127.0.0.1:4100/healthz`
- readiness: `http://127.0.0.1:4100/readyz`

Update or remove the package:

```bash
brew upgrade deep-obsidian-mcp
brew services restart deep-obsidian-mcp

brew services stop deep-obsidian-mcp
brew uninstall deep-obsidian-mcp
```

See [docs/homebrew-service.md](./docs/homebrew-service.md) for the full Homebrew service model and troubleshooting notes.

## Source Usage

```bash
cargo build --release -p deep-obsidian-cli --bin deep-obsidian-mcp
./bin/deep-obsidian-mcp /path/to/obsidian-vault
```

Optional:

```bash
./bin/deep-obsidian-mcp /path/to/obsidian-vault --index-dir /path/to/index-cache
```

Rust workspace commands:

```bash
cargo check --workspace
cargo test --workspace
cargo run -p deep-obsidian-cli --bin deep-obsidian-mcp -- --vault tests/fixtures/vault
```

## Service Mode

`deep-obsidian-mcp` can run as a long-lived Streamable HTTP service instead of only as a stdio subprocess.

Start it directly:

```bash
./bin/deep-obsidian-mcp /path/to/obsidian-vault \
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

Embedding settings for service mode can be configured with:

- `DEEP_OBSIDIAN_EMBEDDING_PROVIDER`
- `DEEP_OBSIDIAN_EMBEDDING_MODEL`
- `DEEP_OBSIDIAN_EMBEDDING_BASE_URL`

or the generic variables already used by the server:

- `EMBEDDING_PROVIDER`
- `EMBEDDING_MODEL`
- `EMBEDDING_BASE_URL`
- `OPENAI_EMBEDDING_MODEL`
- `OPENAI_BASE_URL`

API keys are stored through `setup-service --wizard` as an `apiKeyRef` in `config.json`; the secret value is stored in the OS keyring when possible, or in the encrypted local fallback. Blank API keys are allowed for local OpenAI-compatible endpoints such as Ollama.

Encrypted local secret storage prevents accidental plaintext exposure in config files. For stronger local protection, use the OS keyring provider. The encrypted-file fallback is not equivalent to OS keyring storage because the application carries the decryption key.

The service mode is intentionally stateless and returns JSON responses over the Streamable HTTP endpoint. That keeps the process long-lived and the index warm, while letting MCP clients connect over HTTP without spawning the server process.

### HTTP authentication

HTTP authentication is **optional and disabled by default**, so existing loopback (`127.0.0.1`) setups keep working unchanged. Enable it when you expose the service beyond the local machine (binding `0.0.0.0` or fronting it with a tunnel).

Enable it through the wizard:

```bash
deep-obsidian-mcp setup-service --wizard
```

Or non-interactively with the flag-driven setup (CI / automation):

```bash
deep-obsidian-mcp setup-service --vault ~/Vault --auth
```

Either way, when auth is enabled the CLI generates a random 256-bit token, stores it via the same secret store used for API keys (OS keyring, or the encrypted-file fallback) as a `tokenRef` in `config.json`, and prints the token to stdout **once**. Without `--auth` (and outside the wizard) auth is left as configured — off for a new config. Configure your MCP client to send it:

```
Authorization: Bearer <token>
```

Once enabled, `POST /mcp` and `PUT /upload/{token}` require the token; `/healthz` and `/readyz` stay open for liveness probes. Invalid or missing tokens get `401` with a `WWW-Authenticate: Bearer` challenge.

Two behaviors protect against accidental exposure:

- **Fail-closed bind**: the server refuses to start on a non-loopback host with auth disabled. Override deliberately with `--insecure-no-auth` or `DEEP_OBSIDIAN_ALLOW_INSECURE=1`.
- **Origin validation**: requests carrying a browser `Origin` header are rejected unless the origin is in `auth.allowedOrigins` (DNS-rebinding defence). Non-browser clients (Claude Code, curl) omit `Origin` and are unaffected.

For containers, tunnels, or headless hosts where the OS keyring is unavailable, set `DEEP_OBSIDIAN_AUTH_TOKEN` to a literal token; it enables auth and overrides any configured `tokenRef`.

Useful endpoints:

- MCP: `http://127.0.0.1:4100/mcp`
- health: `http://127.0.0.1:4100/healthz`
- readiness: `http://127.0.0.1:4100/readyz`

Quick HTTP probe:

```bash
./bin/deep-obsidian-mcp probe --vault /path/to/obsidian-vault
```

## Config-Driven Service Flow

The maintained CLI now includes first-class service commands in Rust.

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

Use `--mcp`, `--skills`, and `--vault-snippets` during setup to connect local coding agents and install the packaged Obsidian UI helpers at the same time:

```bash
deep-obsidian-mcp setup-service --vault ~/Vault --mcp --skills --vault-snippets
```

- `--mcp` configures Codex in `~/.codex/config.toml` and, when the `claude` CLI is available, runs `claude mcp add --transport http --scope user deep-obsidian <mcp-url>`.
- `--skills` installs packaged skills into `~/.codex/skills` and `~/.claude/skills`.
- `--vault-snippets` installs packaged Obsidian CSS snippets into `<vault>/.obsidian/snippets` and enables them in `<vault>/.obsidian/appearance.json`.
- `--dry-run` validates and reports these changes without writing them.
- `--overwrite` replaces an existing service config, Codex MCP entry, Claude MCP entry, installed skill directories, or installed vault snippets.

Example flow from the source tree:

```bash
cargo build --release -p deep-obsidian-cli --bin deep-obsidian-mcp
./bin/deep-obsidian-mcp setup-service --vault ~/Vault --mcp --skills --vault-snippets
./bin/deep-obsidian-mcp doctor
./bin/deep-obsidian-mcp serve
```

See [docs/homebrew-service.md](./docs/homebrew-service.md) for the Homebrew workflow, [docs/homebrew-gap-todo.md](./docs/homebrew-gap-todo.md) for the remaining release-artifact validation gaps, and [Formula/deep-obsidian-mcp.rb](./Formula/deep-obsidian-mcp.rb) for the formula.

## Agent Workflows

The server exposes MCP prompts for read/synthesis workflows:

- `obsidian-load-context`
- `obsidian-project-briefing`
- `obsidian-daily-review`

Packaged agent skill templates live in [skills](./skills) for operational workflows:

- `obsidian-wiki-init`
- `obsidian-capture-session`
- `obsidian-knowledge-maintenance`

Packaged Obsidian CSS snippets live in [obsidian-snippets](./obsidian-snippets). The default snippet hides `_Agent` and `_Wiki` from the Obsidian file explorer while keeping those folders available to Deep Obsidian through MCP.

Project logo and icon assets live in [assets](./assets):

- `deep-obsidian-logo.svg`: full-color vector logo
- `deep-obsidian-menubar.svg`: monochrome `currentColor` icon for taskbar and menubar use
- `icons/deep-obsidian-favicon.ico`: favicon-sized multi-resolution ICO
- `icons/deep-obsidian-app.ico`: app-sized multi-resolution ICO
- `icons/png/`: generated PNG exports

Homebrew installs packaged skills under the formula `pkgshare/skills` directory, snippets under `pkgshare/obsidian-snippets`, and icons under `pkgshare/assets`. Source users can install skills and snippets through `setup-service` from the repository root.

## macOS launchd

Install as a user service:

```bash
./scripts/install-launchd-service.sh /path/to/obsidian-vault
```

Example with local embeddings enabled:

```bash
deep-obsidian-mcp setup-service --wizard
```

Remove it:

```bash
./scripts/uninstall-launchd-service.sh
```

The installer writes a user LaunchAgent plist under `~/Library/LaunchAgents/`, starts it with `launchctl`, and keeps it running across terminal sessions.

This is still a local operational path. The Homebrew service flow in [docs/homebrew-service.md](./docs/homebrew-service.md) is the packaged UX and runs the CLI with `--packaged` so default indexes live outside the vault.

Automatic reindexing is enabled by default. The server performs:

- an initial startup build/check
- debounced rebuild checks after vault filesystem changes
- a periodic sync task to catch missed events

You can tune or disable it:

```bash
./bin/deep-obsidian-mcp /path/to/obsidian-vault \
  --auto-reindex true \
  --reindex-debounce-ms 1500 \
  --reindex-interval-ms 30000
```

Or disable it entirely:

```bash
./bin/deep-obsidian-mcp /path/to/obsidian-vault --auto-reindex false
```

Explicit stdio mode:

```bash
./bin/deep-obsidian-mcp /path/to/obsidian-vault --stdio-mode auto
./bin/deep-obsidian-mcp /path/to/obsidian-vault --stdio-mode framed
./bin/deep-obsidian-mcp /path/to/obsidian-vault --stdio-mode newline
```

Embedding-backed mode:

```bash
EMBEDDING_PROVIDER=openai-compatible \
EMBEDDING_MODEL=nomic-embed-text \
EMBEDDING_BASE_URL=http://localhost:11434/v1 \
./bin/deep-obsidian-mcp /path/to/obsidian-vault
```

Or with explicit flags and a config secret reference created by the wizard:

```bash
./bin/deep-obsidian-mcp /path/to/obsidian-vault \
  --embedding-provider openai-compatible \
  --embedding-model nomic-embed-text \
  --embedding-base-url http://localhost:11434/v1
```

## MCP Tools

- `vault_info`
- `load_knowledge`
- `recommend_folder`
- `list_children` (set `foldersOnly:true` to list only subfolders)
- `read_file`
- `find_files`
- `grep_search`
- `build_index`
- `hybrid_search` (set `bm25Weight:0` for semantic-only, `semanticWeight:0` for BM25-only ranking)
- `related_notes`
- `graph_traverse` (use `direction:"incoming"`, `depth:1` for backlinks)
- `upsert_note`
- `update_note_section`
- `request_vault_upload`
- `upsert_session_note`

`upsert_session_note` accepts either:

- `topic` + `folder` to derive the canonical `Session - <slug>.md` path
- or an explicit vault-relative `path` to update an already-known note deterministically

When `path` is provided, it takes precedence over `topic` and `folder`. This is useful for follow-up updates from clients that already know the exact note created earlier in the conversation.

`upsert_session_note` writes the provided markdown body as-is. It does not auto-insert a top-level title; clients should include one explicitly only when they want one saved.

Additional authoring helpers:

- `upsert_note`: generic markdown note create/update with explicit `content` or explicit `frontmatter` + `title` + `body`
- `update_note_section`: replace the preamble or a named heading section without rewriting the full note
- `request_vault_upload`: mint an out-of-band upload URL for binary or large non-markdown files
- `list_children`: inspect actual vault structure instead of guessing from search (use `foldersOnly:true` for subfolders)

## MCP Resources

- `obsidian://vault/info`
- `obsidian://note?path=...`
- `obsidian://heading?path=...&slug=...`
- `obsidian://block?path=...&id=...`

For the recommended pattern for snippet-backed writing conventions, see [docs/writing-conventions-pattern.md](docs/writing-conventions-pattern.md).

## Example Codex Config

Local stdio subprocess:

```toml
[mcp_servers.deep_obsidian]
command = "/absolute/path/to/deep-obsidian-mcp/bin/deep-obsidian-mcp"
args = [
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

### Recently Added Authoring Tools

The Rust runtime now includes the authoring and structure tools that were previously missing:

- `upsert_note`
  - generic note create/update with explicit control over `content` or `frontmatter` + `title` + `body`
  - no implicit title injection
- `update_note_section`
  - patch the note preamble or a named heading section without rewriting the whole note
- `request_vault_upload`
  - mint an out-of-band upload URL for binary or large non-note files
- `list_children`
  - inspect the actual vault structure directly (use `foldersOnly:true` to list only subfolders)
