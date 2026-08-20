use std::{
    collections::HashSet,
    io::{BufRead, BufWriter, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::Duration,
};

use samdebug_core::{CancellationToken, ErrorCategory, SamdebugError, SamdebugResult};
use samdebug_debug::{
    FirmwareArtifact, GdbMiConfig, OpenOcdConfig, OwnedDebugSession, SessionEvent,
};
use samdebug_tools::MacUsbProbeProvider;
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const CAPABILITIES: &[&str] = &[
    "session.start",
    "session.stop",
    "target.halt",
    "target.continue",
    "target.reset",
    "target.step",
    "target.next",
    "breakpoint.insert",
    "breakpoint.remove",
    "stack.list",
    "variables.list",
    "registers.read",
    "memory.read",
    "firmware.load",
];

#[derive(Debug, Clone)]
pub struct AgentContext {
    pub openocd: OpenOcdConfig,
    pub gdb: GdbMiConfig,
    pub firmware: FirmwareArtifact,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema_version: u32,
    kind: String,
    id: Value,
    operation: String,
    payload: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartPayload {
    probe_serial: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BreakpointInsertPayload {
    location: String,
    #[serde(default)]
    temporary: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BreakpointRemovePayload {
    breakpoint_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackPayload {
    #[serde(default)]
    start: u32,
    #[serde(default = "default_levels")]
    levels: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VariablesPayload {
    #[serde(default)]
    frame: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistersPayload {
    names: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryPayload {
    address: u64,
    length: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoadPayload {
    authorization: Authorization,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Authorization {
    operation: String,
    probe_serial: String,
}

#[derive(Debug)]
enum InputLine {
    Line(String),
    TooLarge,
    Eof,
}

#[derive(Debug)]
struct ActiveSession {
    owned: OwnedDebugSession,
    watcher_stop: Arc<AtomicBool>,
    watcher: Option<thread::JoinHandle<()>>,
}

#[derive(Debug)]
struct TokenForwarder {
    stop: Arc<AtomicBool>,
    watcher: Option<thread::JoinHandle<()>>,
}

impl TokenForwarder {
    fn start(external: &CancellationToken, local: &CancellationToken) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let external = external.clone();
        let local = local.clone();
        let watcher = thread::spawn(move || {
            while !worker_stop.load(Ordering::SeqCst) {
                if external.is_cancelled() {
                    local.cancel();
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        Self {
            stop,
            watcher: Some(watcher),
        }
    }
}

impl Drop for TokenForwarder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

impl ActiveSession {
    fn new(owned: OwnedDebugSession, cancellation: &CancellationToken) -> Self {
        let controller = owned.cancellation_controller();
        let watcher_stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&watcher_stop);
        let external = cancellation.clone();
        let watcher = thread::spawn(move || {
            while !worker_stop.load(Ordering::SeqCst) {
                if external.is_cancelled() {
                    controller.cancel();
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        Self {
            owned,
            watcher_stop,
            watcher: Some(watcher),
        }
    }

    fn finish_watcher(&mut self) {
        self.watcher_stop.store(true, Ordering::SeqCst);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

impl Drop for ActiveSession {
    fn drop(&mut self) {
        self.finish_watcher();
    }
}

pub fn run(context: &AgentContext, cancellation: &CancellationToken) -> SamdebugResult<()> {
    let (sender, receiver) = mpsc::sync_channel::<InputLine>(64);
    thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut input = stdin.lock();
        loop {
            let line = read_bounded_line(&mut input);
            let finished = matches!(line, InputLine::Eof);
            if sender.send(line).is_err() || finished {
                break;
            }
        }
    });
    let stdout = std::io::stdout();
    let mut writer = BufWriter::new(stdout.lock());
    emit(
        &mut writer,
        &json!({
            "schema_version": 1,
            "kind": "event",
            "event": "hello",
            "payload": {"protocol_version": 1, "capabilities": CAPABILITIES}
        }),
    )?;
    let mut session: Option<ActiveSession> = None;
    let mut seen_ids = HashSet::new();
    loop {
        if cancellation.is_cancelled() {
            if let Some(active) = session.as_mut() {
                active.owned.cancellation_controller().cancel();
                emit_session_events(&mut writer, active.owned.take_events())?;
            }
            return Err(SamdebugError::new(
                ErrorCategory::Interrupted,
                "INTERRUPTED",
                "agent debug session interrupted",
            ));
        }
        if let Some(active) = session.as_mut() {
            emit_session_events(&mut writer, active.owned.take_events())?;
        }
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(InputLine::Line(line)) => {
                handle_line(
                    &line,
                    context,
                    cancellation,
                    &mut seen_ids,
                    &mut session,
                    &mut writer,
                )?;
            }
            Ok(InputLine::TooLarge) => {
                emit_protocol_error(
                    &mut writer,
                    "REQUEST_TOO_LARGE",
                    "NDJSON request exceeds 1 MiB",
                )?;
            }
            Ok(InputLine::Eof) => {
                if let Some(mut active) = session.take() {
                    active.finish_watcher();
                    active.owned.stop()?;
                    emit_session_events(&mut writer, active.owned.take_events())?;
                }
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

pub fn emit_fatal(error: &SamdebugError) -> SamdebugResult<()> {
    let stdout = std::io::stdout();
    let mut writer = BufWriter::new(stdout.lock());
    emit(
        &mut writer,
        &json!({
            "schema_version": 1,
            "kind": "event",
            "event": "protocol.error",
            "payload": {
                "code": error.code(),
                "message": error.to_string(),
                "exit_code": error.exit_code()
            }
        }),
    )
}

fn handle_line(
    line: &str,
    context: &AgentContext,
    cancellation: &CancellationToken,
    seen_ids: &mut HashSet<String>,
    session: &mut Option<ActiveSession>,
    writer: &mut impl Write,
) -> SamdebugResult<()> {
    if line.len() > MAX_REQUEST_BYTES {
        return emit_protocol_error(writer, "REQUEST_TOO_LARGE", "NDJSON request exceeds 1 MiB");
    }
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => return emit_protocol_error(writer, "REQUEST_INVALID", &error.to_string()),
    };
    let request: Request = match serde_json::from_value(value.clone()) {
        Ok(request) => request,
        Err(error) => {
            if let Some(request) = recover_request_identity(&value) {
                return emit_response(
                    writer,
                    &request,
                    Err(command_error("REQUEST_INVALID", error.to_string())),
                );
            }
            return emit_protocol_error(writer, "REQUEST_INVALID", &error.to_string());
        }
    };
    if request.schema_version != 1 || request.kind != "request" || !valid_id(&request.id) {
        return emit_response(
            writer,
            &request,
            Err(command_error(
                "REQUEST_INVALID",
                "request requires schema_version 1, kind request, and a string/integer id",
            )),
        );
    }
    let id_key = request_id_key(&request.id).expect("validated request id");
    if !seen_ids.insert(id_key) {
        return emit_response(
            writer,
            &request,
            Err(command_error(
                "REQUEST_ID_DUPLICATE",
                "request id was already used in this protocol session",
            )),
        );
    }
    if !CAPABILITIES.contains(&request.operation.as_str()) {
        return emit_response(
            writer,
            &request,
            Err(command_error(
                "OPERATION_UNKNOWN",
                "unknown debug operation",
            )),
        );
    }
    let result = execute(&request, context, cancellation, session, writer);
    emit_response(writer, &request, result)
}

#[allow(clippy::too_many_lines)]
fn execute(
    request: &Request,
    context: &AgentContext,
    cancellation: &CancellationToken,
    session: &mut Option<ActiveSession>,
    writer: &mut impl Write,
) -> SamdebugResult<Value> {
    match request.operation.as_str() {
        "session.start" => {
            if session.is_some() {
                return Err(command_error(
                    "INVALID_SESSION_STATE",
                    "session is already active",
                ));
            }
            let payload: StartPayload = payload(request)?;
            if payload.probe_serial.is_empty() {
                return Err(command_error(
                    "PROBE_SERIAL_INVALID",
                    "probe serial is empty",
                ));
            }
            let session_cancellation = CancellationToken::new();
            let startup_forwarder = TokenForwarder::start(cancellation, &session_cancellation);
            let mut output_error = None;
            let owned = {
                let mut sink = |event: &SessionEvent| {
                    if output_error.is_none()
                        && let Err(error) = emit_session_event(writer, event)
                    {
                        output_error = Some(error);
                    }
                };
                OwnedDebugSession::launch_with_event_sink(
                    &MacUsbProbeProvider,
                    &context.openocd,
                    &context.gdb,
                    &payload.probe_serial,
                    &context.firmware,
                    &session_cancellation,
                    &mut sink,
                )?
            };
            drop(startup_forwarder);
            if let Some(error) = output_error {
                drop(owned);
                return Err(error);
            }
            let result = json!({
                "generation": owned.generation(),
                "state": "halted",
                "probe_serial": payload.probe_serial
            });
            *session = Some(ActiveSession::new(owned, cancellation));
            Ok(result)
        }
        "session.stop" => {
            empty_payload(request)?;
            if let Some(mut active) = session.take() {
                active.finish_watcher();
                active.owned.stop()?;
                emit_session_events(writer, active.owned.take_events())?;
            }
            Ok(json!({"state": "idle"}))
        }
        "target.halt" => {
            empty_payload(request)?;
            let frame = active(session)?.halt()?;
            Ok(json!({"state": "halted", "stop_reason": "halt", "frame": frame}))
        }
        "target.continue" => {
            empty_payload(request)?;
            active(session)?.continue_target()?;
            Ok(json!({"state": "running"}))
        }
        "target.reset" => {
            let object = request.payload.as_object().ok_or_else(|| {
                command_error("PAYLOAD_INVALID", "target.reset payload must be an object")
            })?;
            if object.keys().any(|key| key != "halt")
                || object
                    .get("halt")
                    .is_some_and(|value| value != &Value::Bool(true))
            {
                return Err(command_error(
                    "PAYLOAD_INVALID",
                    "target.reset accepts only halt:true",
                ));
            }
            active(session)?.reset_halt()?;
            Ok(json!({"state": "halted", "stop_reason": "reset"}))
        }
        "target.step" => {
            empty_payload(request)?;
            let frame = active(session)?.step()?;
            Ok(json!({"state": "halted", "stop_reason": "step", "frame": frame}))
        }
        "target.next" => {
            empty_payload(request)?;
            let frame = active(session)?.next_target()?;
            Ok(json!({"state": "halted", "stop_reason": "step", "frame": frame}))
        }
        "breakpoint.insert" => {
            let payload: BreakpointInsertPayload = payload(request)?;
            let point = active(session)?.insert_breakpoint(&payload.location, payload.temporary)?;
            Ok(json!({
                "breakpoint_id": point.id,
                "location": point.location,
                "enabled": point.enabled,
                "temporary": point.temporary
            }))
        }
        "breakpoint.remove" => {
            let payload: BreakpointRemovePayload = payload(request)?;
            active(session)?.remove_breakpoint(&payload.breakpoint_id)?;
            Ok(json!({"removed_id": payload.breakpoint_id}))
        }
        "stack.list" => {
            let payload: StackPayload = payload(request)?;
            Ok(json!({"frames": active(session)?.stack_frames(payload.start, payload.levels)?}))
        }
        "variables.list" => {
            let payload: VariablesPayload = payload(request)?;
            Ok(json!({"variables": active(session)?.variables(payload.frame)?}))
        }
        "registers.read" => {
            let payload: RegistersPayload = payload(request)?;
            Ok(json!({"registers": active(session)?.registers(&payload.names)?}))
        }
        "memory.read" => {
            let payload: MemoryPayload = payload(request)?;
            let block = active(session)?.read_memory(payload.address, payload.length)?;
            Ok(json!({
                "address": block.address,
                "length": block.bytes.len(),
                "bytes_hex": encode_hex(&block.bytes)
            }))
        }
        "firmware.load" => {
            let payload: LoadPayload = payload(request)?;
            let selected = active(session)?;
            if payload.authorization.operation != "firmware.load" {
                return Err(SamdebugError::new(
                    ErrorCategory::Authorization,
                    "AUTHORIZATION_REJECTED",
                    "authorization operation must be firmware.load",
                ));
            }
            let authorization = format!("firmware.load:{}", payload.authorization.probe_serial);
            selected.load_firmware(&authorization)?;
            Ok(json!({"state": "halted", "verified": true}))
        }
        _ => unreachable!("capabilities validated"),
    }
}

fn active(session: &mut Option<ActiveSession>) -> SamdebugResult<&mut OwnedDebugSession> {
    session
        .as_mut()
        .map(|active| &mut active.owned)
        .ok_or_else(|| command_error("INVALID_SESSION_STATE", "session.start is required"))
}

fn read_bounded_line(reader: &mut impl BufRead) -> InputLine {
    let mut bytes = Vec::new();
    let mut too_large = false;
    loop {
        let Ok(available) = reader.fill_buf() else {
            return InputLine::Eof;
        };
        if available.is_empty() {
            if bytes.is_empty() && !too_large {
                return InputLine::Eof;
            }
            break;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        let content = newline.unwrap_or(available.len());
        if !too_large {
            if bytes.len().saturating_add(content) > MAX_REQUEST_BYTES {
                too_large = true;
                bytes.clear();
            } else {
                bytes.extend_from_slice(&available[..content]);
            }
        }
        reader.consume(consumed);
        if newline.is_some() {
            break;
        }
    }
    if too_large {
        InputLine::TooLarge
    } else {
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        match String::from_utf8(bytes) {
            Ok(line) => InputLine::Line(line),
            Err(_) => InputLine::Line(String::from("{")),
        }
    }
}

fn payload<T: for<'de> Deserialize<'de>>(request: &Request) -> SamdebugResult<T> {
    serde_json::from_value(request.payload.clone())
        .map_err(|error| command_error("PAYLOAD_INVALID", error.to_string()))
}

fn empty_payload(request: &Request) -> SamdebugResult<()> {
    let object = request
        .payload
        .as_object()
        .ok_or_else(|| command_error("PAYLOAD_INVALID", "payload must be an object"))?;
    if object.is_empty() {
        Ok(())
    } else {
        Err(command_error("PAYLOAD_INVALID", "payload must be empty"))
    }
}

fn valid_id(id: &Value) -> bool {
    id.is_string() || id.as_i64().is_some() || id.as_u64().is_some()
}

fn request_id_key(id: &Value) -> Option<String> {
    if let Some(value) = id.as_str() {
        Some(format!("s:{value}"))
    } else if let Some(value) = id.as_i64() {
        Some(format!("i:{value}"))
    } else {
        id.as_u64().map(|value| format!("u:{value}"))
    }
}

fn recover_request_identity(value: &Value) -> Option<Request> {
    let object = value.as_object()?;
    let id = object.get("id")?.clone();
    if !valid_id(&id) {
        return None;
    }
    let operation = object.get("operation")?.as_str()?;
    if operation.is_empty() {
        return None;
    }
    Some(Request {
        schema_version: object
            .get("schema_version")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or_default(),
        kind: object
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        id,
        operation: operation.to_owned(),
        payload: object.get("payload").cloned().unwrap_or_else(|| json!({})),
    })
}

fn emit_response(
    writer: &mut impl Write,
    request: &Request,
    result: SamdebugResult<Value>,
) -> SamdebugResult<()> {
    let value = match result {
        Ok(result) => json!({
            "schema_version": 1, "kind": "response", "id": request.id,
            "operation": request.operation, "ok": true, "result": result
        }),
        Err(error) => json!({
            "schema_version": 1, "kind": "response", "id": request.id,
            "operation": request.operation, "ok": false, "error": error
        }),
    };
    emit(writer, &value)
}

fn emit_protocol_error(writer: &mut impl Write, code: &str, message: &str) -> SamdebugResult<()> {
    emit(
        writer,
        &json!({
            "schema_version": 1,
            "kind": "event",
            "event": "protocol.error",
            "payload": {"code": code, "message": message}
        }),
    )
}

fn emit_session_events(writer: &mut impl Write, events: Vec<SessionEvent>) -> SamdebugResult<()> {
    for event in events {
        emit_session_event(writer, &event)?;
    }
    Ok(())
}

fn emit_session_event(writer: &mut impl Write, event: &SessionEvent) -> SamdebugResult<()> {
    let serialized = serde_json::to_value(event).map_err(protocol_io_error)?;
    let raw_name = serialized
        .get("event")
        .and_then(Value::as_str)
        .ok_or_else(|| command_error("EVENT_SERIALIZATION_FAILED", "event name missing"))?;
    let name = match raw_name {
        "probe_selected" => "probe.selected",
        "server_starting" => "server.starting",
        "server_ready" => "server.ready",
        "gdb_starting" => "gdb.starting",
        "target_output" => "target.output",
        "session_error" => "session.error",
        "session_stopped" => "session.stopped",
        value => value,
    };
    let mut payload = serialized
        .get("payload")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if name == "log" {
        payload
            .as_object_mut()
            .expect("SessionEvent payload is an object")
            .insert("level".into(), Value::String("info".into()));
    }
    emit(
        writer,
        &json!({"schema_version": 1, "kind": "event", "event": name, "payload": payload}),
    )
}

fn emit(writer: &mut impl Write, value: &Value) -> SamdebugResult<()> {
    serde_json::to_writer(&mut *writer, value).map_err(protocol_io_error)?;
    writer.write_all(b"\n").map_err(protocol_io_error)?;
    writer.flush().map_err(protocol_io_error)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

const fn default_levels() -> u32 {
    32
}

fn command_error(code: &str, message: impl Into<String>) -> SamdebugError {
    SamdebugError::new(ErrorCategory::Command, code, message)
}

fn protocol_io_error(error: impl std::fmt::Display) -> SamdebugError {
    SamdebugError::new(
        ErrorCategory::Debugger,
        "AGENT_IO_FAILED",
        error.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_and_unknown_requests_emit_one_ndjson_message() {
        let mut output = Vec::new();
        emit_protocol_error(&mut output, "REQUEST_INVALID", "bad").expect("emit");
        let lines = String::from_utf8(output).expect("utf8");
        assert_eq!(lines.lines().count(), 1);
        let value: Value = serde_json::from_str(lines.trim()).expect("json");
        assert_eq!(value["event"], "protocol.error");
    }

    #[test]
    fn session_events_map_to_schema_names_and_log_level() {
        let mut output = Vec::new();
        emit_session_event(
            &mut output,
            &SessionEvent::Log {
                generation: 3,
                source: "gdb".into(),
                text: "ready".into(),
            },
        )
        .expect("emit");
        let value: Value = serde_json::from_slice(&output).expect("json");
        assert_eq!(value["event"], "log");
        assert_eq!(value["payload"]["level"], "info");
    }

    #[test]
    fn request_validation_rejects_float_ids_and_nonempty_empty_payloads() {
        assert!(!valid_id(&json!(1.5)));
        assert_ne!(request_id_key(&json!("1")), request_id_key(&json!(1)));
        let request = Request {
            schema_version: 1,
            kind: "request".into(),
            id: json!(1),
            operation: "session.stop".into(),
            payload: json!({"unexpected": true}),
        };
        assert_eq!(
            empty_payload(&request).unwrap_err().code(),
            "PAYLOAD_INVALID"
        );
    }

    #[test]
    fn malformed_object_with_identity_gets_correlated_error_response() {
        let value = json!({
            "schema_version": 1,
            "kind": "request",
            "id": "abc",
            "operation": "target.halt",
            "payload": {},
            "unknown": true
        });
        let request = recover_request_identity(&value).expect("recover identity");
        let mut output = Vec::new();
        emit_response(
            &mut output,
            &request,
            Err(command_error("REQUEST_INVALID", "unknown field")),
        )
        .expect("emit response");
        let response: Value = serde_json::from_slice(&output).expect("response JSON");
        assert_eq!(response["id"], "abc");
        assert_eq!(response["ok"], false);
    }

    #[test]
    fn bounded_reader_rejects_complete_oversized_lines_and_recovers() {
        let mut input = vec![b'x'; MAX_REQUEST_BYTES + 1];
        input.extend_from_slice(b"\n{}\n");
        let mut reader = std::io::BufReader::with_capacity(17, input.as_slice());
        assert!(matches!(
            read_bounded_line(&mut reader),
            InputLine::TooLarge
        ));
        assert!(matches!(
            read_bounded_line(&mut reader),
            InputLine::Line(line) if line == "{}"
        ));
    }
}
