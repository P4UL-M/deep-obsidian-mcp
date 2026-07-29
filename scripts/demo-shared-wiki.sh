#!/usr/bin/env bash
# Two-participant shared-wiki demo (design: docs/algolia-shared-wiki.md).
#
# Everything runs locally: an in-process mock of the Algolia REST API stands in
# for the real service, so no account, key, or network is needed. Swap
# `baseUrl` out of the configs (and use a real app id + key) to run the same
# flow against actual Algolia.
#
# Model C: the wiki LIVES in the index and is authored through the mount.
#   Paul  (seeder):   imports his local _Wiki/ once with `share seed --move`
#   Alice (teammate): empty vault, mounts the corpus at _Shared/Team/,
#                     reads/searches/writes through MCP over HTTP
#
# Usage: scripts/demo-shared-wiki.sh
set -euo pipefail

MOCK_PORT=9411
ALICE_PORT=4199
ROOT="$(mktemp -d /tmp/deep-obsidian-shared-demo.XXXXXX)"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$REPO/target/debug/deep-obsidian-mcp"
export DEEP_OBSIDIAN_ALGOLIA_API_KEY=demo-key

PIDS=()
cleanup() {
  for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT

say()  { printf '\n\033[1;36m== %s\033[0m\n' "$*"; }
run()  { printf '\033[2m$ %s\033[0m\n' "$*"; "$@"; }

json() { python3 -m json.tool 2>/dev/null || cat; }

mcp() { # mcp <tool> <json-arguments>
  curl -sS "http://127.0.0.1:$ALICE_PORT/mcp" \
    -H 'content-type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"$1\",\"arguments\":$2}}" \
  | python3 -c '
import json, sys
body = json.load(sys.stdin)
result = body.get("result", body)
sc = result.get("structuredContent")
if sc is None:
    texts = [c.get("text", "") for c in result.get("content", [])]
    try:
        sc = json.loads(texts[0]) if texts else result
    except Exception:
        sc = result
print(json.dumps(sc, indent=2, ensure_ascii=False))
'
}

say "Building binaries (debug)"
(cd "$REPO" && cargo build -q -p deep-obsidian-cli \
   && cargo build -q -p deep-obsidian-algolia --example mock_algolia)

say "Starting mock Algolia on :$MOCK_PORT"
"$REPO/target/debug/examples/mock_algolia" "$MOCK_PORT" >"$ROOT/mock.log" 2>&1 &
PIDS+=($!)
sleep 1

# ---------------------------------------------------------------- Paul (publisher)
say "Seeding Paul's vault (3 wiki notes, 1 private, 1 agent-only)"
PAUL="$ROOT/paul-vault"
mkdir -p "$PAUL/_Wiki/Decisions" "$PAUL/_Wiki/Syntheses" "$PAUL/_Agent/Sessions"

cat > "$PAUL/_Wiki/Decisions/Keep retrieval architecture-agnostic.md" <<'EOF'
---
type: wiki-decision
project: Deep Obsidian
status: active
---

# Keep retrieval architecture-agnostic

## Decision

Retrieval tools stay generic; workflow rules live in prompts and skills.

## Rationale

A generic retrieval layer is easier to reuse across projects and vault layouts.
EOF

cat > "$PAUL/_Wiki/Decisions/Spacelift for IaC.md" <<'EOF'
---
type: wiki-decision
project: Agent SEO
status: active
---

# Spacelift for IaC

## Decision

The agent SEO repository uses Spacelift for infrastructure-as-code.

## Rationale

It replaced hand-rolled Terraform pipelines; state stays reviewable per stack.
See [[Keep retrieval architecture-agnostic]] for the retrieval philosophy.
EOF

cat > "$PAUL/_Wiki/Syntheses/Product narrative.md" <<'EOF'
---
type: wiki-synthesis
project: Deep Obsidian
---

# Product narrative

## Summary

Deep Obsidian bridges human note-taking and agent memory: load context, work,
capture the session, distill durable knowledge.
EOF

cat > "$PAUL/_Wiki/Private draft.md" <<'EOF'
---
share: false
---

# Private draft

Never leaves Paul's machine.
EOF

echo "# Session log — local only" > "$PAUL/_Agent/Sessions/session.md"

cat > "$ROOT/paul-config.json" <<EOF
{
  "vaultPath": "$PAUL",
  "indexDir": "$ROOT/paul-index",
  "shared": [
    {
      "mountAt": "_Shared/Team/",
      "appId": "DEMOAPP",
      "indexName": "team-wiki",
      "baseUrl": "http://127.0.0.1:$MOCK_PORT",
      "writable": true,
      "participantId": "paul@demo"
    }
  ]
}
EOF

say "Paul: seed dry-run first (nothing written)"
run "$BIN" --config "$ROOT/paul-config.json" share seed --prefix _Wiki/ --dry-run

say "Paul: seed --move (import once, then the index holds the only copy)"
run "$BIN" --config "$ROOT/paul-config.json" share seed --prefix _Wiki/ --move --yes

say "Paul's vault after --move: only the private + agent notes remain local"
find "$PAUL" -type f -name '*.md' | sed "s|$PAUL/||" | sort

# ---------------------------------------------------------------- Alice (consumer)
say "Alice: empty vault, mounts the shared corpus at _Shared/Team/"
ALICE="$ROOT/alice-vault"
mkdir -p "$ALICE"
cat > "$ALICE/Reading list.md" <<'EOF'
# Reading list

A purely local note about retrieval papers.
EOF

cat > "$ROOT/alice-config.json" <<EOF
{
  "vaultPath": "$ALICE",
  "indexDir": "$ROOT/alice-index",
  "transport": "http",
  "http": { "host": "127.0.0.1", "port": $ALICE_PORT },
  "shared": [
    {
      "mountAt": "_Shared/Team/",
      "appId": "DEMOAPP",
      "indexName": "team-wiki",
      "baseUrl": "http://127.0.0.1:$MOCK_PORT",
      "writable": true,
      "participantId": "alice@demo"
    }
  ]
}
EOF

"$BIN" --config "$ROOT/alice-config.json" serve >"$ROOT/alice-server.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 50); do
  curl -fsS "http://127.0.0.1:$ALICE_PORT/readyz" >/dev/null 2>&1 && break
  sleep 0.2
done

say "vault_info — the mount is visible, recallStage reported"
mcp vault_info '{}' | python3 -c 'import json,sys; d=json.load(sys.stdin); print(json.dumps({"vaultPath": d.get("vaultPath"), "sharedMounts": d.get("sharedMounts")}, indent=2))'

say "list_children — Alice's local root shows the virtual _Shared namespace"
mcp list_children '{}'

say "list_children _Shared/Team/_Wiki — folders are facet values, no files on disk"
mcp list_children '{"path": "_Shared/Team/_Wiki"}'

say "read_file — hydrated from chunk records, byte-exact, with a versionId"
mcp read_file '{"path": "_Shared/Team/_Wiki/Decisions/Spacelift for IaC.md"}'

say "hybrid_search — local + shared fused by rank (RRF); shared hits are labeled"
mcp hybrid_search '{"query": "spacelift infrastructure", "limit": 5}'

say "graph_traverse incoming — backlinks are ONE filter query on the shared index"
mcp graph_traverse '{"path": "_Shared/Team/_Wiki/Decisions/Keep retrieval architecture-agnostic.md", "direction": "incoming", "depth": 1}'

say "grep_search — local stays exhaustive; shared scope is candidate-bounded and says so"
mcp grep_search '{"query": "Spacelift", "limit": 10}'

say "grep_search with an anchor-less regex — the shared pass REFUSES instead of under-reporting"
mcp grep_search '{"query": "^\\\\s*$", "regex": true, "limit": 5}'

say "Alice WRITES to the shared wiki (append-only version, no CAS needed)"
mcp upsert_note '{"path": "_Shared/Team/_Wiki/Decisions/Spacelift for IaC.md", "content": "---\ntype: wiki-decision\nproject: Agent SEO\nstatus: active\n---\n\n# Spacelift for IaC\n\n## Decision\n\nThe agent SEO repository uses Spacelift for infrastructure-as-code.\n\n## Rationale\n\nIt replaced hand-rolled Terraform pipelines; state stays reviewable per stack.\nSee [[Keep retrieval architecture-agnostic]] for the retrieval philosophy.\n\n## Operational notes (Alice)\n\nStack promotion runs through the shared runner pool since June.\n"}'

say "note_history — Paul's version is superseded, not destroyed"
mcp note_history '{"path": "_Shared/Team/_Wiki/Decisions/Spacelift for IaC.md"}'

say "read_version — recover the superseded content (recovery is a query, not a restore)"
OLD_VERSION=$(mcp note_history '{"path": "_Shared/Team/_Wiki/Decisions/Spacelift for IaC.md"}' \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print(next(v["versionId"] for v in d["versions"] if not v.get("current")))')
mcp read_version "{\"path\": \"_Shared/Team/_Wiki/Decisions/Spacelift for IaC.md\", \"versionId\": \"$OLD_VERSION\"}" \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print(json.dumps({"versionId": d["versionId"], "firstLines": d["text"].split("\n")[:8]}, indent=2))'

say "share status — what the index holds, from Paul's side"
run "$BIN" --config "$ROOT/paul-config.json" share status

say "share dump — the exit strategy: materialize the whole index locally"
run "$BIN" --config "$ROOT/paul-config.json" share dump --to "$ROOT/backup"
find "$ROOT/backup" -type f | sed "s|$ROOT/backup/||" | sort

say "share retract — the one destructive op (note + chunks + all history)"
run "$BIN" --config "$ROOT/paul-config.json" share retract --path "_Wiki/Syntheses/Product narrative.md" --yes
run "$BIN" --config "$ROOT/paul-config.json" share status

say "Demo complete"
echo "Sandbox: $ROOT (removed on exit: no — kept for inspection)"
echo "Alice MCP endpoint still up until this script exits."
