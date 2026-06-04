# Fix: Ollama `llama-server` crash from oversized embedding inputs

Status: implemented + empirically verified (2026-06-04)

## Symptom
The local embedding backend (**Ollama**, `llama-server` worker) crashes repeatedly
(`SIGTRAP`, `libsystem_malloc: memory corruption of free block`, backtrace
`llama_decode` → `server_context_impl::update_slots()`) — 26 crash reports in one
afternoon. `deep-obsidian-mcp` itself stays up (embedding errors are logged and
auto-reindex retries), so the visible effect is a crash-restart storm of the
embedding backend.

## Root cause
llama.cpp heap-corrupts when an embedding **input exceeds the worker's allocated
`num_ctx`** (it corrupts instead of erroring cleanly). The client was feeding
oversized inputs:
- `prepare_note_from_snapshot` embedded the **whole note** (`title + content`) as a
  single input — a 34 KB note ≈ ~8.5k tokens, far past Ollama's default `num_ctx=4096`.
- `clamp_text` bounded inputs by **characters (12k)**, never by tokens.
- `chunk_lines` split by **line count only** (80 lines) — no char/token bound.
- 32 inputs per `/embeddings` request.

## Fix (client-side; no embedding-server change required)
- **Token+char input budget** (`embeddings.rs`): every input clamped to
  `min(max_chars, max_input_tokens * chars_per_token)`. Defaults target Ollama's
  out-of-box `num_ctx=4096` with margin (`max_input_tokens=2800`, `chars_per_token=2.5`,
  `max_chars=8000`).
- **Char-bounded chunks** (`index.rs`): `chunk_lines` also closes on a `max_chars`
  budget (`DEFAULT_CHUNK_MAX_CHARS=6000` ≈ 2.4k est tokens), ending on whole-line
  boundaries; an over-budget single line becomes its own chunk and is truncated by
  the input clamp.
- **Mean-pooled note vectors** (`index.rs`): the note vector is the normalized mean
  of its chunk vectors — the whole note is never embedded as one input. Reuses chunk
  vectors at no extra HTTP cost.
- **Per-request token cap + bisect-retry** (`embeddings.rs`): a batch's token sum is
  bounded to the per-input budget; on failure the input list bisects so one bad input
  can't fail the batch. (1:1 input→vector contract preserved.)
- **Auto-reindex backoff** (`runtime.rs`): exponential backoff on consecutive refresh
  failures so a crashing backend is never hammered.
- **Config knobs** (`types`/`config`/`runtime`): `maxChars` / `maxInputTokens` /
  `contextTokens` in `config.json`, defaulted. Raise these only after raising the
  server window (`OLLAMA_CONTEXT_LENGTH`); `qwen3-embedding:0.6b` supports up to 32k.

## Why dense content matters
The vault is technical (eval reports, configs): ~2.7–2.9 chars/token, so a 12k-char
chunk ≈ 4.4–4.8k real tokens — over a 4096 window. The first 4096-targeted attempt
still crashed; defaults were lowered to ~2.4k-token chunks (6k chars) with margin.
The vault contains no CJK (checked), so the 2.5 chars/token estimate is conservative.

## Verification (empirical)
Built the fixed binary and ran `serve --vault <vault> --index-dir <temp>` against the
**live Ollama at default `num_ctx=4096`**, with the old service stopped for clean
attribution:
- Whole vault embedded: **180 note vectors + 283 chunk vectors**, `semanticBackend=embedding`,
  1024 dims, `indexStatus=ready`, no error.
- **Zero new `~/Library/Logs/DiagnosticReports/llama-server-*.ips` reports** during the
  full reindex (vs. a guaranteed crash with the old code, confirmed in the same session).
- All 16 cargo test suites pass (unit coverage for clamp, batching, bisect, chunk
  bounds, mean-pool, backoff).

## Rollout (remaining, on the user's machine)
1. Deploy the fixed binary (build + `brew`/cargo install over the running service).
2. **Force a full rebuild** once after deploy — `same_semantic_config` reuses an
   existing index, so persisted whole-note vectors won't be replaced by mean-pooled
   ones until a forced rebuild or content change.
3. Optional headroom: set `OLLAMA_CONTEXT_LENGTH=8192` and raise the `contextTokens` /
   `maxInputTokens` config knobs for larger, fewer chunks.
