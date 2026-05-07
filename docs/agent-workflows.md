# Agent Workflows

Deep Obsidian MCP exposes two complementary layers for agents:

- MCP prompts for clients that surface `prompts/list` and `prompts/get`.
- Packaged `SKILL.md` templates for Codex-like agents that load workflow instructions from disk.

Each workflow should live in exactly one layer. Prompts are for read/synthesis instructions. Skills are for operational workflows that may write, refactor, initialize project state, or apply safety policy.

## MCP Prompts

The server currently exposes these prompts:

| Prompt | Use |
|---|---|
| `obsidian-load-context` | Retrieve relevant vault context before answering, planning, or coding. |
| `obsidian-project-briefing` | Build a project status briefing from notes, sessions, decisions, and graph context. |
| `obsidian-daily-review` | Review recent daily/session notes and extract carry-over work. |

Prompt arguments are intentionally small:

- `subject`: topic, question, or working context.
- `project`: optional project/repository/product/domain hint.

## Packaged Skills

Skill templates live under `skills/`:

| Skill | Use |
|---|---|
| `obsidian-wiki-init` | Project bootstrap workflow for agent/wiki collaboration. |
| `obsidian-capture-session` | Create or update a structured session note. |
| `obsidian-knowledge-maintenance` | Distill raw sessions, record decisions, refactor notes, and apply memory policy. |

Homebrew installs these under the formula package share directory. Source users can copy or symlink individual skill directories into their agent's skill directory.

## Project Wiki Initialization

`obsidian-wiki-init` creates the scaffold for agentic collaboration without moving existing human notes. The expected layout is:

```text
Projets/<Project>/...human-owned notes...
_Agent/Contract.md
_Agent/Contracts/<Project>.md
_Agent/Sessions/<Project>/Index.md
_Agent/Raw/<Project>/Index.md  # optional, only when raw material is staged
_Agent/Tasks/<Project>.md
_Agent/Log.md
_Wiki/Index.md
_Wiki/Concepts/Index.md
_Wiki/Decisions/Index.md
_Wiki/Syntheses/Index.md
_Wiki/Questions/Index.md
```

`Projets/<Project>/` is human-owned. The init workflow may create the folder if missing, but must not create a hub, index, or agent contract there unless the user explicitly asks.

Workspace or repository `AGENTS.md` and `CLAUDE.md` are required as the auto-loaded agent entrypoints. They should tell coding agents to use the `deep_obsidian` MCP and read `_Agent/Contract.md`, `_Agent/Contracts/<Project>.md`, and `_Agent/Tasks/<Project>.md` from the vault. The vault should store durable contract notes, but should not rely on a vault-root `AGENTS.md` as the auto-load mechanism.

`_Agent/Raw/<Project>/` is an optional staging inbox for transcripts, exports, copied source material, logs, or other unprocessed material. It should not be treated as a required human workflow folder, and recurring maintenance should inspect it only when raw material actually exists.

The init workflow should also offer, but never silently create, a recurring maintenance automation. The automation's job is to run one small maintenance task at a time: distill an unpromoted session, distill a staged raw source when present, extract a decision, extract a concept, update open questions, or refresh the wiki index.

Recommended cadence:

| Project state | Cadence |
|---|---|
| active | every 6 hours |
| stable | daily |
| paused | weekly |

## Design Rules

- Do not make agents read the vault filesystem directly when Deep Obsidian is available.
- Prefer `load_knowledge`, `hybrid_search`, `note_outline`, and graph traversal before full-note reads.
- Keep writes opt-in and conservative.
- Use `dryRun` before broad or multi-note writes.
- Use `expectedHash` where available when updating existing notes.
- Keep raw session notes and durable knowledge separate.
- Treat raw source folders as optional, on-demand staging areas.
- Do not create recurring maintenance automations without explicit user approval.
