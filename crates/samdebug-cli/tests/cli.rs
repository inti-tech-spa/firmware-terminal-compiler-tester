use std::process::Command;

#[test]
fn version_json_is_one_clean_document() {
    let output = Command::new(env!("CARGO_BIN_EXE_samdebug"))
        .args(["version", "--output", "json"])
        .output()
        .expect("run samdebug");
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).expect("utf8");
    assert_eq!(text.lines().count(), 1);
    let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["ok"], true);
    assert_eq!(value["command"], "version");
}

#[test]
fn clap_help_and_version_are_available() {
    let help = Command::new(env!("CARGO_BIN_EXE_samdebug"))
        .arg("--help")
        .output()
        .expect("run help");
    assert!(help.status.success());
    let help_text = String::from_utf8(help.stdout).expect("utf8 help");
    for command in ["setup", "doctor", "init", "build", "probe", "debug"] {
        assert!(help_text.contains(command), "help omits {command}");
    }

    let version = Command::new(env!("CARGO_BIN_EXE_samdebug"))
        .arg("--version")
        .output()
        .expect("run version");
    assert!(version.status.success());
    assert!(
        String::from_utf8(version.stdout)
            .expect("utf8 version")
            .starts_with("samdebug ")
    );
}

#[test]
fn reserved_command_has_stable_json_error_and_exit_code() {
    let output = Command::new(env!("CARGO_BIN_EXE_samdebug"))
        .args(["doctor", "--output", "json"])
        .output()
        .expect("run samdebug");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "NOT_IMPLEMENTED");
    assert_eq!(value["error"]["exit_code"], 2);
}

#[test]
fn authorization_is_probe_and_operation_scoped() {
    let rejected = Command::new(env!("CARGO_BIN_EXE_samdebug"))
        .args([
            "flash",
            "--probe",
            "ABC",
            "--confirm",
            "erase:ABC",
            "--output",
            "json",
        ])
        .output()
        .expect("run rejected flash");
    assert_eq!(rejected.status.code(), Some(8));

    let authorized_but_unimplemented = Command::new(env!("CARGO_BIN_EXE_samdebug"))
        .args([
            "flash",
            "--probe",
            "ABC",
            "--confirm",
            "flash:ABC",
            "--output",
            "json",
        ])
        .output()
        .expect("run accepted flash");
    assert_eq!(authorized_but_unimplemented.status.code(), Some(6));
    let value: serde_json::Value =
        serde_json::from_slice(&authorized_but_unimplemented.stdout).expect("valid json");
    assert_eq!(value["error"]["code"], "NOT_IMPLEMENTED");
}

#[test]
fn invalid_command_in_json_mode_is_structured_and_stdout_clean() {
    let output = Command::new(env!("CARGO_BIN_EXE_samdebug"))
        .args(["not-a-command", "--output", "json"])
        .output()
        .expect("run invalid command");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    assert_eq!(value["error"]["code"], "INVALID_COMMAND");
}
