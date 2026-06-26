# Configuring deep-obsidian-mcp

- [Config file & precedence](#config-file--precedence)
- [Semantic search (embeddings)](#semantic-search-embeddings)
- [Authentication](#authentication)
- [Automatic reindexing](#automatic-reindexing)
- [Transport & stdio modes](#transport--stdio-modes)

## Config file & precedence

`setup-service` writes normalized JSON to
`~/.config/deep-obsidian-mcp/config.json` (override with `--config <path>`).
Inspect the resolved config any time with `deep-obsidian-mcp print-config`
(secrets are redacted).

Settings resolve in this order:

1. CLI flags
2. config file
3. environment variables
4. built-in defaults

Secrets (embedding API keys, the auth token) are **never** stored in the config
file. The config only holds a reference; the value lives in the OS keyring when
available, or an encrypted local file as a fallback. The encrypted-file fallback
is weaker than the OS keyring because the application carries the decryption key.

## Semantic search (embeddings)

The server has two semantic modes:

- **Sparse fallback** (default) — local term vectors, no external dependency.
- **Embedding-backed** — an OpenAI-compatible `/embeddings` endpoint, with
  similarity ranking executed through `sqlite-vec`.

Enable embeddings through the wizard (it also stores the API key securely):

```bash
deep-obsidian-mcp setup-service --wizard
```

Or configure them with flags / environment variables:

```bash
deep-obsidian-mcp serve --vault ~/Vault \
  --embedding-provider openai-compatible \
  --embedding-model nomic-embed-text \
  --embedding-base-url http://localhost:11434/v1
```

Environment variables (useful for the service wrapper and containers):

| Purpose | Variables (first match wins) |
|---|---|
| Provider | `DEEP_OBSIDIAN_EMBEDDING_PROVIDER`, `EMBEDDING_PROVIDER` |
| Model | `DEEP_OBSIDIAN_EMBEDDING_MODEL`, `EMBEDDING_MODEL`, `OPENAI_EMBEDDING_MODEL` |
| Base URL | `DEEP_OBSIDIAN_EMBEDDING_BASE_URL`, `EMBEDDING_BASE_URL`, `OPENAI_BASE_URL` |
| API key | `DEEP_OBSIDIAN_EMBEDDING_API_KEY`, `EMBEDDING_API_KEY`, `OPENAI_API_KEY` |

A blank API key is allowed for local OpenAI-compatible endpoints such as Ollama.

## Authentication

HTTP bearer authentication is **optional and disabled by default**, so loopback
(`127.0.0.1`) setups keep working unchanged. Enable it when you expose the
service beyond the local machine (binding `0.0.0.0` or fronting it with a
tunnel).

Enable it (generates a 256-bit token, stores it securely, prints it once):

```bash
deep-obsidian-mcp setup-service --wizard     # answer yes to authentication
deep-obsidian-mcp setup-service --vault ~/Vault --auth   # non-interactive
```

Disable it again (also deletes the stored token):

```bash
deep-obsidian-mcp setup-service --no-auth
```

Send the token from your client:

```
Authorization: Bearer <token>
```

When enabled, `POST /mcp` and `PUT /upload/{token}` require the token;
`/healthz` and `/readyz` stay open for liveness probes. A missing or invalid
token gets `401` with a `WWW-Authenticate: Bearer` challenge.

Two guards reduce the chance of accidental exposure:

- **Fail-closed bind** — the server refuses to start on a non-loopback host with
  auth disabled. Override deliberately with `--insecure-no-auth` or
  `DEEP_OBSIDIAN_ALLOW_INSECURE=1`.
- **Origin validation** — requests carrying a browser `Origin` header are
  rejected unless that origin is in `auth.allowedOrigins` (DNS-rebinding
  defence). Non-browser clients (Claude Code, curl) omit `Origin` and are
  unaffected.

For containers, tunnels, or headless hosts where the OS keyring is unavailable,
set `DEEP_OBSIDIAN_AUTH_TOKEN` to a literal token; it enables auth and overrides
any configured token reference.

## Automatic reindexing

The local index updates itself in the background: an initial build/check at
startup, debounced rebuilds after vault changes, and a periodic catch-up sync.
It's on by default. Tune or disable it:

```bash
deep-obsidian-mcp serve --vault ~/Vault \
  --auto-reindex true \
  --reindex-debounce-ms 1500 \
  --reindex-interval-ms 30000

deep-obsidian-mcp serve --vault ~/Vault --auto-reindex false
```

The `build_index` tool is still available for an explicit forced rebuild.

## Transport & stdio modes

The server speaks both the HTTP (Streamable) transport and stdio. For stdio
clients you can pin the framing:

```bash
deep-obsidian-mcp --vault ~/Vault --stdio-mode auto      # default
deep-obsidian-mcp --vault ~/Vault --stdio-mode framed
deep-obsidian-mcp --vault ~/Vault --stdio-mode newline
```

For the HTTP service, see [USAGE.md](./USAGE.md#3-run-it-as-a-service).
