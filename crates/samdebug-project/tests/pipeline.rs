use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use samdebug_core::{
    CancellationToken, Configuration, SamdebugResult,
    ports::{CommandOutput, CommandSpec, ManagedChild, ProcessRunner},
};
use samdebug_project::{BuildToolPaths, artifacts, build, clean};
use tempfile::TempDir;

#[derive(Debug)]
struct FakeRunner {
    calls: Mutex<Vec<CommandSpec>>,
    compile_failure: bool,
    link_failure: bool,
    size: &'static str,
    entry: &'static str,
}

impl FakeRunner {
    fn successful() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            compile_failure: false,
            link_failure: false,
            size: "text data bss dec hex filename\n1000 20 30 1050 41a firmware.elf\n",
            entry: "start address 0x00400101\n",
        }
    }
}

impl ProcessRunner for FakeRunner {
    fn run(&self, command: &CommandSpec) -> SamdebugResult<CommandOutput> {
        self.calls.lock().expect("calls").push(command.clone());
        let name = Path::new(&command.program)
            .file_name()
            .and_then(|name| name.to_str())
            .expect("tool name");
        if name == "gcc" && command.args.contains(&"-c".into()) {
            if self.compile_failure {
                return Ok(output(1, b"controlled compile failure"));
            }
            let object = argument_after(&command.args, "-o");
            let dependency = argument_after(&command.args, "-MF");
            let source = command.args[command
                .args
                .iter()
                .position(|argument| argument == "-c")
                .expect("compile marker")
                + 1]
            .clone();
            fs::write(&object, b"object").expect("fake object");
            fs::write(&dependency, format!("{object}: {source}\n")).expect("fake dependency");
        } else if name == "gcc" {
            if self.link_failure {
                return Ok(output(1, b"controlled link failure"));
            }
            let elf = argument_after(&command.args, "-o");
            fs::write(elf, b"elf").expect("fake elf");
            let map = command
                .args
                .iter()
                .find_map(|argument| argument.strip_prefix("-Wl,-Map,"))
                .expect("map argument");
            fs::write(map, b"map").expect("fake map");
        } else if name == "objcopy" {
            fs::write(command.args.last().expect("artifact output"), b"artifact")
                .expect("fake artifact");
        } else if name == "size" {
            return Ok(output(0, self.size.as_bytes()));
        } else if name == "objdump" && command.args.first().is_some_and(|arg| arg == "-f") {
            return Ok(output(0, self.entry.as_bytes()));
        } else if name == "objdump" {
            return Ok(output(0, b"disassembly"));
        }
        Ok(output(0, b""))
    }

    fn spawn(&self, _command: &CommandSpec) -> SamdebugResult<Box<dyn ManagedChild>> {
        unreachable!("build uses finite processes")
    }
}

fn output(code: i32, stdout: &[u8]) -> CommandOutput {
    CommandOutput {
        exit_code: Some(code),
        stdout: stdout.to_vec(),
        stderr: stdout.to_vec(),
    }
}

fn argument_after(arguments: &[String], marker: &str) -> String {
    arguments[arguments
        .iter()
        .position(|argument| argument == marker)
        .expect("argument marker")
        + 1]
    .clone()
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn copy_fixture() -> TempDir {
    let temp = tempfile::tempdir().expect("temporary project");
    copy_tree(&fixture(), temp.path());
    temp
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("create directory");
    for entry in fs::read_dir(source).expect("read fixture") {
        let entry = entry.expect("fixture entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy file");
        }
    }
}

fn tools(temp: &Path) -> BuildToolPaths {
    let path = |name: &str| {
        let path = temp.join(name);
        fs::write(&path, b"tool").expect("fake tool");
        path.to_string_lossy().into_owned()
    };
    BuildToolPaths {
        gcc: path("gcc"),
        objcopy: path("objcopy"),
        objdump: path("objdump"),
        size: path("size"),
    }
}

#[test]
fn builds_incrementally_with_argv_and_manages_artifacts() {
    let temp = copy_fixture();
    let project = temp.path().join("valid");
    let tool_paths = tools(temp.path());
    let runner = FakeRunner::successful();
    let token = CancellationToken::new();
    let first = build(
        &project.join("project.cproj"),
        Configuration::Debug,
        &tool_paths,
        &runner,
        &token,
    )
    .expect("clean build");
    assert_eq!(first.compiled, 4);
    assert_eq!(first.reused, 0);
    assert_eq!(first.memory.flash_bytes, 1_020);
    assert!(
        first
            .artifacts
            .iter()
            .any(|path| has_extension(path, "elf"))
    );
    assert!(
        first
            .artifacts
            .iter()
            .any(|path| has_extension(path, "lss"))
    );

    let second = build(
        &project.join("project.cproj"),
        Configuration::Debug,
        &tool_paths,
        &runner,
        &token,
    )
    .expect("incremental build");
    assert_eq!(second.compiled, 0);
    assert_eq!(second.reused, 4);

    let calls = runner.calls.lock().expect("calls");
    let compile = calls
        .iter()
        .find(|call| call.args.contains(&"-c".into()))
        .expect("compile command");
    for required in [
        "-mcpu=cortex-m4",
        "-mthumb",
        "-mfloat-abi=soft",
        "-D__SAM4SD32C__",
    ] {
        assert!(compile.args.contains(&required.into()));
    }
    assert!(compile.args.contains(&"-DSPACE_VALUE=hello world".into()));
    assert!(
        !compile
            .args
            .iter()
            .any(|argument| argument == "sh" || argument == "-c;")
    );
    drop(calls);

    let listed = artifacts(&project, Configuration::Debug).expect("list artifacts");
    assert!(
        listed
            .artifacts
            .iter()
            .any(|artifact| has_extension(&artifact.path, "hex"))
    );
    let cleaned = clean(&project, Configuration::Debug).expect("clean artifacts");
    assert!(cleaned.removed);
    assert!(
        artifacts(&project, Configuration::Debug)
            .expect("empty listing")
            .artifacts
            .is_empty()
    );
}

fn has_extension(path: &str, expected: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}

#[test]
fn reports_controlled_compile_memory_and_entry_failures() {
    let temp = copy_fixture();
    let project = temp.path().join("valid");
    let tool_paths = tools(temp.path());
    let token = CancellationToken::new();
    let compile_error = FakeRunner {
        compile_failure: true,
        ..FakeRunner::successful()
    };
    assert_eq!(
        build(
            &project.join("project.cproj"),
            Configuration::Debug,
            &tool_paths,
            &compile_error,
            &token
        )
        .expect_err("compile failure")
        .code(),
        "COMPILE_FAILED"
    );

    let overflow = FakeRunner {
        size: "text data bss dec hex filename\n2097152 1 1 0 0 firmware.elf\n",
        ..FakeRunner::successful()
    };
    assert_eq!(
        build(
            &project.join("project.cproj"),
            Configuration::Debug,
            &tool_paths,
            &overflow,
            &token
        )
        .expect_err("memory overflow")
        .code(),
        "MEMORY_OVERFLOW"
    );

    let link_error = FakeRunner {
        link_failure: true,
        ..FakeRunner::successful()
    };
    assert_eq!(
        build(
            &project.join("project.cproj"),
            Configuration::Debug,
            &tool_paths,
            &link_error,
            &token
        )
        .expect_err("link failure")
        .code(),
        "LINK_FAILED"
    );

    let bad_entry = FakeRunner {
        entry: "start address 0x00000000\n",
        ..FakeRunner::successful()
    };
    assert_eq!(
        build(
            &project.join("project.cproj"),
            Configuration::Debug,
            &tool_paths,
            &bad_entry,
            &token
        )
        .expect_err("invalid entry")
        .code(),
        "INVALID_ENTRY_POINT"
    );
}

#[test]
#[cfg(unix)]
fn rejects_symlinked_build_ancestor_before_external_write() {
    use std::os::unix::fs::symlink;

    let temp = copy_fixture();
    let project = temp.path().join("valid");
    let external = tempfile::tempdir().expect("external directory");
    fs::create_dir(project.join(".samdebug")).expect("state directory");
    symlink(external.path(), project.join(".samdebug/build")).expect("build symlink");
    let runner = FakeRunner::successful();
    let error = build(
        &project.join("project.cproj"),
        Configuration::Debug,
        &tools(temp.path()),
        &runner,
        &CancellationToken::new(),
    )
    .expect_err("reject symlinked build ancestor");
    assert_eq!(error.code(), "UNSAFE_BUILD_DIRECTORY");
    assert!(!external.path().join("Debug").exists());
    assert!(runner.calls.lock().expect("calls").is_empty());
}
