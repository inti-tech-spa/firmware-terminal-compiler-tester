use std::{
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use samdebug_core::{
    CancellationToken, ErrorCategory, SamdebugError, SamdebugResult, ports::ProbeProvider,
};

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct DebugCancellationController {
    token: CancellationToken,
    requested: Arc<AtomicBool>,
    activity: Arc<Mutex<Option<String>>>,
}

impl DebugCancellationController {
    pub fn cancel(&self) {
        self.requested.store(true, Ordering::SeqCst);
        self.token.cancel();
    }

    #[must_use]
    pub fn active_command(&self) -> Option<String> {
        self.activity
            .lock()
            .expect("GDB activity mutex poisoned")
            .clone()
    }
}

use crate::{
    Breakpoint, DisassemblyInstruction, FirmwareArtifact, GdbMiConfig, GdbMiProcess, MemoryBlock,
    OpenOcdConfig, OpenOcdDebugServer, RegisterValue, SessionEngine, SessionEvent, SessionState,
    StackFrame, Variable, list_probes,
};

#[derive(Debug)]
pub struct OwnedDebugSession {
    server: OpenOcdDebugServer,
    engine: SessionEngine<GdbMiProcess>,
    openocd_log_offset: usize,
    pending_events: Vec<SessionEvent>,
    cancellation: CancellationToken,
    supervisor: Option<JoinHandle<()>>,
    user_cancel_requested: Arc<AtomicBool>,
    activity: Arc<Mutex<Option<String>>>,
    supervisor_failure: Arc<Mutex<Option<SamdebugError>>>,
}

impl OwnedDebugSession {
    pub fn launch(
        probes: &dyn ProbeProvider,
        openocd: &OpenOcdConfig,
        gdb: &GdbMiConfig,
        requested_serial: &str,
        firmware: &FirmwareArtifact,
        cancellation: &CancellationToken,
    ) -> SamdebugResult<Self> {
        let mut startup_events = Vec::new();
        let mut session = Self::launch_with_event_sink(
            probes,
            openocd,
            gdb,
            requested_serial,
            firmware,
            cancellation,
            &mut |event| startup_events.push(event.clone()),
        )?;
        // Callers that do not consume the live startup sink receive the same
        // lifecycle once through take_events(). Sink-aware callers already
        // published these events and must not receive them a second time.
        session.pending_events = startup_events;
        Ok(session)
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_lines)]
    pub fn launch_with_event_sink(
        probes: &dyn ProbeProvider,
        openocd: &OpenOcdConfig,
        gdb: &GdbMiConfig,
        requested_serial: &str,
        firmware: &FirmwareArtifact,
        cancellation: &CancellationToken,
        event_sink: &mut dyn FnMut(&SessionEvent),
    ) -> SamdebugResult<Self> {
        validate_selected_probe(probes, requested_serial)?;
        let elf = validate_debug_elf(firmware)?;
        let generation = next_generation()?;
        event_sink(&SessionEvent::ProbeSelected {
            generation,
            probe_serial: requested_serial.to_owned(),
        });
        event_sink(&SessionEvent::State {
            generation,
            previous: SessionState::Idle,
            current: SessionState::ProbeSelected,
        });
        event_sink(&SessionEvent::State {
            generation,
            previous: SessionState::ProbeSelected,
            current: SessionState::ServerStarting,
        });
        event_sink(&SessionEvent::ServerStarting { generation });
        let mut server = match OpenOcdDebugServer::launch(openocd, requested_serial, cancellation) {
            Ok(server) => server,
            Err(error) => {
                emit_startup_failure(event_sink, generation, SessionState::ServerStarting, &error);
                return Err(error);
            }
        };
        for event in [
            SessionEvent::State {
                generation,
                previous: SessionState::ServerStarting,
                current: SessionState::ServerReady,
            },
            SessionEvent::ServerReady {
                generation,
                gdb_port: server.ports().gdb,
            },
            SessionEvent::State {
                generation,
                previous: SessionState::ServerReady,
                current: SessionState::GdbStarting,
            },
            SessionEvent::GdbStarting { generation },
        ] {
            event_sink(&event);
        }
        if cancellation.is_cancelled() {
            let error = interrupted();
            emit_startup_failure(event_sink, generation, SessionState::GdbStarting, &error);
            let _ = server.stop();
            return Err(error);
        }
        let process = match GdbMiProcess::launch_cancellable(gdb, cancellation) {
            Ok(process) => process,
            Err(error) => {
                emit_startup_failure(event_sink, generation, SessionState::GdbStarting, &error);
                let _ = server.stop();
                return Err(error);
            }
        };
        let activity = process.activity_handle();
        let supervisor_failure = Arc::new(Mutex::new(None));
        let supervisor = spawn_supervisor(
            process.child_handle(),
            server.child_handle(),
            cancellation.clone(),
            Arc::clone(&supervisor_failure),
        );
        let mut engine =
            SessionEngine::with_started_lifecycle(process, cancellation.clone(), generation);
        if let Err(error) = engine.connect_started(requested_serial, server.ports().gdb, &elf) {
            let supervised = supervisor_failure
                .lock()
                .expect("supervisor failure mutex poisoned")
                .clone();
            let error =
                resolve_startup_error(error, server.diagnosed_connection_failure(), supervised);
            if error.category() == ErrorCategory::Interrupted {
                engine.cancel("session.start");
            } else {
                engine.fail_and_cleanup(&error);
            }
            for event in engine.take_events() {
                event_sink(&event);
            }
            let _ = server.stop();
            let _ = supervisor.join();
            return Err(error);
        }
        Ok(Self {
            server,
            engine,
            openocd_log_offset: 0,
            pending_events: Vec::new(),
            cancellation: cancellation.clone(),
            supervisor: Some(supervisor),
            user_cancel_requested: Arc::new(AtomicBool::new(false)),
            activity,
            supervisor_failure,
        })
    }

    #[must_use]
    pub const fn state(&self) -> SessionState {
        self.engine.state()
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.engine.generation()
    }

    pub fn take_events(&mut self) -> Vec<SessionEvent> {
        self.synchronize_transport();
        self.synchronize_supervisor();
        let mut events = std::mem::take(&mut self.pending_events);
        events.extend(self.engine.take_events());
        let log = self.server.log();
        if let Some(text) = log.get(self.openocd_log_offset..)
            && !text.is_empty()
        {
            events.push(SessionEvent::Log {
                generation: self.engine.generation(),
                source: "openocd".into(),
                text: text.to_owned(),
            });
        }
        self.openocd_log_offset = log.len();
        events
    }

    #[must_use]
    pub fn cancellation_controller(&self) -> DebugCancellationController {
        DebugCancellationController {
            token: self.cancellation.clone(),
            requested: Arc::clone(&self.user_cancel_requested),
            activity: Arc::clone(&self.activity),
        }
    }

    pub fn continue_target(&mut self) -> SamdebugResult<()> {
        self.execute(SessionEngine::continue_target)
    }

    pub fn halt(&mut self) -> SamdebugResult<Option<StackFrame>> {
        self.execute(SessionEngine::halt)
    }

    pub fn wait_until_stopped(&mut self) -> SamdebugResult<Option<StackFrame>> {
        self.execute(SessionEngine::wait_until_stopped)
    }

    pub fn step(&mut self) -> SamdebugResult<Option<StackFrame>> {
        self.execute(SessionEngine::step)
    }

    pub fn next_target(&mut self) -> SamdebugResult<Option<StackFrame>> {
        self.execute(SessionEngine::next_target)
    }

    pub fn reset_halt(&mut self) -> SamdebugResult<()> {
        self.execute(SessionEngine::reset_halt)
    }

    pub fn insert_breakpoint(
        &mut self,
        location: &str,
        temporary: bool,
    ) -> SamdebugResult<Breakpoint> {
        self.execute(|engine| engine.insert_breakpoint(location, temporary))
    }

    pub fn remove_breakpoint(&mut self, id: &str) -> SamdebugResult<()> {
        self.execute(|engine| engine.remove_breakpoint(id))
    }

    pub fn stack_frames(&mut self, start: u32, levels: u32) -> SamdebugResult<Vec<StackFrame>> {
        self.execute(|engine| engine.stack_frames(start, levels))
    }

    pub fn variables(&mut self, frame: u32) -> SamdebugResult<Vec<Variable>> {
        self.execute(|engine| engine.variables(frame))
    }

    pub fn registers(&mut self, names: &[String]) -> SamdebugResult<Vec<RegisterValue>> {
        self.execute(|engine| engine.registers(names))
    }

    pub fn read_memory(&mut self, address: u64, length: usize) -> SamdebugResult<MemoryBlock> {
        self.execute(|engine| engine.read_memory(address, length))
    }

    pub fn disassemble(
        &mut self,
        address: u64,
        byte_length: usize,
    ) -> SamdebugResult<Vec<DisassemblyInstruction>> {
        self.execute(|engine| engine.disassemble(address, byte_length))
    }

    pub fn load_firmware(&mut self, authorization: &str) -> SamdebugResult<()> {
        self.execute(|engine| engine.load_firmware(authorization))
    }

    pub fn stop(&mut self) -> SamdebugResult<()> {
        self.cancellation.cancel();
        let gdb_result = self.engine.stop();
        let server_result = self.server.stop();
        if let Some(supervisor) = self.supervisor.take() {
            let _ = supervisor.join();
        }
        gdb_result.and(server_result)
    }

    pub fn cancel(&mut self, operation: &str) {
        self.user_cancel_requested.store(true, Ordering::SeqCst);
        self.cancellation.cancel();
        self.engine.cancel(operation);
        let _ = self.server.stop();
        if let Some(supervisor) = self.supervisor.take() {
            let _ = supervisor.join();
        }
    }

    #[must_use]
    pub fn openocd_log(&self) -> String {
        self.server.log()
    }

    fn alive(&mut self) -> SamdebugResult<()> {
        if let Err(cause) = self.server.check_alive() {
            self.engine.fail_and_cleanup(&cause);
            let _ = self.server.stop();
            self.join_supervisor();
            return Err(cause);
        }
        Ok(())
    }

    fn execute<R>(
        &mut self,
        operation: impl FnOnce(&mut SessionEngine<GdbMiProcess>) -> SamdebugResult<R>,
    ) -> SamdebugResult<R> {
        self.poll_transport()?;
        self.alive()?;
        match operation(&mut self.engine) {
            Ok(value) => Ok(value),
            Err(error) if error.category() == ErrorCategory::Interrupted => {
                let returned = if self.user_cancel_requested.load(Ordering::SeqCst) {
                    self.engine.cancel("in_flight");
                    error
                } else {
                    let failure = self.server.diagnosed_connection_failure().unwrap_or_else(|| {
                        self.supervisor_failure().unwrap_or_else(|| {
                            self.server.check_alive().err().unwrap_or_else(|| {
                                SamdebugError::new(
                                    ErrorCategory::Debugger,
                                    "DEBUG_TRANSPORT_FAILED",
                                    "debugger supervision interrupted the operation after a child or transport failure",
                                )
                            })
                        })
                    });
                    self.engine.fail_and_cleanup(&failure);
                    failure
                };
                let _ = self.server.stop();
                self.join_supervisor();
                Err(returned)
            }
            Err(error) if is_fatal_debug_error(&error) => {
                self.engine.fail_and_cleanup(&error);
                let _ = self.server.stop();
                self.join_supervisor();
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    fn join_supervisor(&mut self) {
        if let Some(supervisor) = self.supervisor.take() {
            let _ = supervisor.join();
        }
    }

    fn synchronize_supervisor(&mut self) {
        if !self.cancellation.is_cancelled() || self.engine.state() == SessionState::Idle {
            return;
        }
        if self.user_cancel_requested.load(Ordering::SeqCst) {
            self.engine.cancel("external");
        } else {
            let cause = self
                .server
                .diagnosed_connection_failure()
                .unwrap_or_else(|| {
                    self.supervisor_failure().unwrap_or_else(|| {
                        self.server.check_alive().err().unwrap_or_else(|| {
                            SamdebugError::new(
                                ErrorCategory::Debugger,
                                "DEBUG_TRANSPORT_FAILED",
                                "a supervised debugger child or transport failed",
                            )
                        })
                    })
                });
            self.engine.fail_and_cleanup(&cause);
        }
        let _ = self.server.stop();
        self.join_supervisor();
    }

    fn poll_transport(&mut self) -> SamdebugResult<()> {
        if let Err(error) = self.engine.poll() {
            self.engine.fail_and_cleanup(&error);
            let _ = self.server.stop();
            self.join_supervisor();
            return Err(error);
        }
        Ok(())
    }

    fn synchronize_transport(&mut self) {
        let _ = self.poll_transport();
    }

    fn supervisor_failure(&self) -> Option<SamdebugError> {
        self.supervisor_failure
            .lock()
            .expect("supervisor failure mutex poisoned")
            .clone()
    }
}

impl Drop for OwnedDebugSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn validate_selected_probe(provider: &dyn ProbeProvider, serial: &str) -> SamdebugResult<()> {
    let report = list_probes(provider)?;
    if report.probes.is_empty() {
        return Err(SamdebugError::new(
            ErrorCategory::Connection,
            "PROBE_NOT_FOUND",
            "no Atmel-ICE probe is connected",
        ));
    }
    if report.probes.iter().any(|probe| probe.serial == serial) {
        Ok(())
    } else {
        Err(SamdebugError::new(
            ErrorCategory::Connection,
            "PROBE_SERIAL_NOT_FOUND",
            format!("Atmel-ICE probe {serial} was not found"),
        ))
    }
}

fn validate_debug_elf(firmware: &FirmwareArtifact) -> SamdebugResult<std::path::PathBuf> {
    let build = firmware.build_directory.canonicalize().map_err(|error| {
        SamdebugError::new(
            ErrorCategory::Debugger,
            "FIRMWARE_ARTIFACT_INVALID",
            error.to_string(),
        )
    })?;
    let elf = firmware.path.canonicalize().map_err(|error| {
        SamdebugError::new(
            ErrorCategory::Debugger,
            "FIRMWARE_ARTIFACT_INVALID",
            error.to_string(),
        )
    })?;
    let metadata = std::fs::symlink_metadata(&firmware.path).map_err(|error| {
        SamdebugError::new(
            ErrorCategory::Debugger,
            "FIRMWARE_ARTIFACT_INVALID",
            error.to_string(),
        )
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() == 0
        || elf.parent() != Some(build.as_path())
        || !managed_build_shape(&build)
    {
        return Err(SamdebugError::new(
            ErrorCategory::Debugger,
            "FIRMWARE_ARTIFACT_INVALID",
            "debug ELF must be a non-empty direct child of the managed build directory",
        ));
    }
    Ok(elf)
}

fn managed_build_shape(build: &Path) -> bool {
    build
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "Debug" | "Release"))
        && build
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "build")
        && build
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .is_some_and(|name| name == ".samdebug")
}

fn interrupted() -> SamdebugError {
    SamdebugError::new(
        ErrorCategory::Interrupted,
        "INTERRUPTED",
        "debug session startup interrupted",
    )
}

fn emit_startup_failure(
    sink: &mut dyn FnMut(&SessionEvent),
    generation: u64,
    current: SessionState,
    error: &SamdebugError,
) {
    let terminal = if error.category() == ErrorCategory::Interrupted {
        SessionState::Cancelling
    } else {
        SessionState::Failed
    };
    sink(&SessionEvent::State {
        generation,
        previous: current,
        current: terminal,
    });
    if terminal == SessionState::Cancelling {
        sink(&SessionEvent::Cancelled {
            generation,
            operation: "session.start".into(),
        });
    } else {
        sink(&SessionEvent::SessionError {
            generation,
            code: error.code().to_owned(),
            message: error.to_string(),
            recoverable: true,
        });
    }
    sink(&SessionEvent::State {
        generation,
        previous: terminal,
        current: SessionState::Disconnecting,
    });
    sink(&SessionEvent::State {
        generation,
        previous: SessionState::Disconnecting,
        current: SessionState::Idle,
    });
    sink(&SessionEvent::SessionStopped {
        generation,
        reason: if terminal == SessionState::Cancelling {
            "cancelled".into()
        } else {
            "error".into()
        },
    });
}

fn resolve_startup_error(
    error: SamdebugError,
    diagnosed: Option<SamdebugError>,
    supervised: Option<SamdebugError>,
) -> SamdebugError {
    if error.category() == ErrorCategory::Interrupted {
        diagnosed.or(supervised).unwrap_or(error)
    } else {
        error
    }
}

fn next_generation() -> SamdebugResult<u64> {
    NEXT_GENERATION
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
            value.checked_add(1)
        })
        .map_err(|_| {
            SamdebugError::new(
                ErrorCategory::Debugger,
                "SESSION_GENERATION_EXHAUSTED",
                "debug session generation counter is exhausted",
            )
        })
}

fn is_fatal_debug_error(error: &SamdebugError) -> bool {
    matches!(
        error.code(),
        "GDB_EXITED"
            | "GDB_READ_FAILED"
            | "GDB_WRITE_FAILED"
            | "GDB_COMMAND_TIMEOUT"
            | "GDB_STOP_TIMEOUT"
            | "GDB_TARGET_DISCONNECTED"
            | "MI_RECORD_INVALID"
            | "MI_TOKEN_INVALID"
            | "MI_RECORD_TOO_LARGE"
            | "MI_RECORD_TRUNCATED"
            | "MI_UTF8_INVALID"
    )
}

fn spawn_supervisor(
    gdb: Arc<Mutex<Child>>,
    openocd: Arc<Mutex<Child>>,
    cancellation: CancellationToken,
    failure: Arc<Mutex<Option<SamdebugError>>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        loop {
            if cancellation.is_cancelled() {
                terminate_and_reap(&gdb);
                terminate_and_reap(&openocd);
                break;
            }
            let gdb_exited = child_exited(&gdb);
            let openocd_exited = child_exited(&openocd);
            if gdb_exited || openocd_exited {
                let cause = if gdb_exited {
                    SamdebugError::new(
                        ErrorCategory::Debugger,
                        "GDB_EXITED",
                        "GDB exited while the debug session was active",
                    )
                } else {
                    SamdebugError::new(
                        ErrorCategory::Connection,
                        "OPENOCD_EXITED",
                        "OpenOCD exited while the debug session was active",
                    )
                };
                *failure.lock().expect("supervisor failure mutex poisoned") = Some(cause);
                cancellation.cancel();
                terminate_and_reap(&gdb);
                terminate_and_reap(&openocd);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    })
}

fn child_exited(child: &Arc<Mutex<Child>>) -> bool {
    child
        .lock()
        .expect("supervised child mutex poisoned")
        .try_wait()
        .is_ok_and(|status| status.is_some())
}

fn terminate_and_reap(child: &Arc<Mutex<Child>>) {
    let mut child = child.lock().expect("supervised child mutex poisoned");
    let pid = child.id();
    signal_group(pid, "-TERM");
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let leader_exited = child.try_wait().is_ok_and(|status| status.is_some());
        if Instant::now() >= deadline {
            break;
        }
        if leader_exited {
            // The process-group leader may exit before descendants. Keep the
            // group addressable briefly, then force any remaining descendants
            // down below instead of returning early.
            thread::sleep(Duration::from_millis(50));
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    signal_group(pid, "-KILL");
    if !child.try_wait().is_ok_and(|status| status.is_some()) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: &str) {
    let _ = Command::new("/bin/kill")
        .args([signal, &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn signal_group(_pid: u32, _signal: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generations_are_monotonic_above_individual_sessions() {
        let first = next_generation().expect("generation");
        let second = next_generation().expect("generation");
        assert_eq!(second, first + 1);
    }

    #[test]
    fn startup_failure_sink_receives_terminal_cleanup_lifecycle() {
        let mut events = Vec::new();
        emit_startup_failure(
            &mut |event| events.push(event.clone()),
            41,
            SessionState::ServerStarting,
            &SamdebugError::new(ErrorCategory::Connection, "OPENOCD_START_FAILED", "failed"),
        );
        assert!(matches!(
            events.first(),
            Some(SessionEvent::State {
                current: SessionState::Failed,
                ..
            })
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::State {
                current: SessionState::Disconnecting,
                ..
            }
        )));
        assert!(matches!(
            events.last(),
            Some(SessionEvent::SessionStopped { reason, .. }) if reason == "error"
        ));
    }

    #[test]
    fn startup_transport_failure_overrides_apparent_cancellation_with_typed_cause() {
        let interrupted = SamdebugError::new(
            ErrorCategory::Interrupted,
            "INTERRUPTED",
            "startup interrupted",
        );
        let probe = SamdebugError::new(
            ErrorCategory::Connection,
            "PROBE_DISCONNECTED",
            "probe disconnected",
        );
        let resolved = resolve_startup_error(interrupted, Some(probe), None);
        assert_eq!(resolved.code(), "PROBE_DISCONNECTED");
        assert_eq!(resolved.exit_code(), 5);

        let interrupted = SamdebugError::new(
            ErrorCategory::Interrupted,
            "INTERRUPTED",
            "startup interrupted",
        );
        let gdb = SamdebugError::new(ErrorCategory::Debugger, "GDB_EXITED", "GDB exited");
        assert_eq!(
            resolve_startup_error(interrupted, None, Some(gdb)).code(),
            "GDB_EXITED"
        );
    }

    #[test]
    #[cfg(unix)]
    fn supervisor_reaps_both_groups_on_cancel_or_peer_exit() {
        let cancellation = CancellationToken::new();
        let gdb = spawn_group("/bin/sleep 30");
        let openocd = spawn_group("/bin/sleep 30");
        let supervisor = spawn_supervisor(
            Arc::clone(&gdb),
            Arc::clone(&openocd),
            cancellation.clone(),
            Arc::new(Mutex::new(None)),
        );
        cancellation.cancel();
        supervisor.join().expect("supervisor");
        assert!(child_exited(&gdb));
        assert!(child_exited(&openocd));

        let temp = tempfile::tempdir().expect("tempdir");
        let descendant_file = temp.path().join("descendant.pid");
        let cancellation = CancellationToken::new();
        let gdb = spawn_group(&format!(
            "/bin/sleep 30 & child=$!; echo $child > {}; exit 0",
            descendant_file.display()
        ));
        let openocd = spawn_group("/bin/sleep 30");
        let supervisor = spawn_supervisor(
            Arc::clone(&gdb),
            Arc::clone(&openocd),
            cancellation.clone(),
            Arc::new(Mutex::new(None)),
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while !descendant_file.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let descendant: u32 = std::fs::read_to_string(&descendant_file)
            .expect("descendant pid")
            .trim()
            .parse()
            .expect("numeric pid");
        supervisor.join().expect("supervisor");
        assert!(cancellation.is_cancelled());
        assert!(!pid_is_alive(descendant), "descendant must be terminated");

        let cancellation = CancellationToken::new();
        let gdb = spawn_group("exit 0");
        let openocd = spawn_group("/bin/sleep 30");
        let failure = Arc::new(Mutex::new(None));
        let supervisor = spawn_supervisor(
            Arc::clone(&gdb),
            Arc::clone(&openocd),
            cancellation.clone(),
            Arc::clone(&failure),
        );
        supervisor.join().expect("supervisor");
        assert!(cancellation.is_cancelled());
        assert_eq!(
            failure
                .lock()
                .expect("failure")
                .as_ref()
                .map(SamdebugError::code),
            Some("GDB_EXITED")
        );
        assert!(child_exited(&gdb));
        assert!(child_exited(&openocd));
    }

    #[cfg(unix)]
    fn spawn_group(script: &str) -> Arc<Mutex<Child>> {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        Arc::new(Mutex::new(command.spawn().expect("spawn group")))
    }

    #[cfg(unix)]
    fn pid_is_alive(pid: u32) -> bool {
        Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}
