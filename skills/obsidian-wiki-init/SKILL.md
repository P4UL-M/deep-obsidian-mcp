---
name: obsidian-wiki-init
description: Initialize a project-specific Obsidian agent/wiki workspace through Deep Obsidian MCP. Use when starting a new project, onboarding an agent to an existing project, or creating the scaffold that separates human notes from messy agent sessions and durable wiki knowledge.
---

# Obsidian Wiki Init

Use this skill to bootstrap a project for agentic collaboration. The goal is to create a small operating structure that lets agents capture work, optionally stage raw material when provided, distill durable knowledge, and promote validated notes without polluting the user's human project folders.

All vault access must go through `deep_obsidian`. Do not use raw filesystem reads or writes.

## Target Architecture

```text
Projets/
  <Project>/
    ...human-owned notes...

_Agent/
  Contract.md
  Contracts/
    <Project>.md
  Sessions/
    <Project>/
      Index.md
  Raw/
    <Project>/
      Index.md  # optional, created only when raw material is ingested
  Tasks/
    <Project>.md
  Log.md

_Wiki/
  Index.md
  Concepts/
    Index.md
  Decisions/
    Index.md
  Syntheses/
    Index.md
  Questions/
    Index.md

Workspace/
  AGENTS.md
  CLAUDE.md
```

## Meaning Of Each Layer

| Layer | Owner | Purpose |
|---|---|---|
| `Projets/<Project>/` | Human | Clean project notes structured only by the user. Agents must not impose a hub, index, or contract here. |
| `_Agent/Contract.md` | Agent plus human review | Durable vault architecture contract referenced by workspace instructions. |
| `_Agent/Contracts/<Project>.md` | Agent plus human review | Project-specific operating rules that point to, but do not modify, the human project surface. |
| `_Agent/Sessions/<Project>/` | Agent | Messy session captures, work logs, traces, and temporary summaries. |
| `_Agent/Raw/<Project>/` | Agent | Optional inbox for imported sources, transcripts, snippets, and unprocessed material. Create only when raw material is actually ingested. |
| `_Agent/Tasks/<Project>.md` | Agent | Project maintenance duties, distillation queue, and recurring automation instructions. |
| `_Agent/Log.md` | Agent | Append-only record of agent ingests, distillations, promotions, and major writes. |
| `_Wiki/Concepts/` | Agent plus human review | Cross-project concepts that should not be duplicated per project. |
| `_Wiki/Decisions/` | Agent plus human review | Durable decisions before or unless they are promoted to `RFCs/`. |
| `_Wiki/Syntheses/` | Agent plus human review | Stable syntheses distilled from sessions, human notes, and optional raw sources. |
| `_Wiki/Questions/` | Agent plus human review | Open research questions, uncertainties, and follow-up prompts. |
| workspace `AGENTS.md` | Coding agent | Auto-loaded entrypoint for Codex-like agents. |
| workspace `CLAUDE.md` | Coding agent | Auto-loaded entrypoint for Claude Code. |

## Workflow

1. Identify the project name and optional project goal.
2. Call `deep_obsidian.vault_info`.
3. Check the minimal required paths directly before doing broader folder listing:
   - `Projets/<Project>/`
   - `_Agent/Contract.md`
   - `_Agent/Contracts/<Project>.md`
   - `_Agent/Sessions/<Project>/Index.md`
   - `_Agent/Tasks/<Project>.md`
   - `_Agent/Log.md`
   - `_Wiki/Index.md`
4. Use `deep_obsidian.list_folders` or `deep_obsidian.list_children` only when direct reads are inconclusive or the project name may already exist with a variant spelling.
5. Read an existing scaffold note before updating it.
6. If `Projets/<Project>/` does not exist, create the folder with a minimal `.keep.md`; do not create a project hub note unless the user explicitly asks.
7. Create only missing scaffold notes. For existing notes, append only missing sections or leave them unchanged. Do not create `_Agent/Raw/<Project>/Index.md` unless raw material is explicitly supplied or already present.
8. Create or update workspace `AGENTS.md` and `CLAUDE.md` outside the vault.
9. Propose a recurring maintenance automation after initialization. Do not create the automation silently.
10. Verify each created or updated note with `deep_obsidian.read_file`.

## Idempotency And Efficiency

- Running this skill twice should not duplicate sections, tasks, index links, or automation text.
- Prefer direct path checks over broad vault scans.
- Do not read human project notes unless needed to disambiguate the project.
- Do not rewrite existing scaffold notes wholesale.
- Preserve manual additions, frontmatter, aliases, links, and local naming conventions.
- Keep created scaffold notes small; this skill initializes operating structure, it does not populate project knowledge.
- Treat `_Agent/Raw/<Project>/` as an optional staging inbox, not a required project area.
- If a scaffold note already exists with equivalent intent but different wording, keep it and add only missing operational constraints.
- If a project already has an active session or task structure, link to it instead of creating a parallel one.

## Recommended Scaffold Content

### `Projets/<Project>/.keep.md`

```md
# <Project>

This placeholder only keeps the human project folder present.

Agents must not use this file as a project hub. The human decides how to structure notes in this folder.
```

### `_Agent/Contract.md`

```md
---
type: vault-agent-contract
layer: agent
---

# Vault Agent Contract

## Boundaries

- Human project notes live in `Projets/`.
- Agent session traces live in `_Agent/Sessions/`.
- Optional raw source material lives in `_Agent/Raw/` only when provided or imported.
- Durable agent-maintained knowledge lives in `_Wiki/`.
- Formal deliverables live in `RFCs/`, `Blog/`, `Présentations/`, or other human-owned folders.

## Write Policy

- Do not write messy generated notes into `Projets/` by default.
- Read existing notes before updating them.
- Use dry-run for broad changes.
- Use hash guards when updating existing notes.
- Preserve frontmatter and manual sections.

## Knowledge Lifecycle

1. Capture messy work in `_Agent/Sessions/<Project>/`.
2. If raw material is explicitly provided or imported, stage it in `_Agent/Raw/<Project>/`.
3. Distill stable facts, concepts, decisions, and questions into `_Wiki/`.
4. Promote reviewed deliverables into human folders when explicitly requested.
5. Append important write operations to `_Agent/Log.md`.
```

### `_Agent/Contracts/<Project>.md`

```md
---
type: project-agent-contract
project: <Project>
layer: agent
---

# Agent Contract - <Project>

## Human Surface

Human notes live under:

- `Projets/<Project>/`

Do not create project hub notes, indexes, or operational notes there unless the user explicitly asks.

When linking to human notes, discover them through search, graph traversal, and the existing folder structure.

## Agent Workspace

- `_Agent/Sessions/<Project>/`
- `_Agent/Raw/<Project>/` when raw material is explicitly provided or imported
- `_Agent/Tasks/<Project>.md`

## Project-Specific Rules

- Keep messy generated work in `_Agent/Sessions/<Project>/`.
- Keep explicitly provided raw source material in `_Agent/Raw/<Project>/`.
- Distill stable project knowledge into `_Wiki/`.
- Ask before writing directly into `Projets/<Project>/`.

## Distillation Criteria

- Promote decisions when they affect future work.
- Promote concepts when they are reusable across sessions.
- Promote open questions when they need follow-up beyond the current session.
```

### `_Agent/Sessions/<Project>/Index.md`

```md
---
type: agent-session-index
project: <Project>
layer: agent
---

# Agent Sessions - <Project>

Use this folder for messy agent-created session notes. Do not treat these notes as verified durable knowledge until they are distilled into `_Wiki/`.

## Session Naming

- `YYYY-MM-DD - <short topic>.md`

## Promotion Rule

Important findings should be distilled into `_Wiki/Syntheses/`, `_Wiki/Decisions/`, or human project notes after review.
```

### Optional `_Agent/Raw/<Project>/Index.md`

```md
---
type: raw-source-index
project: <Project>
layer: agent
---

# Raw Sources - <Project>

Use this folder for unprocessed source material. Keep provenance and source links when available.

This folder is optional. Do not create or maintain raw notes just because the project exists. Use it only when the user provides transcripts, exports, copied source material, large snippets, logs, or other material that should be staged before distillation.
```

### `_Agent/Tasks/<Project>.md`

```md
---
type: agent-task-board
project: <Project>
layer: agent
status: active
---

# Agent Tasks - <Project>

## Always-On Duties

When actively working:

- Capture useful work traces in `_Agent/Sessions/<Project>/`.
- Link sessions to relevant human notes in `Projets/<Project>/`.
- Store raw source material in `_Agent/Raw/<Project>/` only when it is explicitly provided or imported.
- Append major writes, imports, distillations, and promotions to `_Agent/Log.md`.

When not busy:

- Review recent unpromoted sessions.
- Distill stable knowledge into `_Wiki/Syntheses/`.
- Extract durable decisions into `_Wiki/Decisions/`.
- Extract reusable concepts into `_Wiki/Concepts/`.
- Extract unresolved questions into `_Wiki/Questions/`.
- Update `_Wiki/Index.md`.

## Distillation Queue

- [ ] Distill unpromoted sessions from `_Agent/Sessions/<Project>/`.
- [ ] If `_Agent/Raw/<Project>/` contains unprocessed material, review it.
- [ ] Update project synthesis.
- [ ] Update open questions.
- [ ] Check stale decisions.

## Automation

Recommended maintenance cadence:

- active project: every 6 hours
- stable project: daily
- paused project: weekly
```

### `_Wiki/Index.md`

```md
---
type: wiki-index
layer: wiki
---

# Wiki Index

This wiki contains durable agent-maintained knowledge. It should be compact, sourced, linked, and periodically linted.

## Sections

- [[_Wiki/Concepts/Index|Concepts]]
- [[_Wiki/Decisions/Index|Decisions]]
- [[_Wiki/Syntheses/Index|Syntheses]]
- [[_Wiki/Questions/Index|Questions]]
```

### Workspace `AGENTS.md` / `CLAUDE.md`

These files belong in the coding workspace or repository, not in the vault. They are the auto-loaded entrypoints for coding agents. Use `CLAUDE.md` when Claude Code is used.

```md
# Obsidian Collaboration

You are now working with an Obsidian-backed project.
Project: `<Project>`.

1. Use the `deep_obsidian` MCP. Do not read or write the vault filesystem directly.
2. Read `_Agent/Contract.md` from the vault.
3. Read:
   - `_Agent/Contracts/<Project>.md`
   - `_Agent/Tasks/<Project>.md`
4. For project-scoped work, maintain a session note in `_Agent/Sessions/<Project>/` when work produces durable context, decisions, artifacts, or follow-ups.
5. Treat `Projets/<Project>/` as human-owned. Do not impose structure there.
6. Distill stable knowledge into `_Wiki/`.
7. Ask before promoting content into human folders such as `Projets/`, `RFCs/`, `Blog/`, or `Présentations/`.
```

## Session Capture Rule

For project-scoped work, maintain a session note in `_Agent/Sessions/<Project>/` when any of these are true:

- the user asks to work on the project
- files, notes, code, or artifacts are changed
- a decision, tradeoff, blocker, or open question appears
- raw material is ingested
- the task spans more than one short answer
- the agent plans to update `_Wiki/`

Do not create a session note for trivial Q&A, simple lookups, or purely conversational replies.

## Optional Maintenance Automation

After initialization, ask the user whether to create a recurring maintenance automation. Do not silently create it.

Recommended modes:

| Project state | Cadence |
|---|---|
| active | every 6 hours |
| stable | daily |
| paused | weekly |

Prefer a detached recurring cron automation for vault maintenance. Use a thread heartbeat only when the user wants this exact conversation to continue later.

Automation prompt:

```md
Maintain the Obsidian agent/wiki workspace for project <Project>.

Use only the deep_obsidian MCP. Do not use raw filesystem access.

Workflow:
1. Call vault_info.
2. Read:
   - _Agent/Contract.md
   - _Agent/Contracts/<Project>.md
   - _Agent/Tasks/<Project>.md
3. Use search, note_outline, and indexes to find recent unpromoted sessions.
4. Check `_Agent/Raw/<Project>/` only if it exists and contains explicitly staged raw material.
5. Read only the source and target notes needed for one maintenance action.
6. Perform at most one small maintenance task:
   - distill a session into _Wiki/Syntheses/
   - distill a staged raw source into _Wiki/Syntheses/
   - extract a decision into _Wiki/Decisions/
   - extract a reusable concept into _Wiki/Concepts/
   - update _Wiki/Questions/
   - update _Wiki/Index.md
7. Use dryRun for broad changes.
8. Do not modify Projets/<Project>/ or RFCs/ unless explicitly instructed.
9. Append a short entry to _Agent/Log.md when a write is made.
10. Report what changed, or say no maintenance was needed.
```

Suggested user-facing proposal:

```md
Project wiki initialized for <Project>. I can also create a recurring Deep Obsidian maintenance automation that distills new sessions into `_Wiki`, updates `_Agent/Log.md`, and keeps the project knowledge base tidy. Want me to set that up?
```

## Safety Rules

- Never move existing human notes during initialization.
- Do not create or rewrite human project notes in `Projets/<Project>/` unless explicitly requested.
- Do not overwrite existing scaffold notes wholesale.
- Do not create recurring automations without explicit user approval.
- Do not rely on a vault-root `AGENTS.md` as the auto-loaded coding-agent entrypoint.
- Prefer workspace `AGENTS.md` and `CLAUDE.md` for auto-loaded instructions.
- Preserve the user's current project architecture.
- Keep generated scaffold text short and useful.
- If the requested project already has a strong structure, adapt to it instead of forcing a template.

## Preferred MCP Tools

- `vault_info`: verify connectivity and index state
- `read_file`: check existing scaffold notes and verify writes
- `list_folders`: disambiguate project folders when direct path checks are not enough
- `list_children`: inspect only the specific folder being initialized
- `upsert_note`: create missing scaffold notes
- `update_note_section`: add missing sections without rewriting whole notes
- `write_file_to_vault`: create minimal placeholder files when folder creation requires a file
