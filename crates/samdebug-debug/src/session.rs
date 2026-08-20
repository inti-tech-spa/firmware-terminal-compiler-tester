use std::{collections::BTreeMap, path::Path, time::Duration};

use samdebug_core::{CancellationToken, ErrorCategory, SamdebugError, SamdebugResult};
use serde::Serialize;

use crate::{MiListItem, MiRecord, MiResult, MiValue};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_MEMORY_READ: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    ProbeSelected,
    ServerStarting,
    ServerReady,
    GdbStarting,
    Connected,
    Halted,
    Running,
    Failed,
    Cancelling,
    Disconnecting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiCommandOutput {
    pub result_class: String,
    pub results: Vec<MiResult>,
    pub records: Vec<MiRecord>,
    /// True only when this command observed a stop record after it was issued.
    pub stopped_after_command: bool,
}

pub trait DebuggerTransport: std::fmt::Debug + Send {
    fn command(
        &mut self,
        command: &str,
        wait_for_stop: bool,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> SamdebugResult<MiCommandOutput>;
    fn wait_for_stop(
        &mut self,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> SamdebugResult<Vec<MiRecord>>;
    fn poll_records(&mut self) -> SamdebugResult<Vec<MiRecord>> {
        Ok(Vec::new())
    }
    fn shutdown(&mut self) -> SamdebugResult<()>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", content = "payload", rename_all = "snake_case")]
pub enum SessionEvent {
    ProbeSelected {
        generation: u64,
        probe_serial: String,
    },
    ServerStarting {
        generation: u64,
    },
    ServerReady {
        generation: u64,
        gdb_port: u16,
    },
    GdbStarting {
        generation: u64,
    },
    State {
        generation: u64,
        previous: SessionState,
        current: SessionState,
    },
    Running {
        generation: u64,
    },
    Stopped {
        generation: u64,
        reason: String,
        frame: Option<StackFrame>,
    },
    Reset {
        generation: u64,
    },
    Progress {
        generation: u64,
        operation: String,
        completed: u64,
        total: u64,
        unit: String,
    },
    Loaded {
        generation: u64,
        verified: bool,
    },
    TargetOutput {
        generation: u64,
        stream: String,
        text: String,
    },
    Log {
        generation: u64,
        source: String,
        text: String,
    },
    SessionError {
        generation: u64,
        code: String,
        message: String,
        recoverable: bool,
    },
    Cancelled {
        generation: u64,
        operation: String,
    },
    SessionStopped {
        generation: u64,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StackFrame {
    pub index: u32,
    pub function: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub address: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Variable {
    pub name: String,
    pub value: String,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegisterValue {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Breakpoint {
    pub id: String,
    pub location: String,
    pub enabled: bool,
    pub temporary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryBlock {
    pub address: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub struct SessionEngine<T: DebuggerTransport> {
    transport: T,
    state: SessionState,
    generation: u64,
    probe_serial: Option<String>,
    events: Vec<SessionEvent>,
    cancellation: CancellationToken,
}

impl<T: DebuggerTransport> SessionEngine<T> {
    #[must_use]
    pub fn new(transport: T) -> Self {
        Self::with_cancellation(transport, CancellationToken::new())
    }

    #[must_use]
    pub const fn with_cancellation(transport: T, cancellation: CancellationToken) -> Self {
        Self {
            transport,
            state: SessionState::Idle,
            generation: 0,
            probe_serial: None,
            events: Vec::new(),
            cancellation,
        }
    }

    #[must_use]
    pub const fn with_started_lifecycle(
        transport: T,
        cancellation: CancellationToken,
        generation: u64,
    ) -> Self {
        Self {
            transport,
            state: SessionState::GdbStarting,
            generation,
            probe_serial: None,
            events: Vec::new(),
            cancellation,
        }
    }

    #[must_use]
    pub const fn state(&self) -> SessionState {
        self.state
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn take_events(&mut self) -> Vec<SessionEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn poll(&mut self) -> SamdebugResult<()> {
        if self.state == SessionState::Idle {
            return Ok(());
        }
        let records = self.transport.poll_records()?;
        self.capture_records(&records);
        if self.state == SessionState::Running
            && records
                .iter()
                .any(|record| matches!(record, MiRecord::Exec { class, .. } if class == "stopped"))
        {
            self.finish_stop(&records, "unknown");
        }
        Ok(())
    }

    pub fn start_connected(
        &mut self,
        probe_serial: &str,
        gdb_port: u16,
        elf: &Path,
    ) -> SamdebugResult<()> {
        self.require(&[SessionState::Idle], "session.start")?;
        if probe_serial.is_empty() || probe_serial.chars().any(char::is_control) {
            return Err(debug_error("PROBE_SERIAL_INVALID", "invalid probe serial"));
        }
        let elf = elf
            .to_str()
            .ok_or_else(|| debug_error("FIRMWARE_PATH_INVALID", "ELF path is not valid UTF-8"))?;
        self.generation = self.generation.saturating_add(1);
        self.probe_serial = Some(probe_serial.to_owned());
        self.transition(SessionState::ProbeSelected);
        self.events.push(SessionEvent::ProbeSelected {
            generation: self.generation,
            probe_serial: probe_serial.to_owned(),
        });
        self.transition(SessionState::ServerStarting);
        self.events.push(SessionEvent::ServerStarting {
            generation: self.generation,
        });
        self.transition(SessionState::ServerReady);
        self.events.push(SessionEvent::ServerReady {
            generation: self.generation,
            gdb_port,
        });
        self.transition(SessionState::GdbStarting);
        self.events.push(SessionEvent::GdbStarting {
            generation: self.generation,
        });
        self.connect_started(probe_serial, gdb_port, Path::new(elf))
    }

    pub fn connect_started(
        &mut self,
        probe_serial: &str,
        gdb_port: u16,
        elf: &Path,
    ) -> SamdebugResult<()> {
        self.require(&[SessionState::GdbStarting], "session.connect")?;
        if self.generation == 0 {
            return Err(debug_error(
                "SESSION_GENERATION_INVALID",
                "debug generation must be nonzero",
            ));
        }
        if probe_serial.is_empty() || probe_serial.chars().any(char::is_control) {
            return Err(debug_error("PROBE_SERIAL_INVALID", "invalid probe serial"));
        }
        let elf = elf
            .to_str()
            .ok_or_else(|| debug_error("FIRMWARE_PATH_INVALID", "ELF path is not valid UTF-8"))?;
        self.probe_serial = Some(probe_serial.to_owned());
        self.run_done("-gdb-set mi-async on", false)?;
        self.run_done(&format!("-file-exec-and-symbols {}", mi_quote(elf)), false)?;
        self.run_connected(&format!("-target-select remote 127.0.0.1:{gdb_port}"))?;
        self.transition(SessionState::Connected);
        self.reset_and_confirm_halted()?;
        self.transition(SessionState::Halted);
        self.events.push(SessionEvent::Stopped {
            generation: self.generation,
            reason: "reset".into(),
            frame: None,
        });
        Ok(())
    }

    pub fn continue_target(&mut self) -> SamdebugResult<()> {
        self.require(&[SessionState::Halted], "target.continue")?;
        let output = self.run_running("-exec-continue", false)?;
        self.transition(SessionState::Running);
        self.events.push(SessionEvent::Running {
            generation: self.generation,
        });
        if output.stopped_after_command {
            self.finish_stop(&output.records, "unknown");
        }
        Ok(())
    }

    pub fn halt(&mut self) -> SamdebugResult<Option<StackFrame>> {
        self.require(&[SessionState::Running], "target.halt")?;
        let output = self.run_done("-exec-interrupt --all", true)?;
        Ok(self.finish_stop(&output.records, "halt"))
    }

    pub fn wait_until_stopped(&mut self) -> SamdebugResult<Option<StackFrame>> {
        self.require(&[SessionState::Running], "target.wait")?;
        let records = self
            .transport
            .wait_for_stop(COMMAND_TIMEOUT, &self.cancellation)?;
        self.capture_records(&records);
        Ok(self.finish_stop(&records, "unknown"))
    }

    pub fn step(&mut self) -> SamdebugResult<Option<StackFrame>> {
        self.exec_step("target.step", "-exec-step")
    }

    pub fn next_target(&mut self) -> SamdebugResult<Option<StackFrame>> {
        self.exec_step("target.next", "-exec-next")
    }

    fn exec_step(&mut self, operation: &str, command: &str) -> SamdebugResult<Option<StackFrame>> {
        self.require(&[SessionState::Halted], operation)?;
        let output = self.run_running(command, true)?;
        self.transition(SessionState::Running);
        self.events.push(SessionEvent::Running {
            generation: self.generation,
        });
        let stopped = if output.stopped_after_command {
            output
        } else {
            self.run_done("-exec-interrupt --all", true)?
        };
        Ok(self.finish_stop(&stopped.records, "step"))
    }

    pub fn reset_halt(&mut self) -> SamdebugResult<()> {
        self.require(
            &[SessionState::Halted, SessionState::Running],
            "target.reset",
        )?;
        self.reset_and_confirm_halted()?;
        if self.state == SessionState::Running {
            self.transition(SessionState::Halted);
        }
        self.events.push(SessionEvent::Reset {
            generation: self.generation,
        });
        self.events.push(SessionEvent::Stopped {
            generation: self.generation,
            reason: "reset".into(),
            frame: None,
        });
        Ok(())
    }

    pub fn insert_breakpoint(
        &mut self,
        location: &str,
        temporary: bool,
    ) -> SamdebugResult<Breakpoint> {
        self.require(&[SessionState::Halted], "breakpoint.insert")?;
        if location.is_empty() || location.contains(['\n', '\r']) {
            return Err(debug_error(
                "BREAKPOINT_INVALID",
                "invalid breakpoint location",
            ));
        }
        let command = format!(
            "-break-insert {}{}",
            if temporary { "-t " } else { "" },
            mi_quote(location)
        );
        let output = self.run_done(&command, false)?;
        let tuple = result(&output.results, "bkpt")
            .and_then(as_tuple)
            .ok_or_else(|| {
                debug_error("GDB_RESPONSE_INVALID", "breakpoint response omitted bkpt")
            })?;
        Ok(Breakpoint {
            id: const_result(tuple, "number").unwrap_or_default().to_owned(),
            location: location.to_owned(),
            enabled: const_result(tuple, "enabled") != Some("n"),
            temporary,
        })
    }

    pub fn remove_breakpoint(&mut self, id: &str) -> SamdebugResult<()> {
        self.require(&[SessionState::Halted], "breakpoint.remove")?;
        if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit() || byte == b'.') {
            return Err(debug_error("BREAKPOINT_INVALID", "invalid breakpoint id"));
        }
        self.run_done(&format!("-break-delete {id}"), false)?;
        Ok(())
    }

    pub fn stack_frames(&mut self, start: u32, levels: u32) -> SamdebugResult<Vec<StackFrame>> {
        self.require(&[SessionState::Halted], "stack.list")?;
        if levels == 0 || levels > 256 {
            return Err(debug_error("STACK_RANGE_INVALID", "levels must be 1..=256"));
        }
        let end = start.saturating_add(levels - 1);
        let output = self.run_done(&format!("-stack-list-frames {start} {end}"), false)?;
        Ok(list_values(result(&output.results, "stack"))
            .filter_map(|value| as_tuple(value).and_then(parse_frame))
            .collect())
    }

    pub fn variables(&mut self, frame: u32) -> SamdebugResult<Vec<Variable>> {
        self.require(&[SessionState::Halted], "variables.list")?;
        self.run_done(&format!("-stack-select-frame {frame}"), false)?;
        let output = self.run_done("-stack-list-variables --all-values", false)?;
        Ok(list_values(result(&output.results, "variables"))
            .filter_map(as_tuple)
            .map(|tuple| Variable {
                name: const_result(tuple, "name").unwrap_or_default().to_owned(),
                value: const_result(tuple, "value").unwrap_or_default().to_owned(),
                type_name: const_result(tuple, "type").map(str::to_owned),
            })
            .collect())
    }

    pub fn registers(&mut self, requested: &[String]) -> SamdebugResult<Vec<RegisterValue>> {
        self.require(&[SessionState::Halted], "registers.read")?;
        if requested.is_empty() {
            return Err(debug_error(
                "REGISTER_LIST_EMPTY",
                "at least one register is required",
            ));
        }
        let names_output = self.run_done("-data-list-register-names", false)?;
        let names = list_consts(result(&names_output.results, "register-names"));
        let by_name = names
            .iter()
            .enumerate()
            .map(|(index, name)| (name.as_str(), index))
            .collect::<BTreeMap<_, _>>();
        let mut indices = Vec::with_capacity(requested.len());
        for name in requested {
            let index = by_name.get(name.as_str()).ok_or_else(|| {
                debug_error("REGISTER_UNKNOWN", format!("unknown register {name}"))
            })?;
            indices.push(*index);
        }
        let suffix = indices
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        let output = self.run_done(&format!("-data-list-register-values x {suffix}"), false)?;
        let values = list_values(result(&output.results, "register-values"))
            .filter_map(as_tuple)
            .filter_map(|tuple| {
                Some((
                    const_result(tuple, "number")?.parse::<usize>().ok()?,
                    const_result(tuple, "value")?.to_owned(),
                ))
            })
            .collect::<BTreeMap<_, _>>();
        Ok(indices
            .into_iter()
            .zip(requested.iter())
            .map(|(index, name)| RegisterValue {
                name: name.clone(),
                value: values.get(&index).cloned().unwrap_or_default(),
            })
            .collect())
    }

    pub fn read_memory(&mut self, address: u64, length: usize) -> SamdebugResult<MemoryBlock> {
        self.require(&[SessionState::Halted], "memory.read")?;
        if length == 0 || length > MAX_MEMORY_READ {
            return Err(debug_error(
                "MEMORY_LENGTH_INVALID",
                "memory length must be 1..=65536",
            ));
        }
        let output = self.run_done(
            &format!("-data-read-memory-bytes 0x{address:x} {length}"),
            false,
        )?;
        let memory = list_values(result(&output.results, "memory"))
            .find_map(as_tuple)
            .ok_or_else(|| debug_error("GDB_RESPONSE_INVALID", "memory response omitted data"))?;
        let hex = const_result(memory, "contents").ok_or_else(|| {
            debug_error("GDB_RESPONSE_INVALID", "memory response omitted contents")
        })?;
        let bytes = decode_hex(hex)?;
        if bytes.len() != length {
            return Err(debug_error(
                "GDB_RESPONSE_INVALID",
                "memory response length does not match request",
            ));
        }
        Ok(MemoryBlock { address, bytes })
    }

    pub fn load_firmware(&mut self, authorization: &str) -> SamdebugResult<()> {
        self.require(&[SessionState::Halted], "firmware.load")?;
        let serial = self.probe_serial.as_deref().ok_or_else(|| {
            debug_error(
                "SESSION_PROBE_MISSING",
                "debug session has no selected probe",
            )
        })?;
        let expected = format!("firmware.load:{serial}");
        if authorization != expected {
            return Err(SamdebugError::new(
                ErrorCategory::Authorization,
                "AUTHORIZATION_REJECTED",
                format!("expected authorization {expected}"),
            ));
        }
        self.events.push(SessionEvent::Progress {
            generation: self.generation,
            operation: "firmware.load".into(),
            completed: 0,
            total: 1,
            unit: "steps".into(),
        });
        self.run_done("-target-download", false)?;
        let verification = self.run_done(
            &format!("-interpreter-exec console {}", mi_quote("compare-sections")),
            false,
        )?;
        let verification_text = verification
            .records
            .iter()
            .filter_map(|record| match record {
                MiRecord::Console(text) | MiRecord::Log(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>()
            .to_ascii_lowercase();
        if verification_text.contains("mis-match")
            || verification_text.contains("mismatch")
            || verification_text.contains("failed")
            || !verification_text.contains("matched")
        {
            return Err(debug_error(
                "FIRMWARE_VERIFY_FAILED",
                "GDB compare-sections did not affirmatively verify every firmware section",
            ));
        }
        self.reset_and_confirm_halted()?;
        self.events.push(SessionEvent::Progress {
            generation: self.generation,
            operation: "firmware.load".into(),
            completed: 1,
            total: 1,
            unit: "steps".into(),
        });
        self.events.push(SessionEvent::Loaded {
            generation: self.generation,
            verified: true,
        });
        Ok(())
    }

    pub fn stop(&mut self) -> SamdebugResult<()> {
        if self.state == SessionState::Idle {
            return Ok(());
        }
        self.transition(SessionState::Disconnecting);
        let result = self.transport.shutdown();
        let generation = self.generation;
        self.transition(SessionState::Idle);
        self.probe_serial = None;
        self.events.push(SessionEvent::SessionStopped {
            generation,
            reason: "requested".into(),
        });
        result
    }

    pub fn fail_and_cleanup(&mut self, error: &SamdebugError) {
        if self.state == SessionState::Idle {
            return;
        }
        self.transition(SessionState::Failed);
        self.events.push(SessionEvent::SessionError {
            generation: self.generation,
            code: error.code().to_owned(),
            message: error.to_string(),
            recoverable: true,
        });
        self.transition(SessionState::Disconnecting);
        let _ = self.transport.shutdown();
        let generation = self.generation;
        self.transition(SessionState::Idle);
        self.probe_serial = None;
        self.events.push(SessionEvent::SessionStopped {
            generation,
            reason: "error".into(),
        });
    }

    pub fn cancel(&mut self, operation: &str) {
        if self.state == SessionState::Idle {
            return;
        }
        self.transition(SessionState::Cancelling);
        self.events.push(SessionEvent::Cancelled {
            generation: self.generation,
            operation: operation.to_owned(),
        });
        self.transition(SessionState::Disconnecting);
        let _ = self.transport.shutdown();
        let generation = self.generation;
        self.transition(SessionState::Idle);
        self.probe_serial = None;
        self.events.push(SessionEvent::SessionStopped {
            generation,
            reason: "cancelled".into(),
        });
    }

    fn finish_stop(&mut self, records: &[MiRecord], fallback_reason: &str) -> Option<StackFrame> {
        let (reason, frame) = stopped_details(records).unwrap_or((fallback_reason.into(), None));
        self.transition(SessionState::Halted);
        self.events.push(SessionEvent::Stopped {
            generation: self.generation,
            reason,
            frame: frame.clone(),
        });
        frame
    }

    fn run_done(&mut self, command: &str, wait_for_stop: bool) -> SamdebugResult<MiCommandOutput> {
        let output =
            self.transport
                .command(command, wait_for_stop, COMMAND_TIMEOUT, &self.cancellation)?;
        self.capture_records(&output.records);
        if output.result_class == "done" || output.result_class == "exit" {
            Ok(output)
        } else {
            Err(command_error(command, &output))
        }
    }

    fn run_connected(&mut self, command: &str) -> SamdebugResult<MiCommandOutput> {
        let output = self
            .transport
            .command(command, false, COMMAND_TIMEOUT, &self.cancellation)?;
        self.capture_records(&output.records);
        if output.result_class == "connected" || output.result_class == "done" {
            Ok(output)
        } else {
            Err(command_error(command, &output))
        }
    }

    fn run_running(
        &mut self,
        command: &str,
        wait_for_stop: bool,
    ) -> SamdebugResult<MiCommandOutput> {
        let output =
            self.transport
                .command(command, wait_for_stop, COMMAND_TIMEOUT, &self.cancellation)?;
        self.capture_records(&output.records);
        if output.result_class == "running" {
            Ok(output)
        } else {
            Err(command_error(command, &output))
        }
    }

    fn capture_records(&mut self, records: &[MiRecord]) {
        for record in records {
            let event = match record {
                MiRecord::Target(text) => Some(SessionEvent::TargetOutput {
                    generation: self.generation,
                    stream: "stdout".into(),
                    text: text.clone(),
                }),
                MiRecord::Console(text) | MiRecord::Log(text) => Some(SessionEvent::Log {
                    generation: self.generation,
                    source: "gdb".into(),
                    text: text.clone(),
                }),
                _ => None,
            };
            if let Some(event) = event {
                self.events.push(event);
            }
        }
    }

    fn reset_and_confirm_halted(&mut self) -> SamdebugResult<()> {
        self.run_done(
            &format!(
                "-interpreter-exec console {}",
                mi_quote("monitor reset halt")
            ),
            true,
        )?;
        let output = self.run_done("-stack-info-frame", false)?;
        if result(&output.results, "frame")
            .and_then(as_tuple)
            .is_none()
        {
            return Err(debug_error(
                "TARGET_HALT_UNCONFIRMED",
                "GDB did not return a current frame after reset halt",
            ));
        }
        Ok(())
    }

    fn require(&self, allowed: &[SessionState], operation: &str) -> SamdebugResult<()> {
        if allowed.contains(&self.state) {
            Ok(())
        } else {
            Err(debug_error(
                "INVALID_SESSION_STATE",
                format!("{operation} is invalid while session is {:?}", self.state),
            ))
        }
    }

    fn transition(&mut self, next: SessionState) {
        let previous = self.state;
        self.state = next;
        if self.generation > 0 {
            self.events.push(SessionEvent::State {
                generation: self.generation,
                previous,
                current: next,
            });
        }
    }
}

impl<T: DebuggerTransport> Drop for SessionEngine<T> {
    fn drop(&mut self) {
        if self.state != SessionState::Idle {
            let _ = self.transport.shutdown();
            self.state = SessionState::Idle;
        }
    }
}

fn result<'a>(results: &'a [MiResult], name: &str) -> Option<&'a MiValue> {
    results
        .iter()
        .find(|result| result.variable == name)
        .map(|result| &result.value)
}

fn as_tuple(value: &MiValue) -> Option<&[MiResult]> {
    if let MiValue::Tuple(results) = value {
        Some(results)
    } else {
        None
    }
}

fn const_result<'a>(results: &'a [MiResult], name: &str) -> Option<&'a str> {
    match result(results, name) {
        Some(MiValue::Const(value)) => Some(value),
        _ => None,
    }
}

fn list_values(value: Option<&MiValue>) -> impl Iterator<Item = &MiValue> {
    value
        .and_then(|value| match value {
            MiValue::List(items) => Some(items.as_slice()),
            _ => None,
        })
        .unwrap_or_default()
        .iter()
        .map(|item| match item {
            MiListItem::Value(value) => value,
            MiListItem::Result(result) => &result.value,
        })
}

fn list_consts(value: Option<&MiValue>) -> Vec<String> {
    list_values(value)
        .filter_map(|value| match value {
            MiValue::Const(value) => Some(value.clone()),
            _ => None,
        })
        .collect()
}

fn parse_frame(tuple: &[MiResult]) -> Option<StackFrame> {
    Some(StackFrame {
        index: const_result(tuple, "level")?.parse().ok()?,
        function: const_result(tuple, "func").unwrap_or("??").to_owned(),
        file: const_result(tuple, "fullname")
            .or_else(|| const_result(tuple, "file"))
            .map(str::to_owned),
        line: const_result(tuple, "line").and_then(|line| line.parse().ok()),
        address: const_result(tuple, "addr").and_then(parse_address),
    })
}

fn parse_address(value: &str) -> Option<u64> {
    u64::from_str_radix(value.strip_prefix("0x").unwrap_or(value), 16).ok()
}

fn stopped_details(records: &[MiRecord]) -> Option<(String, Option<StackFrame>)> {
    records.iter().find_map(|record| {
        let MiRecord::Exec { class, results } = record else {
            return None;
        };
        if class != "stopped" {
            return None;
        }
        let reason = match const_result(results, "reason").unwrap_or("unknown") {
            "breakpoint-hit" => "breakpoint",
            "end-stepping-range" | "function-finished" => "step",
            "signal-received" => "signal",
            "watchpoint-trigger" | "read-watchpoint-trigger" | "access-watchpoint-trigger" => {
                "watchpoint"
            }
            value => value,
        };
        Some((
            reason.to_owned(),
            result(results, "frame")
                .and_then(as_tuple)
                .and_then(parse_frame),
        ))
    })
}

fn decode_hex(value: &str) -> SamdebugResult<Vec<u8>> {
    if !value.len().is_multiple_of(2) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(debug_error("GDB_RESPONSE_INVALID", "invalid memory hex"));
    }
    (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16)
                .map_err(|error| debug_error("GDB_RESPONSE_INVALID", error.to_string()))
        })
        .collect()
}

fn mi_quote(value: &str) -> String {
    let mut output = String::from('"');
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            value => output.push(value),
        }
    }
    output.push('"');
    output
}

fn command_error(command: &str, output: &MiCommandOutput) -> SamdebugError {
    let message = const_result(&output.results, "msg").unwrap_or("GDB did not provide a message");
    let lower = message.to_ascii_lowercase();
    let code = if lower.contains("remote connection closed")
        || lower.contains("remote communication error")
        || lower.contains("target disconnected")
        || lower.contains("not connected")
    {
        "GDB_TARGET_DISCONNECTED"
    } else {
        "GDB_COMMAND_FAILED"
    };
    SamdebugError::new(
        ErrorCategory::Debugger,
        code,
        format!("GDB/MI command failed: {command}: {message}"),
    )
    .with_details(serde_json::json!({
        "result_class": output.result_class,
        "gdb_message": message
    }))
}

fn debug_error(code: &str, message: impl Into<String>) -> SamdebugError {
    SamdebugError::new(ErrorCategory::Debugger, code, message)
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, fs};

    use super::*;

    #[derive(Debug)]
    struct FakeTransport {
        commands: Vec<String>,
        stop_wait_commands: Vec<String>,
        outputs: VecDeque<MiCommandOutput>,
        pending_records: Vec<MiRecord>,
        shutdowns: usize,
    }

    impl FakeTransport {
        fn scripted(outputs: Vec<MiCommandOutput>) -> Self {
            Self {
                commands: Vec::new(),
                stop_wait_commands: Vec::new(),
                outputs: outputs.into(),
                pending_records: Vec::new(),
                shutdowns: 0,
            }
        }
    }

    impl DebuggerTransport for FakeTransport {
        fn command(
            &mut self,
            command: &str,
            wait_for_stop: bool,
            _timeout: Duration,
            cancellation: &CancellationToken,
        ) -> SamdebugResult<MiCommandOutput> {
            if cancellation.is_cancelled() {
                return Err(SamdebugError::new(
                    ErrorCategory::Interrupted,
                    "INTERRUPTED",
                    "debug operation interrupted",
                ));
            }
            self.commands.push(command.to_owned());
            if wait_for_stop {
                self.stop_wait_commands.push(command.to_owned());
            }
            self.outputs
                .pop_front()
                .ok_or_else(|| debug_error("TEST_OUTPUT_MISSING", command))
        }

        fn shutdown(&mut self) -> SamdebugResult<()> {
            self.shutdowns += 1;
            Ok(())
        }

        fn poll_records(&mut self) -> SamdebugResult<Vec<MiRecord>> {
            Ok(std::mem::take(&mut self.pending_records))
        }

        fn wait_for_stop(
            &mut self,
            _timeout: Duration,
            cancellation: &CancellationToken,
        ) -> SamdebugResult<Vec<MiRecord>> {
            if cancellation.is_cancelled() {
                return Err(SamdebugError::new(
                    ErrorCategory::Interrupted,
                    "INTERRUPTED",
                    "debug operation interrupted",
                ));
            }
            Ok(self
                .outputs
                .pop_front()
                .ok_or_else(|| debug_error("TEST_OUTPUT_MISSING", "wait"))?
                .records)
        }
    }

    fn done() -> MiCommandOutput {
        MiCommandOutput {
            result_class: "done".into(),
            results: Vec::new(),
            records: Vec::new(),
            stopped_after_command: false,
        }
    }

    fn started_transport(extra: Vec<MiCommandOutput>) -> FakeTransport {
        let mut outputs = vec![done(), done()];
        outputs.push(MiCommandOutput {
            result_class: "connected".into(),
            results: Vec::new(),
            records: Vec::new(),
            stopped_after_command: false,
        });
        outputs.push(done());
        outputs.push(frame_output());
        outputs.extend(extra);
        FakeTransport::scripted(outputs)
    }

    fn frame_output() -> MiCommandOutput {
        MiCommandOutput {
            result_class: "done".into(),
            results: vec![MiResult {
                variable: "frame".into(),
                value: tuple(&[("level", "0"), ("func", "main"), ("addr", "0x00400100")]),
            }],
            records: Vec::new(),
            stopped_after_command: false,
        }
    }

    fn start(engine: &mut SessionEngine<FakeTransport>) {
        let temp = tempfile::tempdir().expect("tempdir");
        let elf = temp.path().join("firmware.elf");
        fs::write(&elf, b"ELF").expect("elf");
        engine
            .start_connected("ATML123", 33_333, &elf)
            .expect("start");
    }

    #[test]
    fn lifecycle_enforces_states_generation_and_cleanup() {
        let mut engine = SessionEngine::new(started_transport(vec![MiCommandOutput {
            result_class: "running".into(),
            results: Vec::new(),
            records: Vec::new(),
            stopped_after_command: false,
        }]));
        assert_eq!(
            engine.continue_target().unwrap_err().code(),
            "INVALID_SESSION_STATE"
        );
        start(&mut engine);
        assert_eq!(engine.state(), SessionState::Halted);
        assert_eq!(engine.generation(), 1);
        assert!(
            engine
                .transport
                .stop_wait_commands
                .iter()
                .any(|command| command.contains("monitor reset halt")),
            "reset-halt must consume its own asynchronous stop record"
        );
        engine.continue_target().expect("continue");
        assert_eq!(engine.state(), SessionState::Running);
        assert_eq!(engine.step().unwrap_err().code(), "INVALID_SESSION_STATE");
        engine.stop().expect("stop");
        assert_eq!(engine.state(), SessionState::Idle);
        assert_eq!(engine.transport.shutdowns, 1);
    }

    #[test]
    fn poll_dispatches_idle_streams_and_running_stop_for_current_generation() {
        let mut engine = SessionEngine::new(started_transport(vec![MiCommandOutput {
            result_class: "running".into(),
            results: Vec::new(),
            records: Vec::new(),
            stopped_after_command: false,
        }]));
        start(&mut engine);
        engine.continue_target().expect("continue");
        engine.transport.pending_records = vec![
            MiRecord::Target("late target output\n".into()),
            MiRecord::Exec {
                class: "stopped".into(),
                results: vec![MiResult {
                    variable: "reason".into(),
                    value: MiValue::Const("breakpoint-hit".into()),
                }],
            },
        ];
        engine.poll().expect("poll");
        assert_eq!(engine.state(), SessionState::Halted);
        let generation = engine.generation();
        let events = engine.take_events();
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::TargetOutput { generation: event_generation, text, .. }
                if *event_generation == generation && text == "late target output\n"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::Stopped { generation: event_generation, reason, .. }
                if *event_generation == generation && reason == "breakpoint"
        )));
    }

    #[test]
    fn supports_breakpoints_stack_variables_registers_memory_and_authorized_load() {
        let breakpoint = MiCommandOutput {
            result_class: "done".into(),
            results: vec![MiResult {
                variable: "bkpt".into(),
                value: MiValue::Tuple(vec![
                    MiResult {
                        variable: "number".into(),
                        value: MiValue::Const("1".into()),
                    },
                    MiResult {
                        variable: "enabled".into(),
                        value: MiValue::Const("y".into()),
                    },
                ]),
            }],
            records: vec![MiRecord::Target("query output\n".into())],
            stopped_after_command: false,
        };
        let stack = list_output(
            "stack",
            vec![tuple(&[
                ("level", "0"),
                ("func", "main"),
                ("addr", "0x00400100"),
            ])],
        );
        let variables = list_output(
            "variables",
            vec![tuple(&[("name", "x"), ("value", "7"), ("type", "int")])],
        );
        let register_names = list_output(
            "register-names",
            vec![MiValue::Const("r0".into()), MiValue::Const("pc".into())],
        );
        let register_values = list_output(
            "register-values",
            vec![tuple(&[("number", "1"), ("value", "0x400100")])],
        );
        let memory = list_output("memory", vec![tuple(&[("contents", "0102a0ff")])]);
        let mut engine = SessionEngine::new(started_transport(vec![
            breakpoint,
            stack,
            done(),
            variables,
            register_names,
            register_values,
            memory,
            done(),
            MiCommandOutput {
                result_class: "done".into(),
                results: Vec::new(),
                records: vec![MiRecord::Console(
                    "Section .text, range 0x400000 -- 0x401000: matched.\n".into(),
                )],
                stopped_after_command: false,
            },
            done(),
            frame_output(),
        ]));
        start(&mut engine);
        assert_eq!(engine.insert_breakpoint("main", false).unwrap().id, "1");
        assert!(engine.take_events().iter().any(
            |event| matches!(event, SessionEvent::TargetOutput { text, .. } if text == "query output\n")
        ));
        assert_eq!(engine.stack_frames(0, 32).unwrap()[0].function, "main");
        assert_eq!(engine.variables(0).unwrap()[0].value, "7");
        assert_eq!(
            engine.registers(&["pc".into()]).unwrap()[0].value,
            "0x400100"
        );
        assert_eq!(
            engine.read_memory(0x0040_0000, 4).unwrap().bytes,
            [1, 2, 160, 255]
        );
        assert_eq!(engine.load_firmware("wrong").unwrap_err().exit_code(), 8);
        engine.load_firmware("firmware.load:ATML123").expect("load");
    }

    #[test]
    fn failure_and_cancellation_pass_through_cleanup_to_idle() {
        let mut failed = SessionEngine::new(started_transport(Vec::new()));
        start(&mut failed);
        failed.fail_and_cleanup(&debug_error("GDB_EXITED", "GDB crashed"));
        assert_eq!(failed.state(), SessionState::Idle);
        assert_eq!(failed.transport.shutdowns, 1);
        let events = failed.take_events();
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::State {
                current: SessionState::Failed,
                ..
            }
        )));
        assert!(events.iter().any(
            |event| matches!(event, SessionEvent::SessionStopped { reason, .. } if reason == "error")
        ));

        let mut cancelled = SessionEngine::new(started_transport(Vec::new()));
        start(&mut cancelled);
        cancelled.cancel("target.step");
        assert_eq!(cancelled.state(), SessionState::Idle);
        assert_eq!(cancelled.transport.shutdowns, 1);
        assert!(cancelled.take_events().iter().any(
            |event| matches!(event, SessionEvent::Cancelled { operation, .. } if operation == "target.step")
        ));
    }

    #[test]
    fn firmware_load_rejects_compare_mismatch_before_reset() {
        let mismatch = MiCommandOutput {
            result_class: "done".into(),
            results: Vec::new(),
            records: vec![MiRecord::Console(
                "Section .text, range 0x400000 -- 0x401000: MIS-MATCHED!\n".into(),
            )],
            stopped_after_command: false,
        };
        let mut engine = SessionEngine::new(started_transport(vec![done(), mismatch]));
        start(&mut engine);
        let error = engine
            .load_firmware("firmware.load:ATML123")
            .expect_err("verification mismatch");
        assert_eq!(error.code(), "FIRMWARE_VERIFY_FAILED");
        assert_eq!(
            engine
                .transport
                .commands
                .iter()
                .filter(|command| command.contains("monitor reset halt"))
                .count(),
            1,
            "only startup reset is allowed after a failed compare"
        );
        assert!(
            !engine
                .take_events()
                .iter()
                .any(|event| matches!(event, SessionEvent::Loaded { .. }))
        );
    }

    fn tuple(values: &[(&str, &str)]) -> MiValue {
        MiValue::Tuple(
            values
                .iter()
                .map(|(name, value)| MiResult {
                    variable: (*name).into(),
                    value: MiValue::Const((*value).into()),
                })
                .collect(),
        )
    }

    fn list_output(name: &str, values: Vec<MiValue>) -> MiCommandOutput {
        MiCommandOutput {
            result_class: "done".into(),
            results: vec![MiResult {
                variable: name.into(),
                value: MiValue::List(values.into_iter().map(MiListItem::Value).collect()),
            }],
            records: Vec::new(),
            stopped_after_command: false,
        }
    }
}
