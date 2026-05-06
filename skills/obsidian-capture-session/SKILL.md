---
name: obsidian-capture-session
description: Create or update a project-scoped Obsidian agent session note through Deep Obsidian MCP. Use when the user asks to capture, save, remember, log, or persist the current work session, implementation session, research session, or project memory into their vault.
---

# Obsidian Capture Session

## Purpose

Use this skill when the user explicitly wants the current work captured into Obsidian.

The output is a compact session synthesis, not a transcript. It belongs in the agent workspace by default, usually `_Agent/Sessions/<Project>/`, and can later be distilled into `_Wiki/` by `obsidian-knowledge-maintenance`.

All vault access must go through `deep_obsidian`. Do not read or write the vault filesystem directly.

## Activation

Use this skill for requests such as:

- "capture this session"
- "save this in Obsidian"
- "record what we did"
- "write the session note"
- "update project memory"
- "log this work"

Do not use this skill for general context retrieval, daily review, project briefing, or durable wiki distillation unless the user explicitly asks to write a session capture.

## Required MCP Flow

1. Call `deep_obsidian.vault_info` to confirm the server is reachable.
2. Read `_Agent/Contract.md` when it exists.
3. Identify the project name from the conversation, workspace, repository, current task, or user instruction.
4. If a project is known, read `_Agent/Contracts/<Project>.md` and `_Agent/Tasks/<Project>.md` when they exist.
5. Reuse a known session note path from this conversation when available.
6. If no path is known, create or update a session note under `_Agent/Sessions/<Project>/` when a project is known.
7. Search for related notes with `related_notes` for an existing note, otherwise `hybrid_search`.
8. Draft dense Markdown in the user's language unless the user asks for another language.
9. Write with `deep_obsidian.upsert_session_note`.
10. Verify the saved note with `deep_obsidian.read_file`.

If `deep_obsidian` is unavailable, stop and report the blocker. Do not fall back to raw filesystem access.

## Project And Path Resolution

Prefer project-scoped capture. Choose the project name in this order:

1. Explicit project name provided by the user.
2. Project name in workspace `AGENTS.md` or `CLAUDE.md`.
3. Repository or package name currently being worked on.
4. Strong project signal from the conversation.

If the project is still ambiguous, ask one short clarification instead of writing to a generic folder.

Default session folder:

```text
_Agent/Sessions/<Project>/
```

Default filename:

```text
YYYY-MM-DD - <short-kebab-topic>.md
```

Use the current local date. Keep the topic short and stable. Prefer updating the same note over creating near-duplicates.

Only use `deep_obsidian.recommend_folder` when there is no project-scoped agent workspace and the user did not provide a target folder. Do not route messy session captures into human-owned folders such as `Projets/`, `RFCs/`, `Blog/`, or `Présentations/` unless the user explicitly asks.

## Session Identity

Treat the current session note as the note with:

- the current local date
- the project name
- the normalized session topic
- the selected session folder

When a previous capture in the same conversation already returned a path, topic, or folder:

- reuse that exact note identity by default
- prefer passing the exact prior `path` to `deep_obsidian.upsert_session_note` when supported
- update that same note instead of inventing a slightly different topic
- create a new note only when the user explicitly asks for a separate capture

If an existing same-day note covers the same work, update it.

## What To Capture

Capture high-signal project memory:

- user intent and problem framing
- implementation or research work completed
- files, commands, releases, packages, assets, or automations touched
- decisions and tradeoffs
- technical findings and constraints
- unresolved questions
- follow-up tasks
- links to relevant existing notes

Do not copy large chunks of conversation verbatim. Do not store secrets, tokens, `.env` contents, private credentials, or unnecessary personal data.

## Default Note Shape

Draft the body exactly as it should be stored. `deep_obsidian.upsert_session_note` does not add a title automatically.

```md
---
type: agent-session
project: <Project>
date: YYYY-MM-DD
layer: agent
status: captured
---

# YYYY-MM-DD - <Readable Topic>

## Context

## Work Completed

## Technical Findings

## Decisions

## Artifacts

## Related Notes

## Open Questions

## Next Steps
```

Omit empty sections instead of adding filler. Use the user's language by default, while keeping paths, symbols, commands, and identifiers exact.

## Related Notes

Include real Obsidian wiki links when there are strong matches. Do not create weak graph noise.

Preferred strategy:

1. If updating an existing session note, call `deep_obsidian.related_notes` on that note path.
2. Otherwise call `deep_obsidian.hybrid_search` with the project name, session topic, and key technical terms.
3. Convert strong note paths into Obsidian wiki links.
4. Exclude the note currently being written.
5. Cap `## Related Notes` at 5 links.

Prefer exact links:

```md
- [[Projets/Deep Obsidian/Some Human Note|Some Human Note]]
- [[_Wiki/Decisions/SQLite Incremental Indexing]]
- [[_Agent/Sessions/Deep Obsidian/2026-05-06 - release-icons]]
```

Use aliases when the stored path is long but a clean display title is better:

```md
[[Knowledge Capture/Sessions/Session - deep-obsidian-mcp-codebase-inspection-and-current-behavior|Deep Obsidian codebase inspection]]
```

Do not add a `Related Notes` section when no strong candidates exist.

## Human-Owned Folder Boundary

Human folders are not session storage. Treat these as human-owned unless the user says otherwise:

- `Projets/`
- `RFCs/`
- `Blog/`
- `Présentations/`
- any folder explicitly described by the user as human-owned

Rules:

- It is fine to link to human notes.
- It is fine to read human notes through `deep_obsidian` for context.
- Do not create project hubs, indexes, contracts, or messy session notes inside human folders.
- Ask before promoting content from `_Agent/Sessions/` or `_Wiki/` into a human-owned folder.

## Update Semantics

When updating an existing session note:

- replace the generated session synthesis with the new version
- preserve frontmatter unless it is clearly generated by this skill and needs a narrow update
- preserve a trailing `## Manual Notes` section when present
- preserve explicit human additions that are clearly outside the generated structure
- keep the same path unless the user asks for a rename

Use `expectedHash` when the tool provides a hash and the write may conflict with recent user edits.

## Distillation Boundary

This skill captures the session. It should not perform broad distillation by default.

Allowed:

- add a short `## Next Steps`
- add a short distillation candidate list
- link to existing `_Wiki/` notes
- mention what should be promoted later

Not allowed unless explicitly requested:

- rewrite durable wiki notes
- promote content into human-owned folders
- refactor multiple notes
- create decision records outside the session note

For that work, use `obsidian-knowledge-maintenance`.

## Recommended Write Flow

1. Resolve project and target path.
2. Read existing target note if it exists.
3. Retrieve relevant context with `hybrid_search`, `related_notes`, `note_outline`, or `read_file`.
4. Draft the final Markdown in memory.
5. Preserve manual sections from the existing note.
6. Write with `deep_obsidian.upsert_session_note`, using `path` when available.
7. Verify with `deep_obsidian.read_file`.
8. Report the saved path and any follow-up maintenance suggestion.

## Preferred MCP Tools

- `vault_info`: verify connectivity and index state
- `read_file`: read contracts, existing sessions, and verify saved content
- `hybrid_search`: retrieve related project notes before writing
- `related_notes`: find adjacent notes for an existing session
- `note_outline`: inspect long notes before reading full content
- `upsert_session_note`: create or update the session note
- `grep_search`: find exact terms when search precision matters
