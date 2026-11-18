//! Static shell completions generated locally from the management command tree.
use std::io::{self, Write};

use anyhow::Result;
use clap::Command;
use clap_complete::Shell;

pub(crate) fn print(shell: Shell) -> Result<()> {
    let mut command = crate::help::command();
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut command, "deep-obsidian-cli", &mut script);
    if shell == Shell::Fish {
        // clap_complete's Fish generator stops before third-level subcommands
        // (mounts add couchdb). Generate those leaves as standalone commands,
        // then scope their directives to the real path. Keep option escaping,
        // value hints and choices in the upstream generator.
        add_fish_completions(&command, &[], &mut script);
    }
    io::stdout().lock().write_all(&script)?;
    Ok(())
}

fn add_fish_completions(command: &Command, parents: &[&str], script: &mut Vec<u8>) {
    for child in command.get_subcommands() {
        let mut path = parents.to_vec();
        path.push(child.get_name());
        let condition = std::iter::once(format!(
            "__fish_deep_obsidian_cli_using_subcommand {}",
            fish_quote(path[0])
        ))
        .chain(
            path[1..]
                .iter()
                .map(|name| format!("__fish_seen_subcommand_from {}", fish_quote(name))),
        )
        .collect::<Vec<_>>()
        .join("; and ");
        // Upstream Fish output omits positional choices, such as the shell
        // argument of `completions`. Read their vocabulary from clap as well.
        for arg in child.get_positionals() {
            let values = arg.get_possible_values();
            let choices = values
                .iter()
                .filter(|value| !value.is_hide_set())
                .map(|value| fish_quote(value.get_name()))
                .collect::<Vec<_>>()
                .join(" ");
            if !choices.is_empty() {
                script.extend_from_slice(
                    format!(
                        "complete -c deep-obsidian-cli -n \"{condition}\" -f -a \"{choices}\"\n"
                    )
                    .as_bytes(),
                );
            }
        }
        if path.len() >= 3 && !child.has_subcommands() {
            let mut leaf = child.clone().name("deep-obsidian-cli");
            let mut generated = Vec::new();
            clap_complete::generate(Shell::Fish, &mut leaf, "deep-obsidian-cli", &mut generated);
            // With no subcommands, Fish output consists only of completion
            // directives. A directive may span lines when it lists choices.
            let scoped = String::from_utf8(generated)
                .expect("Fish completions are UTF-8")
                .replace(
                    "complete -c deep-obsidian-cli",
                    &format!("complete -c deep-obsidian-cli -n \"{condition}\""),
                );
            script.extend_from_slice(scoped.as_bytes());
        }
        add_fish_completions(child, &path, script);
    }
}

fn fish_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}
