use std::{
    fs,
    net::{Ipv4Addr, SocketAddrV4, TcpListener},
    path::{Path, PathBuf},
    time::Duration,
};

use samdebug_core::{
    CancellationToken, ErrorCategory, SamdebugError, SamdebugResult,
    ports::{CommandOutput, CommandSpec, ProbeProvider, ProcessRunner},
};
use serde::Serialize;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
const PORT_BIND_ATTEMPTS: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenOcdConfig {
    pub executable: PathBuf,
    pub scripts: PathBuf,
    pub speed_khz: u32,
    pub timeout: Duration,
}

impl OpenOcdConfig {
    #[must_use]
    pub fn new(executable: PathBuf, scripts: PathBuf, speed_khz: u32) -> Self {
        Self {
            executable,
            scripts,
            speed_khz,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProbeRecord {
    pub serial: String,
    pub product: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProbeListReport {
    pub probes: Vec<ProbeRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareArtifact {
    pub path: PathBuf,
    pub build_directory: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramOperation {
    Erase,
    Flash,
    Reset,
    Halt,
}

impl ProgramOperation {
    const fn name(self) -> &'static str {
        match self {
            Self::Erase => "erase",
            Self::Flash => "flash",
            Self::Reset => "reset",
            Self::Halt => "halt",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProgrammingReport {
    pub operation: String,
    pub probe_serial: String,
    pub verification: String,
    pub reset: String,
    pub target_voltage: Option<String>,
    pub openocd_log: String,
}

pub fn list_probes(provider: &dyn ProbeProvider) -> SamdebugResult<ProbeListReport> {
    let mut probes = provider.list()?;
    probes.sort_by(|left, right| left.serial.cmp(&right.serial));
    probes.dedup_by(|left, right| left.serial != "unknown" && left.serial == right.serial);
    Ok(ProbeListReport {
        probes: probes
            .into_iter()
            .map(|probe| ProbeRecord {
                serial: probe.serial,
                product: probe.product,
            })
            .collect(),
    })
}

#[derive(Debug)]
pub struct OpenOcdProgrammer<'a> {
    probes: &'a dyn ProbeProvider,
    runner: &'a dyn ProcessRunner,
    ports: &'a dyn PortProvider,
    config: OpenOcdConfig,
}

static DYNAMIC_PORT_PROVIDER: DynamicPortProvider = DynamicPortProvider;

impl<'a> OpenOcdProgrammer<'a> {
    #[must_use]
    pub const fn new(
        probes: &'a dyn ProbeProvider,
        runner: &'a dyn ProcessRunner,
        config: OpenOcdConfig,
    ) -> Self {
        Self {
            probes,
            runner,
            ports: &DYNAMIC_PORT_PROVIDER,
            config,
        }
    }

    #[cfg(test)]
    const fn with_ports(
        probes: &'a dyn ProbeProvider,
        runner: &'a dyn ProcessRunner,
        ports: &'a dyn PortProvider,
        config: OpenOcdConfig,
    ) -> Self {
        Self {
            probes,
            runner,
            ports,
            config,
        }
    }

    pub fn execute(
        &self,
        operation: ProgramOperation,
        requested_serial: &str,
        authorization: Option<&str>,
        elf: Option<&FirmwareArtifact>,
        cancellation: &CancellationToken,
    ) -> SamdebugResult<ProgrammingReport> {
        validate_config(&self.config)?;
        let selected = select_probe(self.probes, requested_serial)?;
        if matches!(operation, ProgramOperation::Erase | ProgramOperation::Flash) {
            validate_authorization(operation, &selected.serial, authorization)?;
        }
        let elf = match operation {
            ProgramOperation::Flash => Some(validate_elf(elf)?),
            _ => None,
        };
        for attempt in 1..=PORT_BIND_ATTEMPTS {
            let ports = self.ports.reserve()?;
            let args = build_args(
                &self.config,
                &selected.serial,
                operation,
                elf.as_deref(),
                ports,
            );
            let output = self.runner.run_cancellable_with_timeout(
                &CommandSpec {
                    program: self.config.executable.to_string_lossy().into_owned(),
                    args,
                    current_dir: None,
                },
                cancellation,
                self.config.timeout,
            );
            let output = output.map_err(|error| {
                if error.code() == "PROCESS_TIMEOUT" {
                    operation_timeout(operation)
                } else {
                    error
                }
            })?;
            let lower = output_text(&output).to_ascii_lowercase();
            if output.exit_code != Some(0) && is_probe_transport_failure(&lower) {
                return classify_output(operation, &selected.serial, &output);
            }
            if is_port_bind_failure(&output) {
                if attempt < PORT_BIND_ATTEMPTS {
                    continue;
                }
                return Err(SamdebugError::new(
                    ErrorCategory::Connection,
                    "LOCAL_PORT_UNAVAILABLE",
                    format!(
                        "OpenOCD could not bind fresh loopback ports after {PORT_BIND_ATTEMPTS} attempts"
                    ),
                )
                .with_details(serde_json::json!({"openocd_log": output_text(&output).trim()})));
            }
            return classify_output(operation, &selected.serial, &output);
        }
        unreachable!("the bounded port-attempt loop always returns")
    }
}

fn operation_timeout(operation: ProgramOperation) -> SamdebugError {
    match operation {
        ProgramOperation::Erase | ProgramOperation::Flash => SamdebugError::new(
            ErrorCategory::Programming,
            if operation == ProgramOperation::Erase {
                "ERASE_TIMEOUT"
            } else {
                "FLASH_TIMEOUT"
            },
            format!(
                "{} timed out; the destructive operation may be partial and target firmware state is unknown",
                operation.name()
            ),
        ),
        ProgramOperation::Reset | ProgramOperation::Halt => SamdebugError::new(
            ErrorCategory::Connection,
            "TARGET_COMMAND_TIMEOUT",
            format!("{} timed out before OpenOCD completed", operation.name()),
        ),
    }
}

fn validate_config(config: &OpenOcdConfig) -> SamdebugResult<()> {
    if !config.executable.is_absolute()
        || !config.executable.is_file()
        || !config.scripts.is_absolute()
        || !config.scripts.is_dir()
        || !config.scripts.join("interface/cmsis-dap.cfg").is_file()
        || !config.scripts.join("target/at91sam4sXX.cfg").is_file()
        || config.speed_khz == 0
    {
        return Err(SamdebugError::new(
            ErrorCategory::Tool,
            "OPENOCD_CONFIGURATION_INVALID",
            "OpenOCD executable, scripts, or adapter speed is invalid",
        ));
    }
    Ok(())
}

fn select_probe(provider: &dyn ProbeProvider, serial: &str) -> SamdebugResult<ProbeRecord> {
    if serial.is_empty() || serial.chars().any(char::is_control) {
        return Err(SamdebugError::new(
            ErrorCategory::Connection,
            "PROBE_SERIAL_INVALID",
            "probe serial must be a non-empty printable value",
        ));
    }
    let report = list_probes(provider)?;
    if report.probes.is_empty() {
        return Err(SamdebugError::new(
            ErrorCategory::Connection,
            "PROBE_NOT_FOUND",
            "no Atmel-ICE probe is connected",
        ));
    }
    if serial == "unknown" {
        return Err(SamdebugError::new(
            ErrorCategory::Connection,
            if report.probes.len() > 1 {
                "MULTIPLE_PROBES"
            } else {
                "PROBE_SERIAL_UNAVAILABLE"
            },
            "Atmel-ICE must expose a unique serial for programming",
        ));
    }
    report
        .probes
        .into_iter()
        .find(|probe| probe.serial == serial)
        .ok_or_else(|| {
            SamdebugError::new(
                ErrorCategory::Connection,
                "PROBE_SERIAL_NOT_FOUND",
                format!("Atmel-ICE probe {serial} was not found"),
            )
        })
}

fn validate_authorization(
    operation: ProgramOperation,
    serial: &str,
    authorization: Option<&str>,
) -> SamdebugResult<()> {
    let expected = format!("{}:{serial}", operation.name());
    if authorization == Some(expected.as_str()) {
        Ok(())
    } else {
        Err(SamdebugError::new(
            ErrorCategory::Authorization,
            "AUTHORIZATION_REJECTED",
            format!("expected authorization {expected}"),
        ))
    }
}

fn validate_elf(elf: Option<&FirmwareArtifact>) -> SamdebugResult<PathBuf> {
    let elf = elf.ok_or_else(|| {
        SamdebugError::new(
            ErrorCategory::Programming,
            "FIRMWARE_ARTIFACT_MISSING",
            "the configured ELF artifact is unavailable",
        )
    })?;
    let metadata = fs::symlink_metadata(&elf.path).map_err(|error| {
        SamdebugError::new(
            ErrorCategory::Programming,
            "FIRMWARE_ARTIFACT_MISSING",
            format!("{}: {error}", elf.path.display()),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
        return Err(SamdebugError::new(
            ErrorCategory::Programming,
            "FIRMWARE_ARTIFACT_INVALID",
            "the configured ELF must be a non-empty regular file",
        ));
    }
    let canonical_build = elf.build_directory.canonicalize().map_err(|error| {
        SamdebugError::new(
            ErrorCategory::Programming,
            "FIRMWARE_ARTIFACT_INVALID",
            format!("managed build directory is invalid: {error}"),
        )
    })?;
    let managed_shape = canonical_build
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "Debug" | "Release"))
        && canonical_build
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "build")
        && canonical_build
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .is_some_and(|name| name == ".samdebug");
    if !managed_shape {
        return Err(SamdebugError::new(
            ErrorCategory::Programming,
            "FIRMWARE_ARTIFACT_ESCAPE",
            "firmware must come from the selected managed build directory",
        ));
    }
    let canonical = elf.path.canonicalize().map_err(|error| {
        SamdebugError::new(
            ErrorCategory::Programming,
            "FIRMWARE_ARTIFACT_INVALID",
            error.to_string(),
        )
    })?;
    if canonical.parent() != Some(canonical_build.as_path()) {
        return Err(SamdebugError::new(
            ErrorCategory::Programming,
            "FIRMWARE_ARTIFACT_ESCAPE",
            "firmware must be a direct child of the selected managed build directory",
        ));
    }
    Ok(canonical)
}

#[derive(Debug, Clone, Copy)]
struct DynamicPorts {
    gdb: u16,
    tcl: u16,
    telnet: u16,
}

trait PortProvider: std::fmt::Debug + Send + Sync {
    fn reserve(&self) -> SamdebugResult<DynamicPorts>;
}

#[derive(Debug)]
struct DynamicPortProvider;

impl PortProvider for DynamicPortProvider {
    fn reserve(&self) -> SamdebugResult<DynamicPorts> {
        let mut listeners = Vec::new();
        let mut values = Vec::new();
        for _ in 0..3 {
            let listener =
                TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).map_err(|error| {
                    SamdebugError::new(
                        ErrorCategory::Connection,
                        "LOCAL_PORT_UNAVAILABLE",
                        error.to_string(),
                    )
                })?;
            values.push(
                listener
                    .local_addr()
                    .map_err(|error| {
                        SamdebugError::new(
                            ErrorCategory::Connection,
                            "LOCAL_PORT_UNAVAILABLE",
                            error.to_string(),
                        )
                    })?
                    .port(),
            );
            listeners.push(listener);
        }
        drop(listeners);
        Ok(DynamicPorts {
            gdb: values[0],
            tcl: values[1],
            telnet: values[2],
        })
    }
}

fn build_args(
    config: &OpenOcdConfig,
    serial: &str,
    operation: ProgramOperation,
    elf: Option<&Path>,
    ports: DynamicPorts,
) -> Vec<String> {
    let mut args = vec![
        "-s".into(),
        config.scripts.to_string_lossy().into_owned(),
        "-c".into(),
        "bindto 127.0.0.1".into(),
        "-c".into(),
        format!("gdb_port {}", ports.gdb),
        "-c".into(),
        format!("tcl_port {}", ports.tcl),
        "-c".into(),
        format!("telnet_port {}", ports.telnet),
        "-f".into(),
        "interface/cmsis-dap.cfg".into(),
        "-c".into(),
        "cmsis_dap_backend hid".into(),
        "-c".into(),
        format!("adapter serial {}", command_quote(serial)),
        "-c".into(),
        "transport select swd".into(),
        "-f".into(),
        "target/at91sam4sXX.cfg".into(),
        "-c".into(),
        format!("adapter speed {}", config.speed_khz),
        "-c".into(),
        "init".into(),
    ];
    let commands: Vec<String> = match operation {
        ProgramOperation::Erase => vec![
            "reset halt".into(),
            "flash erase_sector 0 0 last".into(),
            "flash erase_check 0".into(),
            "reset halt".into(),
        ],
        ProgramOperation::Flash => {
            let elf = command_quote(&elf.expect("flash validates ELF").to_string_lossy());
            vec![
                "reset halt".into(),
                format!("flash write_image erase {elf}"),
                format!("verify_image {elf}"),
                "reset run".into(),
            ]
        }
        ProgramOperation::Reset => vec!["reset run".into()],
        ProgramOperation::Halt => vec!["halt".into()],
    };
    for command in commands {
        args.extend(["-c".into(), command]);
    }
    args.extend(["-c".into(), "shutdown".into()]);
    args
}

fn command_quote(value: &str) -> String {
    let mut quoted = String::from('"');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '$' => quoted.push_str("\\$"),
            '[' => quoted.push_str("\\["),
            ']' => quoted.push_str("\\]"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            value => quoted.push(value),
        }
    }
    quoted.push('"');
    quoted
}

fn output_text(output: &CommandOutput) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn is_port_bind_failure(output: &CommandOutput) -> bool {
    if output.exit_code == Some(0) {
        return false;
    }
    let lower = output_text(output).to_ascii_lowercase();
    lower.contains("address already in use")
        || lower.contains("couldn't bind")
        || lower.contains("cannot bind")
        || lower.contains("failed to bind")
        || lower.contains("bind failed")
}

pub(crate) fn is_probe_transport_failure(lower: &str) -> bool {
    lower.lines().any(|line| {
        let cmsis_dap_failure = line.contains("cmsis-dap")
            && (line.contains("not found")
                || line.contains("unable to find")
                || (line.contains("command") && line.contains("failed"))
                || line.contains("command mismatch")
                || line.contains("transfer count mismatch")
                || line.contains("protocol error")
                || line.contains("interface reset failed"));
        let command_transport_failure =
            (line.contains("cmd_") || line.contains("swd_sequence")) && line.contains("failed");
        cmsis_dap_failure
            || command_transport_failure
            || line.contains("usb is disconnected")
            || line.contains("usb read error")
            || line.contains("usb write error")
            || line.contains("usb device discovery failed")
            || line.contains("hid read error")
            || line.contains("hid write error")
            || line.contains("hid read timed out")
            || line.contains("hid write timed out")
            || line.contains("hid write returned")
            || line.contains("libusb_bulk_read error")
            || line.contains("libusb_bulk_write error")
            || line.contains("bulk read failed")
            || line.contains("bulk write failed")
            || line.contains("bulk transfer failed")
    })
}

fn classify_output(
    operation: ProgramOperation,
    serial: &str,
    output: &CommandOutput,
) -> SamdebugResult<ProgrammingReport> {
    let text = output_text(output);
    let lower = text.to_ascii_lowercase();
    if output.exit_code != Some(0) {
        let (category, code, message) = if is_probe_transport_failure(&lower) {
            (
                ErrorCategory::Connection,
                "PROBE_DISCONNECTED",
                "Atmel-ICE disconnected",
            )
        } else if lower.contains("verify")
            && (lower.contains("failed") || lower.contains("mismatch"))
        {
            (
                ErrorCategory::Programming,
                "VERIFY_FAILED",
                "firmware verification failed",
            )
        } else if lower.contains("target voltage")
            || lower.contains("vtarget = 0")
            || lower.contains("vtref")
        {
            (
                ErrorCategory::Connection,
                "TARGET_POWER_MISSING",
                "target reference voltage is absent",
            )
        } else if lower.contains("locked") || lower.contains("security bit") {
            (
                ErrorCategory::Connection,
                "TARGET_LOCKED",
                "target is locked",
            )
        } else if lower.contains("unable to connect") || lower.contains("target examination failed")
        {
            (
                ErrorCategory::Connection,
                "TARGET_UNREACHABLE",
                "ATSAM4SD32C is unreachable",
            )
        } else if operation == ProgramOperation::Erase {
            (
                ErrorCategory::Programming,
                "ERASE_FAILED",
                "chip erase failed",
            )
        } else if operation == ProgramOperation::Flash {
            (
                ErrorCategory::Programming,
                "FLASH_FAILED",
                "firmware programming failed",
            )
        } else {
            (
                ErrorCategory::Connection,
                "TARGET_COMMAND_FAILED",
                "OpenOCD target command failed",
            )
        };
        return Err(SamdebugError::new(category, code, message)
            .with_details(serde_json::json!({"openocd_log": text.trim()})));
    }
    if operation == ProgramOperation::Flash
        && (lower.contains("verify failed")
            || lower.contains("contents differ")
            || lower.contains("mismatch"))
    {
        return Err(SamdebugError::new(
            ErrorCategory::Programming,
            "VERIFY_FAILED",
            "firmware verification failed",
        ));
    }
    if operation == ProgramOperation::Erase && lower.contains("not erased") {
        return Err(SamdebugError::new(
            ErrorCategory::Programming,
            "ERASE_VERIFICATION_FAILED",
            "OpenOCD reported a non-erased flash sector",
        ));
    }
    Ok(ProgrammingReport {
        operation: operation.name().into(),
        probe_serial: serial.into(),
        verification: match operation {
            ProgramOperation::Erase | ProgramOperation::Flash => "verified".into(),
            ProgramOperation::Reset | ProgramOperation::Halt => "not_applicable".into(),
        },
        reset: match operation {
            ProgramOperation::Flash | ProgramOperation::Reset => "running".into(),
            ProgramOperation::Erase | ProgramOperation::Halt => "halted".into(),
        },
        target_voltage: parse_voltage(&text),
        openocd_log: text.trim().into(),
    })
}

fn parse_voltage(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        lower
            .find("vtarget =")
            .map(|index| line[index + "vtarget =".len()..].trim().to_owned())
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, path::Path, sync::Mutex};

    use samdebug_core::{
        SamdebugResult,
        ports::{
            CommandOutput, CommandSpec, ManagedChild, ProbeInfo, ProbeProvider, ProcessRunner,
        },
    };

    use super::*;

    #[derive(Debug)]
    struct FakeProbes(Vec<ProbeInfo>);

    impl ProbeProvider for FakeProbes {
        fn list(&self) -> SamdebugResult<Vec<ProbeInfo>> {
            Ok(self.0.clone())
        }
    }

    #[derive(Debug)]
    struct FakeRunner {
        calls: Mutex<Vec<CommandSpec>>,
        output: CommandOutput,
    }

    #[derive(Debug)]
    struct FixedPorts;

    impl PortProvider for FixedPorts {
        fn reserve(&self) -> SamdebugResult<DynamicPorts> {
            Ok(DynamicPorts {
                gdb: 41_001,
                tcl: 41_002,
                telnet: 41_003,
            })
        }
    }

    #[derive(Debug)]
    struct SequentialPorts(Mutex<u16>);

    impl PortProvider for SequentialPorts {
        fn reserve(&self) -> SamdebugResult<DynamicPorts> {
            let mut base = self.0.lock().expect("port sequence");
            let ports = DynamicPorts {
                gdb: *base,
                tcl: *base + 1,
                telnet: *base + 2,
            };
            *base += 10;
            Ok(ports)
        }
    }

    #[derive(Debug)]
    struct ScriptedRunner {
        calls: Mutex<Vec<CommandSpec>>,
        outputs: Mutex<VecDeque<CommandOutput>>,
    }

    impl ProcessRunner for ScriptedRunner {
        fn run(&self, command: &CommandSpec) -> SamdebugResult<CommandOutput> {
            self.calls.lock().expect("calls").push(command.clone());
            Ok(self
                .outputs
                .lock()
                .expect("outputs")
                .pop_front()
                .expect("scripted output"))
        }

        fn spawn(&self, _command: &CommandSpec) -> SamdebugResult<Box<dyn ManagedChild>> {
            unreachable!()
        }
    }

    #[derive(Debug)]
    struct TimeoutRunner;

    impl ProcessRunner for TimeoutRunner {
        fn run(&self, _command: &CommandSpec) -> SamdebugResult<CommandOutput> {
            Err(SamdebugError::new(
                ErrorCategory::Tool,
                "PROCESS_TIMEOUT",
                "finite process timed out",
            ))
        }

        fn spawn(&self, _command: &CommandSpec) -> SamdebugResult<Box<dyn ManagedChild>> {
            unreachable!()
        }
    }

    impl FakeRunner {
        fn success() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                output: CommandOutput {
                    exit_code: Some(0),
                    stdout: Vec::new(),
                    stderr: b"Info : VTarget = 3.30 V\n".to_vec(),
                },
            }
        }
    }

    impl ProcessRunner for FakeRunner {
        fn run(&self, command: &CommandSpec) -> SamdebugResult<CommandOutput> {
            self.calls.lock().expect("calls").push(command.clone());
            Ok(self.output.clone())
        }

        fn spawn(&self, _command: &CommandSpec) -> SamdebugResult<Box<dyn ManagedChild>> {
            unreachable!()
        }
    }

    fn fixture() -> (tempfile::TempDir, OpenOcdConfig) {
        let temp = tempfile::tempdir().expect("tempdir");
        let executable = temp.path().join("openocd");
        let scripts = temp.path().join("scripts");
        fs::create_dir_all(scripts.join("interface")).expect("interface scripts");
        fs::create_dir_all(scripts.join("target")).expect("target scripts");
        fs::write(&executable, b"tool").expect("openocd");
        fs::write(scripts.join("interface/cmsis-dap.cfg"), b"interface").expect("interface");
        fs::write(scripts.join("target/at91sam4sXX.cfg"), b"target").expect("target");
        let config = OpenOcdConfig::new(executable, scripts, 1_000);
        (temp, config)
    }

    fn probes() -> FakeProbes {
        FakeProbes(vec![ProbeInfo {
            serial: "ATML123".into(),
            product: "Atmel-ICE".into(),
        }])
    }

    #[test]
    fn lists_sorts_and_deduplicates_probes() {
        let report = list_probes(&FakeProbes(vec![
            ProbeInfo {
                serial: "B".into(),
                product: "Atmel-ICE".into(),
            },
            ProbeInfo {
                serial: "A".into(),
                product: "Atmel-ICE".into(),
            },
            ProbeInfo {
                serial: "A".into(),
                product: "duplicate".into(),
            },
        ]))
        .expect("list probes");
        assert_eq!(
            report
                .probes
                .iter()
                .map(|probe| probe.serial.as_str())
                .collect::<Vec<_>>(),
            ["A", "B"]
        );
    }

    #[test]
    fn selection_precedes_probe_scoped_authorization() {
        let (_temp, config) = fixture();
        let runner = FakeRunner::success();
        let probes = probes();
        let programmer = OpenOcdProgrammer::new(&probes, &runner, config);
        let wrong_probe = programmer
            .execute(
                ProgramOperation::Erase,
                "OTHER",
                Some("erase:OTHER"),
                None,
                &CancellationToken::new(),
            )
            .expect_err("unknown probe");
        assert_eq!(wrong_probe.code(), "PROBE_SERIAL_NOT_FOUND");
        let wrong_auth = programmer
            .execute(
                ProgramOperation::Erase,
                "ATML123",
                Some("flash:ATML123"),
                None,
                &CancellationToken::new(),
            )
            .expect_err("wrong authorization");
        assert_eq!(wrong_auth.code(), "AUTHORIZATION_REJECTED");
        assert!(runner.calls.lock().expect("calls").is_empty());

        let unknown = FakeProbes(vec![
            ProbeInfo {
                serial: "unknown".into(),
                product: "Atmel-ICE".into(),
            },
            ProbeInfo {
                serial: "unknown".into(),
                product: "Atmel-ICE".into(),
            },
        ]);
        let programmer = OpenOcdProgrammer::new(&unknown, &runner, programmer.config.clone());
        let error = programmer
            .execute(
                ProgramOperation::Erase,
                "unknown",
                Some("erase:unknown"),
                None,
                &CancellationToken::new(),
            )
            .expect_err("ambiguous probes");
        assert_eq!(error.code(), "MULTIPLE_PROBES");
    }

    #[test]
    fn flash_uses_hid_swd_dynamic_local_ports_elf_and_separate_verification() {
        let (temp, config) = fixture();
        let build_directory = temp.path().join(".samdebug/build/Debug");
        fs::create_dir_all(&build_directory).expect("managed build directory");
        let elf = build_directory.join("firmware with spaces.elf");
        fs::write(&elf, b"ELF").expect("ELF");
        let artifact = FirmwareArtifact {
            path: elf,
            build_directory,
        };
        let runner = FakeRunner::success();
        let report = OpenOcdProgrammer::with_ports(&probes(), &runner, &FixedPorts, config)
            .execute(
                ProgramOperation::Flash,
                "ATML123",
                Some("flash:ATML123"),
                Some(&artifact),
                &CancellationToken::new(),
            )
            .expect("flash");
        assert_eq!(report.verification, "verified");
        assert_eq!(report.target_voltage.as_deref(), Some("3.30 V"));
        let calls = runner.calls.lock().expect("calls");
        let args = &calls[0].args;
        for required in [
            "bindto 127.0.0.1",
            "interface/cmsis-dap.cfg",
            "cmsis_dap_backend hid",
            "transport select swd",
            "target/at91sam4sXX.cfg",
            "adapter speed 1000",
        ] {
            assert!(
                args.iter().any(|argument| argument == required),
                "{required}"
            );
        }
        assert!(
            args.iter()
                .any(|argument| argument.starts_with("gdb_port ") && !argument.ends_with(" 3333"))
        );
        assert!(args.iter().any(
            |argument| argument.starts_with("flash write_image erase \"")
                && argument.contains("firmware with spaces.elf")
        ));
        assert!(
            args.iter()
                .any(|argument| argument.starts_with("verify_image \"")
                    && argument.contains("firmware with spaces.elf"))
        );
        assert!(
            !args
                .iter()
                .any(|argument| argument.contains("write_memory"))
        );
    }

    #[test]
    fn flash_rejects_an_elf_outside_the_selected_managed_build_directory() {
        let (temp, config) = fixture();
        let build_directory = temp.path().join(".samdebug/build/Debug");
        fs::create_dir_all(&build_directory).expect("managed build directory");
        let outside = temp.path().join("outside.elf");
        fs::write(&outside, b"ELF").expect("outside ELF");
        let artifact = FirmwareArtifact {
            path: outside,
            build_directory,
        };
        let runner = FakeRunner::success();
        let error = OpenOcdProgrammer::new(&probes(), &runner, config)
            .execute(
                ProgramOperation::Flash,
                "ATML123",
                Some("flash:ATML123"),
                Some(&artifact),
                &CancellationToken::new(),
            )
            .expect_err("reject unmanaged ELF");
        assert_eq!(error.code(), "FIRMWARE_ARTIFACT_ESCAPE");
        assert!(runner.calls.lock().expect("calls").is_empty());
    }

    #[test]
    fn classifies_connection_programming_and_erase_verification_failures() {
        for (operation, log, expected) in [
            (
                ProgramOperation::Flash,
                "Error: target voltage too low",
                "TARGET_POWER_MISSING",
            ),
            (
                ProgramOperation::Flash,
                "Error: target examination failed",
                "TARGET_UNREACHABLE",
            ),
            (
                ProgramOperation::Flash,
                "Error: verify failed mismatch",
                "VERIFY_FAILED",
            ),
            (
                ProgramOperation::Erase,
                "Error: security bit locked",
                "TARGET_LOCKED",
            ),
        ] {
            let error = classify_output(
                operation,
                "ATML123",
                &CommandOutput {
                    exit_code: Some(1),
                    stdout: Vec::new(),
                    stderr: log.as_bytes().to_vec(),
                },
            )
            .expect_err("classified error");
            assert_eq!(error.code(), expected);
        }
        let error = classify_output(
            ProgramOperation::Erase,
            "ATML123",
            &CommandOutput {
                exit_code: Some(0),
                stdout: b"sector 7 not erased\n".to_vec(),
                stderr: Vec::new(),
            },
        )
        .expect_err("erase verification");
        assert_eq!(error.code(), "ERASE_VERIFICATION_FAILED");
    }

    #[test]
    fn pinned_openocd_transport_vocabulary_maps_to_probe_disconnected() {
        for log in [
            "Error: unable to find CMSIS-DAP device",
            "Error: USB is disconnected",
            "Error: USB read error: LIBUSB_ERROR_NO_DEVICE",
            "Error: USB write error: LIBUSB_ERROR_NO_DEVICE",
            "Error: USB device discovery failed",
            "Error: HID read error The device is not connected",
            "Error: HID read timed out",
            "Error: HID write returned -1",
            "Error: CMSIS-DAP command failed.",
            "Error: CMSIS-DAP command CMD_CONNECT failed.",
            "Error: CMD_DAP_SWJ_CLOCK failed.",
            "Error: CMD_SWD_Configure failed.",
            "Error: SWD_Sequence failed.",
            "Error: CMSIS-DAP command mismatch",
            "Error: CMSIS-DAP transfer count mismatch",
            "Error: CMSIS-DAP Protocol Error",
            "Error: CMSIS-DAP: Interface reset failed",
            "Error: libusb_bulk_read error -4",
            "Error: libusb_bulk_write error -4",
            "Error: verify failed after USB is disconnected",
        ] {
            let error = classify_output(
                ProgramOperation::Flash,
                "ATML123",
                &CommandOutput {
                    exit_code: Some(1),
                    stdout: Vec::new(),
                    stderr: log.as_bytes().to_vec(),
                },
            )
            .expect_err("transport error");
            assert_eq!(error.code(), "PROBE_DISCONNECTED", "{log}");
            assert_eq!(error.exit_code(), 5, "{log}");
        }
    }

    #[test]
    fn retries_bind_collisions_with_fresh_ports_and_reports_exhaustion() {
        let (_temp, config) = fixture();
        let collision = || CommandOutput {
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: b"Error: couldn't bind tcl socket: Address already in use\n".to_vec(),
        };
        let runner = ScriptedRunner {
            calls: Mutex::new(Vec::new()),
            outputs: Mutex::new(VecDeque::from([
                collision(),
                CommandOutput {
                    exit_code: Some(0),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                },
            ])),
        };
        let ports = SequentialPorts(Mutex::new(41_000));
        OpenOcdProgrammer::with_ports(&probes(), &runner, &ports, config.clone())
            .execute(
                ProgramOperation::Reset,
                "ATML123",
                None,
                None,
                &CancellationToken::new(),
            )
            .expect("second allocation succeeds");
        let calls = runner.calls.lock().expect("calls");
        assert_eq!(calls.len(), 2);
        assert!(calls[0].args.contains(&"gdb_port 41000".to_owned()));
        assert!(calls[1].args.contains(&"gdb_port 41010".to_owned()));
        drop(calls);

        let exhausted = ScriptedRunner {
            calls: Mutex::new(Vec::new()),
            outputs: Mutex::new(VecDeque::from([
                collision(),
                collision(),
                collision(),
                collision(),
            ])),
        };
        let error = OpenOcdProgrammer::with_ports(&probes(), &exhausted, &ports, config)
            .execute(
                ProgramOperation::Reset,
                "ATML123",
                None,
                None,
                &CancellationToken::new(),
            )
            .expect_err("collisions exhausted");
        assert_eq!(error.code(), "LOCAL_PORT_UNAVAILABLE");
        assert_eq!(error.exit_code(), 5);
        assert_eq!(exhausted.calls.lock().expect("calls").len(), 4);
    }

    #[test]
    fn probe_transport_failure_precedes_bind_retry() {
        let (_temp, config) = fixture();
        let runner = ScriptedRunner {
            calls: Mutex::new(Vec::new()),
            outputs: Mutex::new(VecDeque::from([CommandOutput {
                exit_code: Some(1),
                stdout: Vec::new(),
                stderr: b"Error: couldn't bind tcl socket: Address already in use\nError: USB is disconnected\n"
                    .to_vec(),
            }])),
        };
        let ports = SequentialPorts(Mutex::new(41_000));
        let error = OpenOcdProgrammer::with_ports(&probes(), &runner, &ports, config)
            .execute(
                ProgramOperation::Reset,
                "ATML123",
                None,
                None,
                &CancellationToken::new(),
            )
            .expect_err("transport failure");
        assert_eq!(error.code(), "PROBE_DISCONNECTED");
        assert_eq!(error.exit_code(), 5);
        assert_eq!(runner.calls.lock().expect("calls").len(), 1);
    }

    #[test]
    fn timeouts_truthfully_distinguish_destructive_and_target_operations() {
        let (temp, config) = fixture();
        for (operation, authorization, code, exit_code) in [
            (
                ProgramOperation::Erase,
                Some("erase:ATML123"),
                "ERASE_TIMEOUT",
                6,
            ),
            (
                ProgramOperation::Flash,
                Some("flash:ATML123"),
                "FLASH_TIMEOUT",
                6,
            ),
            (ProgramOperation::Reset, None, "TARGET_COMMAND_TIMEOUT", 5),
            (ProgramOperation::Halt, None, "TARGET_COMMAND_TIMEOUT", 5),
        ] {
            let build_directory = temp.path().join(".samdebug/build/Debug");
            fs::create_dir_all(&build_directory).expect("managed build directory");
            let elf_path = build_directory.join("firmware.elf");
            fs::write(&elf_path, b"ELF").expect("ELF");
            let elf = FirmwareArtifact {
                path: elf_path,
                build_directory,
            };
            let error = OpenOcdProgrammer::with_ports(
                &probes(),
                &TimeoutRunner,
                &FixedPorts,
                config.clone(),
            )
            .execute(
                operation,
                "ATML123",
                authorization,
                (operation == ProgramOperation::Flash).then_some(&elf),
                &CancellationToken::new(),
            )
            .expect_err("timeout");
            assert_eq!(error.code(), code);
            assert_eq!(error.exit_code(), exit_code);
        }
    }

    #[test]
    fn command_quoting_keeps_path_in_one_openocd_command() {
        let quoted = command_quote("a$[b]\\c\"d\n");
        assert_eq!(quoted, "\"a\\$\\[b\\]\\\\c\\\"d\\n\"");
        assert!(!quoted.contains(';'));
        assert!(Path::new("firmware.elf").extension().is_some());
    }
}
