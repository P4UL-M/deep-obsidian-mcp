use std::process::Command;

#[test]
fn server_rejects_management_commands_and_advertises_only_server_options() {
    let help = Command::new(env!("CARGO_BIN_EXE_deep-obsidian-mcp"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("Deep Obsidian MCP server"), "{help}");
    assert!(!help.contains("setup-service"), "{help}");
    for command in [
        "setup-service",
        "mounts",
        "secrets",
        "doctor",
        "users",
        "link",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_deep-obsidian-mcp"))
            .arg(command)
            .output()
            .unwrap();
        assert!(!output.status.success(), "server accepted {command}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("deep-obsidian-cli"), "{command}: {error}");
    }
}

#[test]
fn management_defaults_to_help_and_does_not_start_a_server() {
    let output = Command::new(env!("CARGO_BIN_EXE_deep-obsidian-cli"))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("setup-service"));
    let output = Command::new(env!("CARGO_BIN_EXE_deep-obsidian-cli"))
        .arg("serve")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("deep-obsidian-mcp"));
}

#[cfg(unix)]
#[test]
fn masked_terminal_input_and_per_mount_secret_regressions() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|path| path.join("tests/test_cli_password_prompt.py").is_file())
        .unwrap();
    let prompt = Command::new("python3")
        .arg(root.join("tests/test_cli_password_prompt.py"))
        .arg(env!("CARGO_BIN_EXE_deep-obsidian-cli"))
        .current_dir(root)
        .output()
        .expect("Python 3 is required for the terminal regression");
    assert!(
        prompt.status.success(),
        "{}{}",
        String::from_utf8_lossy(&prompt.stdout),
        String::from_utf8_lossy(&prompt.stderr)
    );
    let secrets = Command::new("node")
        .args(["--test", "tests/mount-secrets.test.mjs"])
        .current_dir(root)
        .output()
        .expect("Node is required for the Docker secret regression");
    assert!(
        secrets.status.success(),
        "{}{}",
        String::from_utf8_lossy(&secrets.stdout),
        String::from_utf8_lossy(&secrets.stderr)
    );
}
