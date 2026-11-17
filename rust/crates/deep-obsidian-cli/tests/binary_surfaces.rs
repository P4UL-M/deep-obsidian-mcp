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
