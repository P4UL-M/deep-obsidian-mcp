//! Help is local, validates command paths and shows only relevant options.
use std::{
    path::PathBuf,
    process::{Command, Output},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "deep-obsidian-help-{}",
            deep_obsidian_server::auth::generate_token()
        ));
        std::fs::create_dir_all(path.join("deep-obsidian-mcp")).unwrap();
        // Any attempt to consult this link would fail. Help must not read it,
        // invoke Docker, or resolve the explicitly missing configuration.
        std::fs::write(
            path.join("deep-obsidian-mcp/client.json"),
            "invalid profile",
        )
        .unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_deep-obsidian-cli"))
            .args(args)
            .env("XDG_CONFIG_HOME", &self.0)
            .env("PATH", "")
            .output()
            .unwrap()
    }
    fn help(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn overview_fits_a_page_and_points_to_specific_help() {
    let f = Fixture::new();
    let root = f.help(&["--help"]);
    assert_eq!(root, f.help(&["-h"]));
    assert_eq!(root, f.help(&["help"]));
    assert_eq!(root, f.help(&[]));
    assert!(root.lines().count() <= 45, "{root}");
    assert!(
        root.lines().all(|line| line.chars().count() <= 80),
        "{root}"
    );
    assert!(root.contains("<command> --help"), "{root}");
    for command in [
        "setup-service",
        "link",
        "mounts",
        "users",
        "secrets",
        "doctor",
    ] {
        assert!(root.contains(command), "{root}");
    }
    assert!(
        !root.contains("--embedding-model") && !root.contains("--probe-remote"),
        "{root}"
    );
    assert!(!root.contains("  serve"), "{root}");
}

#[test]
fn each_family_and_leaf_gets_its_own_help_offline() {
    let f = Fixture::new();
    for (args, usage, option) in [
        (vec!["doctor", "--help"], "doctor", "--probe-remote"),
        (vec!["probe", "--help"], "probe", "--timeout-ms"),
        (vec!["setup-service", "--help"], "setup-service", "--wizard"),
        (vec!["link", "--help"], "link", "--context"),
        (vec!["unlink", "--help"], "unlink", "--help"),
        (
            vec!["mounts", "add", "couchdb", "--help"],
            "mounts add couchdb",
            "--database",
        ),
        (vec!["secrets", "set", "--help"], "secrets set", "--stdin"),
        (vec!["users", "grant", "--help"], "users grant", "--role"),
        (
            vec!["couchdb", "restore", "--help"],
            "couchdb restore",
            "--from",
        ),
        (
            vec!["algolia", "status", "--mount", "mounts", "--help"],
            "algolia status",
            "--mount",
        ),
        (
            vec!["--config", "/missing/config.json", "doctor", "--help"],
            "doctor",
            "--probe-remote",
        ),
    ] {
        let text = f.help(&args);
        assert!(
            text.contains(&format!("Usage: deep-obsidian-cli {usage}")),
            "{text}"
        );
        assert!(text.contains(option), "{text}");
        assert!(!text.contains("Start here:"), "{text}");
    }
    assert_eq!(
        f.help(&["help", "users", "grant"]),
        f.help(&["users", "grant", "--help"])
    );
    let doctor = f.help(&["doctor", "--help"]);
    assert!(
        !doctor.contains("--embedding-model") && !doctor.contains("--dry-run"),
        "{doctor}"
    );
    assert_eq!(
        std::fs::read_to_string(f.0.join("deep-obsidian-mcp/client.json")).unwrap(),
        "invalid profile"
    );
}

#[test]
fn typos_remain_errors_even_when_help_comes_first() {
    let f = Fixture::new();
    for args in [
        vec!["doctro", "--help"],
        vec!["--help", "doctro"],
        vec!["users", "enabel", "--help"],
        vec!["users", "--help", "enabel"],
        vec!["help", "mounts", "ad"],
        vec!["doctor", "--probe-remtoe", "--help"],
        vec!["mounts", "add", "couchd", "--help"],
    ] {
        let output = f.run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(!output.stderr.is_empty(), "{args:?}");
        assert!(
            output.stdout.is_empty(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
