use std::{
    process::{Command, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;

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
#[cfg(unix)]
fn doctor_uses_and_validates_explicit_system_tools() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("tempdir");
    let definitions = [
        ("gcc", "arm-none-eabi-gcc"),
        ("gdb", "GNU gdb"),
        ("openocd", "Open On-Chip Debugger"),
        ("objcopy", "GNU objcopy"),
        ("objdump", "GNU objdump"),
        ("size", "GNU size"),
    ];
    for (name, output) in definitions {
        let path = temp.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\necho '{output}'\n")).expect("write tool");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make tool executable");
    }
    let tool = |name: &str| temp.path().join(name).to_string_lossy().into_owned();
    let config = format!(
        r#"schema_version = 1
[project]
kind = "microchip-studio-cproj"
path = "firmware.cproj"
configuration = "Debug"
device = "ATSAM4SD32C"
[tools]
channel = "system"
[tools.system]
gcc = {gcc:?}
gdb = {gdb:?}
openocd = {openocd:?}
objcopy = {objcopy:?}
objdump = {objdump:?}
size = {size:?}
[probe]
kind = "atmel-ice"
transport = "swd"
"#,
        gcc = tool("gcc"),
        gdb = tool("gdb"),
        openocd = tool("openocd"),
        objcopy = tool("objcopy"),
        objdump = tool("objdump"),
        size = tool("size"),
    );
    std::fs::write(temp.path().join("samdebug.toml"), config).expect("write config");

    let output = Command::new(env!("CARGO_BIN_EXE_samdebug"))
        .args(["doctor", "--output=json"])
        .current_dir(temp.path())
        .output()
        .expect("run system doctor");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(value["data"]["tool_channel"], "system");
    assert_eq!(value["data"]["tools"][0]["install"], "verified");
    assert_eq!(
        value["data"]["tools"][0]["executables"]
            .as_array()
            .expect("executables")
            .len(),
        6
    );
    assert!(
        value["data"]["guidance"][0]
            .as_str()
            .expect("guidance")
            .contains("not reproducible")
    );
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

#[test]
fn sigint_cancels_setup_and_removes_partial_download() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("samdebug-m2-cancel-{nonce}"));
    let ready_file = std::env::temp_dir().join(format!("samdebug-m2-ready-{nonce}"));
    let process = Command::new(env!("CARGO_BIN_EXE_samdebug"))
        .args(["__test-setup-cancel", "--output=json"])
        .env("SAMDEBUG_TEST_SETUP_ROOT", &root)
        .env("SAMDEBUG_TEST_SETUP_READY_FILE", &ready_file)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cancellable setup");

    let ready = (0..300).any(|_| {
        if ready_file.is_file() {
            true
        } else {
            thread::sleep(Duration::from_millis(10));
            false
        }
    });
    assert!(ready, "setup reached downloader");
    let signal = Command::new("/bin/kill")
        .args(["-INT", &process.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(signal.success());

    let output = process.wait_with_output().expect("wait for setup");
    assert_eq!(output.status.code(), Some(130));
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(value["error"]["code"], "INTERRUPTED");
    let partials = std::fs::read_dir(root.join("downloads"))
        .expect("downloads directory")
        .collect::<Result<Vec<_>, _>>()
        .expect("scan downloads");
    assert!(partials.is_empty(), "partial download was not removed");
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_file(ready_file);
}
