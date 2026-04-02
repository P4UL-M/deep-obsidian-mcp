# Writing Conventions Pattern

This document defines the recommended pattern for exposing reusable Obsidian writing and formatting conventions through MCP without turning tool descriptions into documentation dumps.

## Goal

Support conventions such as:

- custom callouts like `legend`
- snippet-backed markdown patterns
- status rendering conventions
- reusable note-writing rules

The conventions should be:

- documented once
- easy for agents to discover
- easy for writing workflows to enforce
- separate from low-level tool contracts

## Recommendation

Use this split:

1. MCP resource: canonical convention documentation
2. skill or writing workflow: explicit instruction to read and apply that resource
3. tool description: short pointer only

Do not make long-form snippet documentation live in the tool descriptions themselves.

## Why This Split

### MCP resource

The resource is the single source of truth for the actual conventions.

Good fit for:

- custom callout names
- markdown examples
- CSS snippet references
- usage guidance
- fallback rules

### skill or writing workflow

The skill is where behavioral enforcement belongs.

Good fit for:

- "before writing, read the conventions resource"
- "apply documented custom patterns when relevant"
- "do not invent undocumented custom syntax"
- "fall back to plain markdown if conventions are unavailable"

### tool description

Tool descriptions should stay short and operational.

Good fit for:

- what the tool does
- required inputs
- output shape
- side effects
- one short pointer to the conventions resource

Bad fit for:

- long snippet examples
- workflow policy
- formatting playbooks

## Canonical Resource Shape

Recommended resource URI:

- `obsidian://conventions/writing`

Recommended document structure:

```md
# Obsidian Writing Conventions

Version: 1
Scope: Obsidian note writing in this workspace
Applies to: note creation, note updates, templates, summaries

## Principle
Use documented custom Markdown and UI patterns when they improve readability.
Do not invent new callout names unless they are documented here.

## Custom Patterns

### legend
Purpose: muted subtext under a paragraph
Use when:
- adding a legend
- adding secondary clarification
- adding lightweight caveats without a full callout

Syntax:
> [!legend]
> Smaller muted subtext.

Visual behavior:
- smaller text
- muted color
- no icon
- no visible title

Backed by:
- `.obsidian/snippets/legend-callout.css`

### meteo-status
Purpose: render project health with emoji
Use when:
- summarizing status from a `status` property

Examples:
- `green` -> `🟢`
- `orange` -> `🟠`
- `red` -> `🔴`

## Fallback Rules
- If a custom pattern is unavailable, fall back to standard Markdown.
- Do not reference undocumented snippets.

## Examples

Regular paragraph.

> [!legend]
> This is supporting context.
```

## Skill Integration Pattern

Any skill or workflow that writes Obsidian content should include a short conventions-loading rule.

Recommended structure:

```md
## Writing Conventions

Before drafting Obsidian content, read `obsidian://conventions/writing` when available.

Rules:
- Apply documented custom patterns only when relevant.
- Prefer `legend` for muted explanatory subtext under a paragraph.
- Do not invent new custom callouts or snippet-dependent syntax not documented in the conventions resource.
- If the conventions resource is unavailable, fall back to standard Obsidian Markdown.
```

## Tool Description Pattern

Keep the tool description short:

```txt
Writes notes to the Obsidian vault. For formatting conventions and custom callouts, consult `obsidian://conventions/writing`.
```

Do not duplicate the full conventions document in the tool description.

## Optional MCP Prompt

An MCP prompt can be added as a convenience entrypoint, but it should not replace the resource-plus-skill pattern.

Use an MCP prompt when you want:

- a user-invoked slash-command style helper
- a reusable template for writing notes
- optional guided invocation

Do not rely on the prompt alone for durable conventions enforcement.

## Implementation Plan

1. Add a canonical conventions resource to the MCP server.
2. Keep the document concise, example-driven, and versioned.
3. Update writing skills to read the conventions resource before generating note content.
4. Add only a short pointer from tool descriptions.
5. Reuse the same resource across note-writing, summarization, and template workflows.

## Decision

For snippet-backed writing conventions, the preferred architecture is:

- resource for documentation
- skill for enforcement
- tool description for discovery

This keeps conventions centralized without overloading the MCP tool surface.
