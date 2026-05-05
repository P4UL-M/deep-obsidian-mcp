---
name: obsidian-capture-session
description: Create or update a structured Obsidian session note through Deep Obsidian MCP. Use when the user asks to capture, remember, log, or save the current work session into their vault.
---

# Obsidian Capture Session

Use this skill when the user explicitly wants the current work captured into Obsidian. The note should be a synthesis, not a transcript. All vault actions must go through `deep_obsidian`.

## Workflow

1. Call `deep_obsidian.vault_info` to confirm the server is reachable.
2. Reuse a known session note path from this conversation when available.
3. If no path is known, call `deep_obsidian.recommend_folder` with the topic and optional project hint.
4. Find related notes with `related_notes` for an existing note, otherwise `hybrid_search`.
5. Draft dense Markdown in English unless the user requests another language.
6. Include real Obsidian wiki links in `## Related Notes` when there are strong matches, capped at 5.
7. Write with `deep_obsidian.upsert_session_note`, preserving manual notes.
8. Verify the saved note with `deep_obsidian.read_file`.

## Default Note Shape

```md
Date: YYYY-MM-DD
Project: <project or repo>

## Context

## Work Completed

## Technical Findings

## Decisions

## Artifacts

## Related Notes

## Open Questions

## Next Steps
```

Omit empty sections instead of adding filler.

## Safety Rules

- Update the same session note instead of creating near-duplicates.
- Preserve a trailing `## Manual Notes` section.
- Do not use raw filesystem access for vault writes.
- If Deep Obsidian is unavailable, stop and report the blocker.
