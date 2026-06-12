---
name: obsidian-knowledge-maintenance
description: Maintain durable Obsidian knowledge through Deep Obsidian MCP. Use when the user wants to distill agent sessions, extract decisions, promote stable knowledge, refactor notes, clean links, or decide where project memory belongs.
---

# Obsidian Knowledge Maintenance

## Purpose

Use this skill for durable knowledge work after information has already been captured or discovered.

This skill turns messy or temporary material into stable, linked, auditable notes. It is deliberately conservative: do one bounded maintenance task at a time unless the user explicitly approves a broader batch.

All vault access must go through `deep_obsidian`. Do not read or write the vault filesystem directly.

## Activation

Use this skill for requests such as:

- "distill these sessions"
- "record this as a decision"
- "promote this into the wiki"
- "clean/refactor these notes"
- "deduplicate this knowledge"
- "decide where this should be remembered"
- recurring maintenance automation for `_Agent` and `_Wiki`

Do not use this skill for first-pass session capture. Use `obsidian-capture-session` for that.

Do not use this skill for read-only context loading, project briefings, or daily review. Those belong to MCP prompts.

## Required MCP Flow

1. Call `deep_obsidian.vault_info`.
2. Identify project, subject, requested outcome, and primary mode.
3. Read `_Agent/Contract.md` when it exists.
4. If project is known, read `_Agent/Contracts/<Project>.md` and `_Agent/Tasks/<Project>.md` when they exist.
5. Use search and outlines before full-note reads.
6. Read only candidate notes that are likely to be changed or cited.
7. Use `dryRun` before broad, multi-note, uncertain, or human-folder writes.
8. Use `expectedHash` when updating an existing note and a hash is available.
9. Write the smallest useful durable change.
10. Verify changed notes with `read_file` and link/graph checks when relevant.
11. Append a short entry to `_Agent/Log.md` for meaningful distillations, promotions, imports, refactors, and decisions.

If `deep_obsidian` is unavailable, stop and report the blocker. Do not fall back to raw filesystem access.

## Pick One Mode

Pick exactly one primary mode before writing:

| Mode | Use when |
|---|---|
| Distill | Agent sessions, explicitly staged raw sources, or scattered context should become durable wiki knowledge. |
| Decision | A technical, product, architecture, or workflow decision should be recorded. |
| Refactor | Existing notes should be split, merged, retitled, relinked, deduplicated, or cleaned. |
| Memory Policy | The main question is whether something should be remembered and where it belongs. |

If the request spans several modes, propose a short write plan and ask before doing broad changes.

For recurring maintenance, perform at most one small task per run.

## Efficiency Rules

- Prefer `note_outline`, `hybrid_search`, `grep_search`, and `graph_traverse` (use `direction:"incoming"` for backlinks) before `read_file` on large notes.
- Prefer `read_file` with `startLine`/`endLine` over a full read when only one section is needed.
- Do not scan the whole vault manually.
- Do not read every session note. Search or inspect indexes first.
- Prefer updating an existing durable note over creating near-duplicates.
- Keep outputs compact. Use links to source sessions instead of copying their full content.
- Batch related small updates only when they are clearly part of the same maintenance action.

## Target Locations

Default durable destinations:

| Content | Destination |
|---|---|
| Stable synthesis | `_Wiki/Syntheses/` |
| Reusable concept | `_Wiki/Concepts/` |
| Durable decision | `_Wiki/Decisions/` |
| Open question | `_Wiki/Questions/` |
| Raw source or transcript explicitly provided for processing | `_Agent/Raw/<Project>/` |
| Work trace | `_Agent/Sessions/<Project>/` |
| Project maintenance queue | `_Agent/Tasks/<Project>.md` |
| Write/distillation log | `_Agent/Log.md` |

Human-owned folders such as `Projets/`, `RFCs/`, `Blog/`, and `Présentations/` require explicit user approval before writes or promotions.

## Distill Mode

Turn captured sessions, explicitly staged raw sources, or scattered context into durable wiki knowledge.

Workflow:

1. Identify the source note or source set.
2. Use `note_outline` or search to find the relevant source sections.
3. Search `_Wiki/` for an existing durable note that should be updated.
4. Read only the selected source sections and target durable notes.
5. Extract stable facts, decisions, concepts, unresolved questions, and source links.
6. Write one durable note update or one new durable note.
7. Link back to the source sessions or raw notes.
8. Mark source status only if the vault convention supports it; otherwise leave a log entry.

Raw notes are optional. Do not look for `_Agent/Raw/<Project>/` as a routine duty unless the user mentions raw material, an index shows staged sources, or search finds unprocessed raw notes.

Distillation output should be concise and auditable:

```md
---
type: synthesis
project: <Project>
source:
  - [[_Agent/Sessions/<Project>/<Session>]]
layer: wiki
---

# <Topic>

## Summary

## Stable Knowledge

## Evidence

## Related Notes

## Follow-Up
```

Do not rewrite the source session unless the user asks.

## Decision Mode

Create or update a decision note when the intent to record a decision is clear.

Default destination:

```text
_Wiki/Decisions/YYYY-MM-DD - <short-kebab-decision>.md
```

Default shape:

```md
---
type: decision
project: <Project>
date: YYYY-MM-DD
status: proposed | accepted | superseded
layer: wiki
---

# <Decision>

## Context

## Decision

## Options Considered

## Tradeoffs

## Consequences

## Revisit Criteria

## Sources

## Related Notes
```

Rules:

- Make the decision explicit and dated.
- Preserve uncertainty and rejected options.
- Link to the session, issue, PR, human note, or source that motivated the decision.
- Do not write a decision note when the user only made a passing comment.

## Refactor Mode

Safely split, merge, retitle, relink, or clean notes.

Workflow:

1. Load target outlines first.
2. Read full target notes only after the touched set is known.
3. Search backlinks and graph neighbors before changing links.
4. Propose touched files, moved content, link changes, and risks.
5. Use `dryRun` first unless the change is a single obvious edit.
6. Preserve frontmatter, manual sections, aliases, and valid Obsidian links.
7. Prefer small reversible edits over sweeping restructuring.

Do not move human-owned notes unless the user asks for that exact move.

## Memory Policy Mode

Use this mode when deciding whether and where to persist information.

Policy:

- Session-only work history belongs in `_Agent/Sessions/<Project>/`.
- Stable reusable knowledge belongs in `_Wiki/`.
- Decisions with future consequences belong in `_Wiki/Decisions/`.
- Raw material belongs in `_Agent/Raw/<Project>/` only when it is explicitly provided or imported for processing.
- Human-facing deliverables belong in human-owned folders only after approval.
- Sensitive material, secrets, tokens, credentials, and raw `.env` content should not be stored.
- Personal preferences or long-term user memory require clear user intent.

When uncertain, ask whether the memory should be global, project-local, or session-only.

## Related Notes And Links

Use real Obsidian wiki links.

- Prefer exact path links when folder ambiguity exists.
- Use aliases for readability.
- Cap newly added related links at 5 unless the user asks for a larger map.
- Do not add weak links just to make the graph denser.
- Preserve existing link style when updating a note.

Example:

```md
- [[_Agent/Sessions/Deep Obsidian/2026-05-06 - release-icons|Release icon session]]
- [[_Wiki/Decisions/SQLite Incremental Indexing]]
- [[Projets/Deep Obsidian/Human Planning Note|Human planning note]]
```

## Log Entry

Append a short entry to `_Agent/Log.md` after meaningful maintenance.

Use this compact shape:

```md
- YYYY-MM-DD: <action> for <Project>. Sources: [[...]]. Outputs: [[...]].
```

Do not log trivial read-only exploration.

## Safety Rules

- Do not use raw filesystem access for vault content.
- Do not perform broad rewrites without explicit approval.
- Do not write messy generated content into human-owned folders.
- Do not overwrite scaffold or human notes wholesale.
- Preserve manual content, frontmatter, aliases, and existing style.
- Use `dryRun` and `expectedHash` for risky writes.
- Keep each maintenance run bounded and reviewable.

## Preferred MCP Tools

- `vault_info`: verify connectivity and index state
- `read_file`: read contracts, selected targets, and verify writes (use `startLine`/`endLine` to inspect a section without loading the whole note)
- `note_outline`: inspect long notes cheaply
- `hybrid_search`: find candidate source and target notes
- `grep_search`: find exact phrases, IDs, file paths, decisions, or headings
- `graph_traverse`: inspect local graph context (use `direction:"incoming"`, `depth:1` for backlinks before refactors)
- `upsert_note` or write tools: create/update durable notes with hash guards when available
