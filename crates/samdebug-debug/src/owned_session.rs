use std::path::Path;

use samdebug_core::{
    CancellationToken, ErrorCategory, SamdebugError, SamdebugResult, ports::ProbeProvider,
};

use crate::{
    Breakpoint, FirmwareArtifact, GdbMiConfig, GdbMiProcess, MemoryBlock, OpenOcdConfig,
    OpenOcdDebugServer, RegisterValue, SessionEngine, SessionEvent, SessionState, StackFrame,
    Variable, list_probes,
};

#[derive(Debug)]
pub struct OwnedDebugSession {
    server: OpenOcdDebugServer,
    engine: SessionEngine<GdbMiProcess>,
    openocd_log_offset: usize,
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
        validate_selected_probe(probes, requested_serial)?;
        let elf = validate_debug_elf(firmware)?;
        let mut server = OpenOcdDebugServer::launch(openocd, requested_serial, cancellation)?;
        if cancellation.is_cancelled() {
            let _ = server.stop();
            return Err(interrupted());
        }
        let process = match GdbMiProcess::launch(gdb) {
            Ok(process) => process,
            Err(error) => {
                let _ = server.stop();
                return Err(error);
            }
        };
        let mut engine = SessionEngine::new(process);
        if let Err(error) = engine.start_connected(requested_serial, server.ports().gdb, &elf) {
            let _ = engine.stop();
            let _ = server.stop();
            return Err(error);
        }
        Ok(Self {
            server,
            engine,
            openocd_log_offset: 0,
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
        let mut events = self.engine.take_events();
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

    pub fn load_firmware(&mut self, authorization: &str) -> SamdebugResult<()> {
        self.execute(|engine| engine.load_firmware(authorization))
    }

    pub fn stop(&mut self) -> SamdebugResult<()> {
        let gdb_result = self.engine.stop();
        let server_result = self.server.stop();
        gdb_result.and(server_result)
    }

    pub fn cancel(&mut self, operation: &str) {
        self.engine.cancel(operation);
        let _ = self.server.stop();
    }

    #[must_use]
    pub fn openocd_log(&self) -> String {
        self.server.log()
    }

    fn alive(&mut self) -> SamdebugResult<()> {
        if let Err(cause) = self.server.check_alive() {
            let error = SamdebugError::new(
                ErrorCategory::Debugger,
                cause.code(),
                "debug server connection was lost; stop and start a new session to reconnect",
            )
            .with_details(serde_json::json!({"cause": cause}));
            self.engine.fail_and_cleanup(&error);
            let _ = self.server.stop();
            return Err(error);
        }
        Ok(())
    }

    fn execute<R>(
        &mut self,
        operation: impl FnOnce(&mut SessionEngine<GdbMiProcess>) -> SamdebugResult<R>,
    ) -> SamdebugResult<R> {
        self.alive()?;
        match operation(&mut self.engine) {
            Ok(value) => Ok(value),
            Err(error) if is_fatal_debug_error(&error) => {
                self.engine.fail_and_cleanup(&error);
                let _ = self.server.stop();
                Err(error)
            }
            Err(error) => Err(error),
        }
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

fn is_fatal_debug_error(error: &SamdebugError) -> bool {
    matches!(
        error.code(),
        "GDB_EXITED"
            | "GDB_READ_FAILED"
            | "GDB_WRITE_FAILED"
            | "GDB_COMMAND_TIMEOUT"
            | "GDB_STOP_TIMEOUT"
    )
}
