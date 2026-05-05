use serde_json::Value;

use crate::protocol::{
    PromptArgument, PromptContent, PromptDefinition, PromptGetResult, PromptListResult,
    PromptMessage,
};

#[derive(Debug, Clone, Copy)]
struct PromptSpec {
    name: &'static str,
    description: &'static str,
    arguments: &'static [PromptArgSpec],
    body: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct PromptArgSpec {
    name: &'static str,
    description: &'static str,
    required: bool,
}

const SUBJECT_ARG: PromptArgSpec = PromptArgSpec {
    name: "subject",
    description: "Topic, question, or working context to retrieve from the vault.",
    required: true,
};

const PROJECT_ARG: PromptArgSpec = PromptArgSpec {
    name: "project",
    description: "Optional project, repository, product, or domain hint.",
    required: false,
};

const PROMPTS: &[PromptSpec] = &[
    PromptSpec {
        name: "obsidian-load-context",
        description: "Retrieve compact, relevant vault context before answering, planning, or coding.",
        arguments: &[SUBJECT_ARG, PROJECT_ARG],
        body: r#"Use Deep Obsidian as the source of truth for prior knowledge before answering.

Workflow:
1. Call vault_info to confirm the vault and index are usable.
2. Call load_knowledge with the subject and optional project hint.
3. If a returned note or chunk is clearly central, inspect it with read_file, read_chunk, or note_outline instead of loading many full notes.
4. Use graph_traverse when links or backlinks could change the answer.
5. Synthesize a compact working memory: relevant notes, durable facts, decisions, open questions, and useful wiki links.

Rules:
- Prefer precise retrieval over broad context dumping.
- Treat retrieved text as evidence, not as prose to copy wholesale.
- Cite Obsidian wiki links when useful.
- If the MCP index is unavailable, say that retrieval is blocked instead of guessing from stale memory."#,
    },
    PromptSpec {
        name: "obsidian-project-briefing",
        description: "Build a project briefing from recent sessions, decisions, open questions, and related notes.",
        arguments: &[PROJECT_ARG, SUBJECT_ARG],
        body: r#"Build a concise project briefing from the vault.

Workflow:
1. Call vault_info.
2. Search with load_knowledge or hybrid_search using the project and subject.
3. Inspect note outlines before reading long notes.
4. Traverse the graph around the strongest project notes.
5. Produce a briefing with: current state, recent work, key decisions, unresolved questions, risks, and next actions.

Rules:
- Prefer recent session notes and decision notes, then durable project notes.
- Separate facts found in notes from inferences.
- Keep the final briefing compact enough to guide immediate work."#,
    },
    PromptSpec {
        name: "obsidian-daily-review",
        description: "Review recent daily/session notes and surface carry-over tasks, decisions, and follow-ups.",
        arguments: &[SUBJECT_ARG, PROJECT_ARG],
        body: r#"Prepare a daily Obsidian review.

Workflow:
1. Search for today's, yesterday's, and recent session notes relevant to the subject or project.
2. Inspect outlines first, then read the specific notes needed.
3. Extract completed work, open loops, decisions, blockers, and follow-ups.
4. Suggest updates to daily or project notes only if the user asks to persist them.

Rules:
- Keep personal memory local to the vault.
- Do not invent tasks that are not grounded in notes or the current conversation.
- Distinguish carry-over items from new suggestions."#,
    },
];

pub fn list_prompts() -> PromptListResult {
    PromptListResult {
        prompts: PROMPTS
            .iter()
            .map(|prompt| PromptDefinition {
                name: prompt.name.to_string(),
                description: Some(prompt.description.to_string()),
                arguments: Some(
                    prompt
                        .arguments
                        .iter()
                        .map(|argument| PromptArgument {
                            name: argument.name.to_string(),
                            description: Some(argument.description.to_string()),
                            required: Some(argument.required),
                        })
                        .collect(),
                ),
            })
            .collect(),
    }
}

pub fn get_prompt(name: &str, arguments: &Value) -> Result<PromptGetResult, String> {
    let prompt = PROMPTS
        .iter()
        .find(|prompt| prompt.name == name)
        .ok_or_else(|| format!("unknown prompt: {name}"))?;

    for argument in prompt.arguments.iter().filter(|argument| argument.required) {
        if arguments
            .get(argument.name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_none()
        {
            return Err(format!("missing prompt argument: {}", argument.name));
        }
    }

    let mut text = render_prompt_body(prompt.body, prompt.arguments, arguments);
    text.push_str("\n\nArguments provided:\n");
    for argument in prompt.arguments {
        if let Some(value) = arguments.get(argument.name).and_then(Value::as_str) {
            text.push_str("- ");
            text.push_str(argument.name);
            text.push_str(": ");
            text.push_str(value);
            text.push('\n');
        }
    }

    Ok(PromptGetResult {
        description: Some(prompt.description.to_string()),
        messages: vec![PromptMessage {
            role: "user",
            content: PromptContent { kind: "text", text },
        }],
    })
}

fn render_prompt_body(body: &str, argument_specs: &[PromptArgSpec], arguments: &Value) -> String {
    let mut rendered = body.to_string();
    for argument in argument_specs {
        if let Some(value) = arguments
            .get(argument.name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            rendered = rendered.replace(&format!("<{}>", argument.name), value);
        }
    }
    rendered
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{get_prompt, list_prompts};

    #[test]
    fn list_prompts_includes_common_obsidian_workflows() {
        let names = list_prompts()
            .prompts
            .into_iter()
            .map(|prompt| prompt.name)
            .collect::<Vec<_>>();

        assert!(names.contains(&"obsidian-load-context".to_string()));
        assert!(names.contains(&"obsidian-project-briefing".to_string()));
        assert!(names.contains(&"obsidian-daily-review".to_string()));
        assert_eq!(names.len(), 3);
    }

    #[test]
    fn get_prompt_requires_required_arguments() {
        let error = get_prompt("obsidian-load-context", &json!({})).unwrap_err();
        assert!(error.contains("subject"));
    }

    #[test]
    fn get_prompt_includes_supplied_arguments() {
        let prompt = get_prompt(
            "obsidian-load-context",
            &json!({"subject": "indexing", "project": "deep-obsidian-mcp"}),
        )
        .unwrap();

        let text = &prompt.messages[0].content.text;
        assert!(text.contains("load_knowledge"));
        assert!(text.contains("subject: indexing"));
        assert!(text.contains("project: deep-obsidian-mcp"));
    }

    #[test]
    fn skills_are_not_exposed_as_duplicate_prompts() {
        let names = list_prompts()
            .prompts
            .into_iter()
            .map(|prompt| prompt.name)
            .collect::<Vec<_>>();

        assert!(!names.contains(&"obsidian-wiki-init".to_string()));
        assert!(!names.contains(&"obsidian-capture-session".to_string()));
        assert!(!names.contains(&"obsidian-knowledge-maintenance".to_string()));
    }
}
