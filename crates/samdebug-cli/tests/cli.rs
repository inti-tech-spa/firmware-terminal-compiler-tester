use std::{
    process::{Command, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

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
fn json_equals_form_wraps_errors_help_and_version() {
    for args in [
        vec!["--output=json", "not-a-command"],
        vec!["--output=json", "--help"],
        vec!["--output=json", "--version"],
        vec!["--help", "--output=json"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_samdebug"))
            .args(args)
            .output()
            .expect("run machine form");
        assert!(output.stderr.is_empty());
        assert_eq!(String::from_utf8_lossy(&output.stdout).lines().count(), 1);
        let value: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("one JSON envelope");
        assert_eq!(value["schema_version"], 1);
    }
}

#[test]
fn doctor_has_stable_json_report() {
    let output = Command::new(env!("CARGO_BIN_EXE_samdebug"))
        .args(["doctor", "--output", "json"])
        .output()
        .expect("run samdebug");
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    assert_eq!(value["ok"], true);
    assert_eq!(value["command"], "doctor");
    assert!(value["data"]["tools"].is_array());
}

#[test]
fn setup_fails_closed_while_openocd_bundle_is_unpublished() {
    let output = Command::new(env!("CARGO_BIN_EXE_samdebug"))
        .args(["setup", "--output=json"])
        .output()
        .expect("run setup");
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    assert_eq!(value["error"]["code"], "TOOL_MANIFEST_DISABLED");
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

#[test]
fn sigint_cancels_work_reaps_child_and_returns_130() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let pid_file = std::env::temp_dir().join(format!("samdebug-m1-{nonce}.pid"));
    let process = Command::new(env!("CARGO_BIN_EXE_samdebug"))
        .args(["__test-block", "--output=json"])
        .env("SAMDEBUG_TEST_CHILD_PID_FILE", &pid_file)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn blocking samdebug command");

    let child_pid = (0..200)
        .find_map(|_| {
            let pid = std::fs::read_to_string(&pid_file).ok();
            if pid.is_none() {
                thread::sleep(Duration::from_millis(10));
            }
            pid
        })
        .expect("child pid file created");
    let signal = Command::new("/bin/kill")
        .args(["-INT", &process.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(signal.success());

    let output = process.wait_with_output().expect("wait for samdebug");
    assert_eq!(output.status.code(), Some(130));
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(value["error"]["code"], "INTERRUPTED");

    let child_is_gone = !Command::new("/bin/kill")
        .args(["-0", child_pid.trim()])
        .stderr(Stdio::null())
        .status()
        .expect("probe child pid")
        .success();
    assert!(child_is_gone, "managed child was not reaped");
    let _ = std::fs::remove_file(pid_file);
}
