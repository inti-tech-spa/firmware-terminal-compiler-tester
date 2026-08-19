use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use samdebug_core::{ErrorCategory, SamdebugError, SamdebugResult};

use crate::{DebuggerTransport, MiCommandOutput, MiRecord, MiStreamParser};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GdbMiConfig {
    pub executable: PathBuf,
    pub current_directory: Option<PathBuf>,
}

impl GdbMiConfig {
    #[must_use]
    pub const fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            current_directory: None,
        }
    }
}

#[derive(Debug)]
pub struct GdbMiProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    records: Receiver<SamdebugResult<MiRecord>>,
    readers: Vec<JoinHandle<()>>,
    token: u64,
    stopped: bool,
}

impl GdbMiProcess {
    pub fn launch(config: &GdbMiConfig) -> SamdebugResult<Self> {
        validate_config(config)?;
        let mut command = Command::new(&config.executable);
        command
            .args(["--interpreter=mi2", "--nx", "--quiet"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(directory) = &config.current_directory {
            command.current_dir(directory);
        }
        configure_process_group(&mut command);
        let mut child = command.spawn().map_err(|error| {
            debug_error("GDB_START_FAILED", format!("failed to start GDB: {error}"))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| debug_error("GDB_START_FAILED", "GDB standard input was not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| debug_error("GDB_START_FAILED", "GDB standard output was not piped"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| debug_error("GDB_START_FAILED", "GDB standard error was not piped"))?;
        let (sender, records) = mpsc::channel();
        let stdout_sender = sender.clone();
        let stdout_reader = thread::spawn(move || read_stdout(stdout, &stdout_sender));
        let stderr_reader = thread::spawn(move || read_stderr(stderr, &sender));
        Ok(Self {
            child,
            stdin: Some(stdin),
            records,
            readers: vec![stdout_reader, stderr_reader],
            token: 0,
            stopped: false,
        })
    }

    fn write_command(&mut self, token: u64, command: &str) -> SamdebugResult<()> {
        if command.contains(['\n', '\r']) {
            return Err(debug_error(
                "GDB_COMMAND_INVALID",
                "GDB/MI command contains a line break",
            ));
        }
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| debug_error("GDB_EXITED", "GDB input is closed"))?;
        writeln!(stdin, "{token}{command}")
            .and_then(|()| stdin.flush())
            .map_err(|error| debug_error("GDB_WRITE_FAILED", error.to_string()))
    }

    fn terminate_and_reap(&mut self) -> SamdebugResult<()> {
        if self.stopped {
            return Ok(());
        }
        self.stdin.take();
        if wait_child(&mut self.child, Duration::from_millis(500))? {
            self.finish_readers();
            self.stopped = true;
            return Ok(());
        }
        terminate_process_group(self.child.id());
        if !wait_child(&mut self.child, Duration::from_secs(1))? {
            kill_process_group(self.child.id());
            self.child
                .kill()
                .map_err(|error| debug_error("GDB_KILL_FAILED", error.to_string()))?;
            self.child
                .wait()
                .map_err(|error| debug_error("GDB_WAIT_FAILED", error.to_string()))?;
        }
        self.finish_readers();
        self.stopped = true;
        Ok(())
    }

    fn finish_readers(&mut self) {
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

impl DebuggerTransport for GdbMiProcess {
    fn command(
        &mut self,
        command: &str,
        wait_for_stop: bool,
        timeout: Duration,
    ) -> SamdebugResult<MiCommandOutput> {
        if self.stopped {
            return Err(debug_error("GDB_EXITED", "GDB has exited"));
        }
        self.token = self
            .token
            .checked_add(1)
            .ok_or_else(|| debug_error("GDB_TOKEN_EXHAUSTED", "GDB/MI command token exhausted"))?;
        let token = self.token;
        self.write_command(token, command)?;
        let deadline = Instant::now() + timeout;
        let mut records = Vec::new();
        let mut result = None;
        let mut stopped = !wait_for_stop;
        while result.is_none() || !stopped {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(debug_error(
                    "GDB_COMMAND_TIMEOUT",
                    format!("GDB/MI command timed out: {command}"),
                ));
            }
            let record = match self.records.recv_timeout(remaining) {
                Ok(record) => record?,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(debug_error(
                        "GDB_COMMAND_TIMEOUT",
                        format!("GDB/MI command timed out: {command}"),
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(debug_error("GDB_EXITED", "GDB output closed"));
                }
            };
            if let MiRecord::Result {
                token: Some(record_token),
                class,
                results,
            } = &record
                && *record_token == token
            {
                result = Some((class.clone(), results.clone()));
            }
            if matches!(&record, MiRecord::Exec { class, .. } if class == "stopped") {
                stopped = true;
            }
            records.push(record);
        }
        let (result_class, results) = result.expect("loop requires command result");
        Ok(MiCommandOutput {
            result_class,
            results,
            records,
        })
    }

    fn shutdown(&mut self) -> SamdebugResult<()> {
        if self.stopped {
            return Ok(());
        }
        self.token = self.token.saturating_add(1);
        let token = self.token;
        let _ = self.write_command(token, "-gdb-exit");
        self.terminate_and_reap()
    }

    fn wait_for_stop(&mut self, timeout: Duration) -> SamdebugResult<Vec<MiRecord>> {
        let deadline = Instant::now() + timeout;
        let mut records = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(debug_error(
                    "GDB_STOP_TIMEOUT",
                    "target did not stop within the bounded timeout",
                ));
            }
            let record = match self.records.recv_timeout(remaining) {
                Ok(record) => record?,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(debug_error(
                        "GDB_STOP_TIMEOUT",
                        "target did not stop within the bounded timeout",
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(debug_error("GDB_EXITED", "GDB output closed"));
                }
            };
            let stopped = matches!(&record, MiRecord::Exec { class, .. } if class == "stopped");
            records.push(record);
            if stopped {
                return Ok(records);
            }
        }
    }
}

impl Drop for GdbMiProcess {
    fn drop(&mut self) {
        let _ = self.terminate_and_reap();
    }
}

fn validate_config(config: &GdbMiConfig) -> SamdebugResult<()> {
    let metadata = fs::symlink_metadata(&config.executable).map_err(|error| {
        debug_error(
            "GDB_CONFIGURATION_INVALID",
            format!("{}: {error}", config.executable.display()),
        )
    })?;
    if !config.executable.is_absolute() || !metadata.is_file() || metadata.file_type().is_symlink()
    {
        return Err(debug_error(
            "GDB_CONFIGURATION_INVALID",
            "GDB executable must be an absolute regular non-symlink file",
        ));
    }
    if let Some(directory) = &config.current_directory {
        validate_directory(directory)?;
    }
    Ok(())
}

fn validate_directory(directory: &Path) -> SamdebugResult<()> {
    if directory.is_absolute() && directory.is_dir() {
        Ok(())
    } else {
        Err(debug_error(
            "GDB_CONFIGURATION_INVALID",
            "GDB current directory must be an absolute directory",
        ))
    }
}

fn read_stdout(mut stdout: impl Read, sender: &Sender<SamdebugResult<MiRecord>>) {
    let mut parser = MiStreamParser::new();
    let mut buffer = [0_u8; 4096];
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) => {
                if let Err(error) = parser.finish() {
                    let _ = sender.send(Err(error));
                }
                break;
            }
            Ok(count) => match parser.push(&buffer[..count]) {
                Ok(records) => {
                    for record in records {
                        if sender.send(Ok(record)).is_err() {
                            return;
                        }
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            },
            Err(error) => {
                let _ = sender.send(Err(debug_error("GDB_READ_FAILED", error.to_string())));
                return;
            }
        }
    }
}

fn read_stderr(stderr: impl Read, sender: &Sender<SamdebugResult<MiRecord>>) {
    for line in BufReader::new(stderr).lines() {
        match line {
            Ok(line) => {
                if sender.send(Ok(MiRecord::Log(line))).is_err() {
                    return;
                }
            }
            Err(error) => {
                let _ = sender.send(Err(debug_error("GDB_READ_FAILED", error.to_string())));
                return;
            }
        }
    }
}

fn wait_child(child: &mut Child, timeout: Duration) -> SamdebugResult<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if child
            .try_wait()
            .map_err(|error| debug_error("GDB_WAIT_FAILED", error.to_string()))?
            .is_some()
        {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn terminate_process_group(pid: u32) {
    signal_group(pid, "-TERM");
}

#[cfg(not(unix))]
fn terminate_process_group(_pid: u32) {}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    signal_group(pid, "-KILL");
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

#[cfg(unix)]
fn signal_group(pid: u32, signal: &str) {
    let _ = Command::new("/bin/kill")
        .args([signal, &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn debug_error(code: &str, message: impl Into<String>) -> SamdebugError {
    SamdebugError::new(ErrorCategory::Debugger, code, message)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    #[cfg(unix)]
    fn launches_mi2_without_init_files_parses_fragments_and_reaps() {
        let temp = tempfile::tempdir().expect("tempdir");
        let script = temp.path().join("fake-gdb");
        fs::write(
            &script,
            "#!/bin/sh\n[ \"$1\" = '--interpreter=mi2' ] || exit 90\n[ \"$2\" = '--nx' ] || exit 91\n[ \"$3\" = '--quiet' ] || exit 92\nprintf '(gdb)\\n'\nwhile IFS= read -r line; do\n token=${line%%-*}\n case \"$line\" in\n *-gdb-exit) printf '%s^exit\\n' \"$token\"; exit 0 ;;\n *) printf '%s' \"$token\"; printf '^do'; printf 'ne\\n*stopped,reason=\"end-stepping-range\"\\n' ;;\n esac\ndone\n",
        )
        .expect("script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("executable");
        let mut process = GdbMiProcess::launch(&GdbMiConfig::new(script)).expect("launch");
        assert_eq!(
            process
                .command("-exec-step\n-gdb-exit", true, Duration::from_secs(2))
                .unwrap_err()
                .code(),
            "GDB_COMMAND_INVALID"
        );
        let output = process
            .command("-exec-step", true, Duration::from_secs(2))
            .expect("command");
        assert_eq!(output.result_class, "done");
        assert!(
            output
                .records
                .iter()
                .any(|record| matches!(record, MiRecord::Exec { class, .. } if class == "stopped"))
        );
        process.shutdown().expect("shutdown");
        assert!(process.stopped);
    }

    #[test]
    fn rejects_relative_symlink_and_newline_commands() {
        assert_eq!(
            GdbMiProcess::launch(&GdbMiConfig::new(PathBuf::from("gdb")))
                .unwrap_err()
                .code(),
            "GDB_CONFIGURATION_INVALID"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let temp = tempfile::tempdir().expect("tempdir");
            let target = temp.path().join("gdb-target");
            let link = temp.path().join("gdb-link");
            fs::write(&target, b"tool").expect("target");
            symlink(&target, &link).expect("symlink");
            assert_eq!(
                GdbMiProcess::launch(&GdbMiConfig::new(link))
                    .unwrap_err()
                    .code(),
                "GDB_CONFIGURATION_INVALID"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn classifies_gdb_crash_and_timeout_then_reaps_process_group() {
        let temp = tempfile::tempdir().expect("tempdir");
        let crashing = temp.path().join("crashing-gdb");
        fs::write(
            &crashing,
            "#!/bin/sh\nprintf '(gdb)\\n'\nIFS= read -r line\nexit 17\n",
        )
        .expect("crash script");
        fs::set_permissions(&crashing, fs::Permissions::from_mode(0o755)).expect("executable");
        let mut process = GdbMiProcess::launch(&GdbMiConfig::new(crashing)).expect("launch");
        assert_eq!(
            process
                .command("-exec-step", true, Duration::from_secs(2))
                .unwrap_err()
                .code(),
            "GDB_EXITED"
        );
        process.shutdown().expect("reap crash");

        let hanging = temp.path().join("hanging-gdb");
        fs::write(
            &hanging,
            "#!/bin/sh\nprintf '(gdb)\\n'\nIFS= read -r line\n/bin/sleep 30 &\nwait\n",
        )
        .expect("hang script");
        fs::set_permissions(&hanging, fs::Permissions::from_mode(0o755)).expect("executable");
        let mut process = GdbMiProcess::launch(&GdbMiConfig::new(hanging)).expect("launch");
        let pid = process.child.id();
        assert_eq!(
            process
                .command("-exec-step", true, Duration::from_millis(50))
                .unwrap_err()
                .code(),
            "GDB_COMMAND_TIMEOUT"
        );
        process.shutdown().expect("kill timeout group");
        assert!(process.stopped);
        assert!(
            !Command::new("/bin/kill")
                .args(["-0", &pid.to_string()])
                .stderr(Stdio::null())
                .status()
                .expect("probe pid")
                .success()
        );
    }
}
