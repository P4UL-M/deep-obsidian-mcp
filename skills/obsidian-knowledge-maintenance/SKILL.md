---
name: obsidian-knowledge-maintenance
description: Maintain durable Obsidian knowledge through Deep Obsidian MCP. Use when the user wants to distill sessions, record decisions, refactor notes, clean links, or decide what should be persisted.
---

# Obsidian Knowledge Maintenance

Use this skill for durable knowledge work in an Obsidian vault. It combines distillation, decision capture, note refactoring, and memory policy. All vault access must go through `deep_obsidian`; do not read or write the vault filesystem directly.

## Choose The Mode

Pick exactly one primary mode before writing:

| Mode | Use when |
|---|---|
| Distill | Raw sessions or scattered context should become durable knowledge. |
| Decision | The user wants to record a technical, product, architecture, or workflow decision. |
| Refactor | Notes should be split, merged, retitled, relinked, deduplicated, or cleaned. |
| Memory Policy | You need to decide whether something should be remembered and where it belongs. |

If the request spans several modes, propose a short write plan first and ask before broad changes.

## Common Workflow

1. Identify the project, subject, target note path, and intended mode.
2. Call `deep_obsidian.vault_info`.
3. Retrieve context with `load_knowledge`, `hybrid_search`, `note_outline`, `read_file`, `read_chunk`, `backlinks`, or `graph_traverse` as needed.
4. Reread existing target notes before updating them.
5. Prefer `dryRun` for broad changes, note refactors, multi-note writes, or uncertain routing.
6. Use `expectedHash` when updating existing notes if a hash is available.
7. Write the smallest useful change.
8. Verify changed notes with `read_file` and graph/link checks when relevant.

## Distill Mode

Turn raw sessions, worklogs, imported material, or scattered context into durable notes.

1. Retrieve raw sessions and related durable notes.
2. Identify what belongs in wiki knowledge, project knowledge, decisions, or follow-up tasks.
3. Preserve source links so the distillation is auditable.
4. Prefer updating an existing durable note over creating a near-duplicate.
5. Keep raw history separate from distilled knowledge.

## Decision Mode

Create or update a decision note.

Use this shape unless the local vault has a stronger convention:

```md
Date: YYYY-MM-DD
Project: <project or domain>
Status: proposed | accepted | superseded

## Context

## Decision

## Options Considered

## Tradeoffs

## Consequences

## Revisit Criteria

## Related Notes
```

Rules:

- Make the decision explicit and dated.
- Preserve uncertainty instead of over-polishing it away.
- Do not write a decision note unless the user intent to record the decision is clear.

## Refactor Mode

Safely split, merge, retitle, relink, or clean notes.

1. Load target notes with `note_outline` and `read_file`.
2. Search backlinks and graph neighbors before changing links.
3. Propose files touched, content moved, links changed, and risk.
4. Use `dryRun` first.
5. Preserve frontmatter, manual notes, and valid Obsidian links.
6. Prefer small reversible edits over sweeping restructuring.

## Memory Policy Mode

Apply this policy before persisting memory:

- Ask before persisting personal preferences, long-term decisions, or sensitive information unless the user clearly requested capture.
- Prefer session notes for work history and durable notes for stable knowledge.
- Prefer decision notes for tradeoffs that should be revisited later.
- Avoid storing secrets, tokens, private credentials, or raw `.env` contents.
- Use project and domain hints to route memory.
- When uncertain, ask whether the memory should be global, project-local, or session-only.

## Safety Rules

- Do not use raw filesystem access for vault content.
- Do not perform broad rewrites without explicit approval.
- Do not move human-owned notes unless the user asks for that exact change.
- Do not overwrite existing scaffold or human notes wholesale.
- Preserve manual content, frontmatter, and existing style.
- Prefer alias wiki links with clean display titles when creating Obsidian links.
