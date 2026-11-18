//! Completion is a local client action, generated from the management tree.
use std::{path::PathBuf, process::Command};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "deep-obsidian-completions-{}",
            deep_obsidian_server::auth::generate_token()
        ));
        std::fs::create_dir_all(path.join("deep-obsidian-mcp")).unwrap();
        std::fs::write(
            path.join("deep-obsidian-mcp/client.json"),
            "invalid profile",
        )
        .unwrap();
        Self(path)
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_deep-obsidian-cli"))
            .args(args)
            .env("XDG_CONFIG_HOME", &self.0)
            .env("PATH", "")
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn all_shells_generate_offline_with_client_and_nested_commands() {
    let f = Fixture::new();
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let output = f.run(&["--config", "/missing/config.json", "completions", shell]);
        assert!(output.status.success(), "{shell}: {output:?}");
        assert!(output.stderr.is_empty(), "{shell}: {output:?}");
        let script = String::from_utf8(output.stdout).unwrap();
        for expected in [
            "deep-obsidian-cli",
            "link",
            "unlink",
            "context",
            "local-config",
            "database",
            "role",
            "completions",
        ] {
            assert!(script.contains(expected), "{shell}: missing {expected}");
        }
    }
    assert_eq!(
        std::fs::read_to_string(f.0.join("deep-obsidian-mcp/client.json")).unwrap(),
        "invalid profile"
    );
}

#[test]
fn invalid_shells_and_extra_arguments_fail_without_printing_a_script() {
    let f = Fixture::new();
    for args in [
        vec!["completions"],
        vec!["completions", "unknown"],
        vec!["completions", "bash", "extra"],
        vec!["completions", "bash", "--bogus"],
    ] {
        let output = f.run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
    }
    let help = f.run(&["help", "completions"]);
    assert!(help.status.success(), "{help:?}");
    assert!(String::from_utf8_lossy(&help.stdout).contains("powershell"));
}

#[cfg(unix)]
#[test]
fn bash_completes_root_nested_flags_and_enum_values() {
    let f = Fixture::new();
    let generated = f.run(&["completions", "bash"]);
    assert!(generated.status.success());
    let file = f.0.join("completion.bash");
    std::fs::write(&file, generated.stdout).unwrap();
    for (words, expected) in [
        (vec!["deep-obsidian-cli", "li"], vec!["link"]),
        (
            vec!["deep-obsidian-cli", "mounts", "add", "co"],
            vec!["couchdb"],
        ),
        (
            vec!["deep-obsidian-cli", "link", "--con"],
            vec!["--container", "--context"],
        ),
        (
            vec!["deep-obsidian-cli", "users", "grant", "--role", ""],
            vec!["reader", "editor", "owner"],
        ),
        (
            vec!["deep-obsidian-cli", "completions", ""],
            vec!["bash", "zsh", "fish", "powershell", "elvish"],
        ),
    ] {
        let output = Command::new("bash")
            .args([
                "--noprofile",
                "--norc",
                "-c",
                r#"
source "$1"
shift
COMP_WORDS=("$@")
COMP_CWORD=$((${#COMP_WORDS[@]} - 1))
[[ $(complete -p deep-obsidian-cli) =~ -F\ ([^\ ]+) ]] || exit 1
"${BASH_REMATCH[1]}" "${COMP_WORDS[0]}" "${COMP_WORDS[COMP_CWORD]}" "${COMP_WORDS[COMP_CWORD-1]}"
printf '%s\n' "${COMPREPLY[@]}"
"#,
                "completion-test",
            ])
            .arg(&file)
            .args(&words)
            .output()
            .expect("Bash is required for the shell completion regression");
        assert!(output.status.success(), "{words:?}: {output:?}");
        let matches = String::from_utf8(output.stdout).unwrap();
        for value in expected {
            assert!(
                matches.lines().any(|line| line == value),
                "{words:?}: missing {value}: {matches}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn fish_completes_deep_options_and_fixed_values() {
    if Command::new("fish").arg("--version").output().is_err() {
        eprintln!("Fish unavailable locally; the CI test job installs it");
        return;
    }
    let f = Fixture::new();
    let generated = f.run(&["completions", "fish"]);
    assert!(generated.status.success());
    let file = f.0.join("completion.fish");
    std::fs::write(&file, generated.stdout).unwrap();
    for (line, expected) in [
        ("deep-obsidian-cli li", "link"),
        ("deep-obsidian-cli mounts add couchdb --data", "--database"),
        (
            "deep-obsidian-cli users grant alice --mount team --role ed",
            "editor",
        ),
        ("deep-obsidian-cli completions zs", "zsh"),
    ] {
        let output = Command::new("fish")
            .args(["--no-config", "-c", "source $argv[1]; complete -C $argv[2]"])
            .arg(&file)
            .arg(line)
            .output()
            .unwrap();
        assert!(output.status.success(), "{line}: {output:?}");
        let matches = String::from_utf8(output.stdout).unwrap();
        assert!(
            matches
                .lines()
                .any(|line| line.split('\t').next() == Some(expected)),
            "{line}: {matches}"
        );
    }
}

#[cfg(unix)]
#[test]
fn zsh_completion_loads_and_registers() {
    if Command::new("zsh").arg("--version").output().is_err() {
        eprintln!("Zsh unavailable locally; the CI test job installs it");
        return;
    }
    let f = Fixture::new();
    let generated = f.run(&["completions", "zsh"]);
    assert!(generated.status.success());
    let file = f.0.join("_deep-obsidian-cli");
    std::fs::write(&file, generated.stdout).unwrap();
    let output = Command::new("zsh")
        .args(["-f", "-c", r#"autoload -Uz compinit; compinit -i -D; source "$1"; [[ ${_comps[deep-obsidian-cli]} == _deep-obsidian-cli ]]"#, "completion-test"])
        .arg(file).output().unwrap();
    assert!(output.status.success(), "{output:?}");
}
