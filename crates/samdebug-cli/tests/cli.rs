use std::{
    path::Path,
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
#[cfg(target_os = "macos")]
fn probe_list_has_stable_json_report() {
    let output = Command::new(env!("CARGO_BIN_EXE_samdebug"))
        .args(["probe", "list", "--output=json"])
        .output()
        .expect("list probes");
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(value["command"], "probe");
    assert!(value["data"]["probes"].is_array());
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
fn programming_commands_require_project_configuration() {
    let temp = TempDir::new().expect("tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_samdebug"))
        .args([
            "flash",
            "--probe",
            "ABC",
            "--confirm",
            "flash:ABC",
            "--output",
            "json",
        ])
        .current_dir(temp.path())
        .output()
        .expect("run flash without config");
    assert_eq!(output.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    assert_eq!(value["error"]["code"], "CONFIG_READ_FAILED");
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

#[test]
#[cfg(unix)]
fn sigint_cancels_active_build_tool_cleans_stage_and_returns_130() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("tempdir");
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../samdebug-project/tests/fixtures");
    copy_tree(&fixtures, temp.path());
    let project = temp.path().join("valid");
    let tools = temp.path().join("tools");
    std::fs::create_dir(&tools).expect("tools directory");
    let pid_file = temp.path().join("build-child.pid");
    for name in ["gcc", "gdb", "openocd", "objcopy", "objdump", "size"] {
        let path = tools.join(name);
        let body = if name == "gcc" {
            "#!/bin/sh\necho $$ > \"$SAMDEBUG_TEST_BUILD_CHILD_PID_FILE\"\nexec /bin/sleep 30\n"
        } else {
            "#!/bin/sh\nexit 0\n"
        };
        std::fs::write(&path, body).expect("write tool");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make tool executable");
    }
    let tool = |name: &str| tools.join(name).to_string_lossy().into_owned();
    let config = format!(
        r#"schema_version = 1
[project]
kind = "microchip-studio-cproj"
path = "project.cproj"
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
    std::fs::write(project.join("samdebug.toml"), config).expect("write config");

    let process = Command::new(env!("CARGO_BIN_EXE_samdebug"))
        .args(["build", "--output=json"])
        .current_dir(&project)
        .env("SAMDEBUG_TEST_BUILD_CHILD_PID_FILE", &pid_file)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn build");
    let tool_pid = (0..300)
        .find_map(|_| {
            let pid = std::fs::read_to_string(&pid_file).ok();
            if pid.is_none() {
                thread::sleep(Duration::from_millis(10));
            }
            pid
        })
        .expect("build tool started");
    assert!(
        Command::new("/bin/kill")
            .args(["-INT", &process.id().to_string()])
            .status()
            .expect("signal samdebug")
            .success()
    );
    let output = process.wait_with_output().expect("wait for build");
    assert_eq!(output.status.code(), Some(130));
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(value["error"]["code"], "INTERRUPTED");
    assert!(
        !Command::new("/bin/kill")
            .args(["-0", tool_pid.trim()])
            .stderr(Stdio::null())
            .status()
            .expect("probe build tool")
            .success(),
        "build tool survived cancellation"
    );
    let build_dir = project.join(".samdebug/build/Debug");
    let stale_stage = std::fs::read_dir(build_dir)
        .expect("build directory")
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().starts_with(".stage-"));
    assert!(
        !stale_stage,
        "private staging directory survived cancellation"
    );
}

fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("create fixture destination");
    for entry in std::fs::read_dir(source).expect("read fixture tree") {
        let entry = entry.expect("fixture entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("fixture type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy fixture file");
        }
    }
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires a connected Atmel-ICE/ATSAM4SD32C and explicit destructive confirmation"]
fn physical_sigint_cancels_openocd_and_releases_probe() {
    let project = std::env::var("SAMDEBUG_PHYSICAL_PROJECT").expect("physical project path");
    let serial = std::env::var("SAMDEBUG_PHYSICAL_PROBE_SERIAL").expect("probe serial");
    assert_eq!(
        std::env::var("SAMDEBUG_PHYSICAL_CONFIRM").expect("physical confirmation"),
        format!("erase:{serial}")
    );
    let process = Command::new(env!("CARGO_BIN_EXE_samdebug"))
        .args([
            "erase",
            "--probe",
            &serial,
            "--confirm",
            &format!("erase:{serial}"),
            "--output=json",
        ])
        .current_dir(&project)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start physical erase");
    let openocd_started = (0..300).any(|_| {
        if Command::new("/usr/bin/pgrep")
            .args(["-P", &process.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            true
        } else {
            thread::sleep(Duration::from_millis(10));
            false
        }
    });
    assert!(
        openocd_started,
        "OpenOCD child reached the cancellable stage"
    );
    assert!(
        Command::new("/bin/kill")
            .args(["-INT", &process.id().to_string()])
            .status()
            .expect("interrupt erase")
            .success()
    );
    let output = process
        .wait_with_output()
        .expect("wait for interrupted erase");
    assert_eq!(output.status.code(), Some(130));
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(value["error"]["code"], "INTERRUPTED");

    let doctor = Command::new(env!("CARGO_BIN_EXE_samdebug"))
        .args(["doctor", "--output=json"])
        .current_dir(project)
        .output()
        .expect("reacquire probe after cancellation");
    assert!(doctor.status.success());
    let value: serde_json::Value = serde_json::from_slice(&doctor.stdout).expect("doctor JSON");
    assert_eq!(value["data"]["probe"]["target_connectivity"], "connected");
}
