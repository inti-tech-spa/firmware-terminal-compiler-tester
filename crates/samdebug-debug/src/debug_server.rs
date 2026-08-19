use std::{
    fs,
    io::{BufRead, BufReader, Read},
    net::{Ipv4Addr, SocketAddrV4, TcpListener},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use samdebug_core::{CancellationToken, ErrorCategory, SamdebugError, SamdebugResult};

use crate::OpenOcdConfig;

const STARTUP_ATTEMPTS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebugServerPorts {
    pub gdb: u16,
    pub tcl: u16,
    pub telnet: u16,
}

#[derive(Debug)]
pub struct OpenOcdDebugServer {
    child: Child,
    ports: DebugServerPorts,
    log: Arc<Mutex<String>>,
    readers: Vec<JoinHandle<()>>,
    stopped: bool,
}

impl OpenOcdDebugServer {
    pub fn launch(
        config: &OpenOcdConfig,
        serial: &str,
        cancellation: &CancellationToken,
    ) -> SamdebugResult<Self> {
        validate(config, serial)?;
        for attempt in 1..=STARTUP_ATTEMPTS {
            let ports = select_ports()?;
            let mut server = Self::spawn(config, serial, ports)?;
            match server.wait_ready(config.timeout, cancellation) {
                Ok(()) => return Ok(server),
                Err(error)
                    if error.code() == "LOCAL_PORT_UNAVAILABLE" && attempt < STARTUP_ATTEMPTS =>
                {
                    let _ = server.stop();
                }
                Err(error) => {
                    let _ = server.stop();
                    return Err(error);
                }
            }
        }
        Err(connection_error(
            "LOCAL_PORT_UNAVAILABLE",
            "OpenOCD could not bind fresh loopback ports",
        ))
    }

    fn spawn(
        config: &OpenOcdConfig,
        serial: &str,
        ports: DebugServerPorts,
    ) -> SamdebugResult<Self> {
        let mut command = Command::new(&config.executable);
        command
            .args([
                "-s",
                &config.scripts.to_string_lossy(),
                "-c",
                "bindto 127.0.0.1",
                "-c",
                &format!("gdb_port {}", ports.gdb),
                "-c",
                &format!("tcl_port {}", ports.tcl),
                "-c",
                &format!("telnet_port {}", ports.telnet),
                "-f",
                "interface/cmsis-dap.cfg",
                "-c",
                "cmsis_dap_backend hid",
                "-c",
                &format!("adapter serial {}", tcl_quote(serial)),
                "-c",
                "transport select swd",
                "-f",
                "target/at91sam4sXX.cfg",
                "-c",
                &format!("adapter speed {}", config.speed_khz),
                "-c",
                "init",
                "-c",
                "reset halt",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_group(&mut command);
        let mut child = command.spawn().map_err(|error| {
            connection_error(
                "OPENOCD_START_FAILED",
                format!("failed to start OpenOCD: {error}"),
            )
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            connection_error("OPENOCD_START_FAILED", "OpenOCD stdout was not piped")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            connection_error("OPENOCD_START_FAILED", "OpenOCD stderr was not piped")
        })?;
        let log = Arc::new(Mutex::new(String::new()));
        let readers = vec![
            spawn_log_reader(stdout, Arc::clone(&log)),
            spawn_log_reader(stderr, Arc::clone(&log)),
        ];
        Ok(Self {
            child,
            ports,
            log,
            readers,
            stopped: false,
        })
    }

    fn wait_ready(
        &mut self,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> SamdebugResult<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if cancellation.is_cancelled() {
                return Err(SamdebugError::new(
                    ErrorCategory::Interrupted,
                    "INTERRUPTED",
                    "debug server startup interrupted",
                ));
            }
            if self.log().contains(&format!(
                "Listening on port {} for gdb connections",
                self.ports.gdb
            )) {
                return Ok(());
            }
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|error| connection_error("OPENOCD_WAIT_FAILED", error.to_string()))?
            {
                self.finish_readers();
                let log = self.log();
                let (code, message) = if is_probe_disconnect(&log) {
                    (
                        "PROBE_DISCONNECTED",
                        "Atmel-ICE disconnected during startup",
                    )
                } else if is_bind_failure(&log) {
                    (
                        "LOCAL_PORT_UNAVAILABLE",
                        "OpenOCD could not bind a loopback port",
                    )
                } else {
                    (
                        "OPENOCD_START_FAILED",
                        "OpenOCD exited before GDB port readiness",
                    )
                };
                return Err(connection_error(code, message).with_details(
                    serde_json::json!({"exit_code": status.code(), "openocd_log": log}),
                ));
            }
            if Instant::now() >= deadline {
                return Err(connection_error(
                    "OPENOCD_START_TIMEOUT",
                    "OpenOCD GDB port did not become ready within the bounded startup timeout",
                )
                .with_details(serde_json::json!({"openocd_log": self.log()})));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[must_use]
    pub const fn ports(&self) -> DebugServerPorts {
        self.ports
    }

    #[must_use]
    pub fn log(&self) -> String {
        self.log.lock().expect("OpenOCD log mutex poisoned").clone()
    }

    pub fn check_alive(&mut self) -> SamdebugResult<()> {
        if self.stopped {
            return Err(connection_error("OPENOCD_EXITED", "OpenOCD is stopped"));
        }
        if let Some(status) = self
            .child
            .try_wait()
            .map_err(|error| connection_error("OPENOCD_WAIT_FAILED", error.to_string()))?
        {
            self.finish_readers();
            let log = self.log();
            let code = if is_probe_disconnect(&log) {
                "PROBE_DISCONNECTED"
            } else {
                "OPENOCD_EXITED"
            };
            return Err(
                connection_error(code, "OpenOCD debug server exited").with_details(
                    serde_json::json!({"exit_code": status.code(), "openocd_log": log}),
                ),
            );
        }
        Ok(())
    }

    pub fn stop(&mut self) -> SamdebugResult<()> {
        if self.stopped {
            return Ok(());
        }
        if !wait_child(&mut self.child, Duration::from_millis(100))? {
            terminate_group(self.child.id());
            if !wait_child(&mut self.child, Duration::from_secs(1))? {
                kill_group(self.child.id());
                self.child
                    .kill()
                    .map_err(|error| connection_error("OPENOCD_KILL_FAILED", error.to_string()))?;
                self.child
                    .wait()
                    .map_err(|error| connection_error("OPENOCD_WAIT_FAILED", error.to_string()))?;
            }
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

impl Drop for OpenOcdDebugServer {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn select_ports() -> SamdebugResult<DebugServerPorts> {
    let mut listeners = Vec::new();
    for _ in 0..3 {
        listeners.push(
            TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
                .map_err(|error| connection_error("LOCAL_PORT_UNAVAILABLE", error.to_string()))?,
        );
    }
    let ports = DebugServerPorts {
        gdb: listeners[0]
            .local_addr()
            .map_err(|error| connection_error("LOCAL_PORT_UNAVAILABLE", error.to_string()))?
            .port(),
        tcl: listeners[1]
            .local_addr()
            .map_err(|error| connection_error("LOCAL_PORT_UNAVAILABLE", error.to_string()))?
            .port(),
        telnet: listeners[2]
            .local_addr()
            .map_err(|error| connection_error("LOCAL_PORT_UNAVAILABLE", error.to_string()))?
            .port(),
    };
    drop(listeners);
    Ok(ports)
}

fn validate(config: &OpenOcdConfig, serial: &str) -> SamdebugResult<()> {
    let metadata = fs::symlink_metadata(&config.executable)
        .map_err(|error| connection_error("OPENOCD_CONFIGURATION_INVALID", error.to_string()))?;
    if !config.executable.is_absolute()
        || !metadata.is_file()
        || metadata.file_type().is_symlink()
        || !config.scripts.is_absolute()
        || !config.scripts.join("interface/cmsis-dap.cfg").is_file()
        || !config.scripts.join("target/at91sam4sXX.cfg").is_file()
        || config.speed_khz == 0
        || serial.is_empty()
        || serial.chars().any(char::is_control)
    {
        return Err(connection_error(
            "OPENOCD_CONFIGURATION_INVALID",
            "invalid OpenOCD debug server configuration",
        ));
    }
    Ok(())
}

fn spawn_log_reader(stream: impl Read + Send + 'static, log: Arc<Mutex<String>>) -> JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            let mut log = log.lock().expect("OpenOCD log mutex poisoned");
            if log.len() < 1024 * 1024 {
                log.push_str(&line);
                log.push('\n');
            }
        }
    })
}

fn is_bind_failure(log: &str) -> bool {
    let lower = log.to_ascii_lowercase();
    lower.contains("address already in use")
        || lower.contains("couldn't bind")
        || lower.contains("failed to bind")
}

fn is_probe_disconnect(log: &str) -> bool {
    let lower = log.to_ascii_lowercase();
    crate::programming::is_probe_transport_failure(&lower)
}

fn tcl_quote(value: &str) -> String {
    let mut output = String::from('"');
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '$' => output.push_str("\\$"),
            '[' => output.push_str("\\["),
            ']' => output.push_str("\\]"),
            value => output.push(value),
        }
    }
    output.push('"');
    output
}

fn wait_child(child: &mut Child, timeout: Duration) -> SamdebugResult<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if child
            .try_wait()
            .map_err(|error| connection_error("OPENOCD_WAIT_FAILED", error.to_string()))?
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
fn terminate_group(pid: u32) {
    signal_group(pid, "-TERM");
}

#[cfg(not(unix))]
fn terminate_group(_pid: u32) {}

#[cfg(unix)]
fn kill_group(pid: u32) {
    signal_group(pid, "-KILL");
}

#[cfg(not(unix))]
fn kill_group(_pid: u32) {}

#[cfg(unix)]
fn signal_group(pid: u32, signal: &str) {
    let _ = Command::new("/bin/kill")
        .args([signal, &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn connection_error(code: &str, message: impl Into<String>) -> SamdebugError {
    SamdebugError::new(ErrorCategory::Connection, code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_and_bind_diagnostics_are_distinct() {
        assert!(is_bind_failure(
            "Error: couldn't bind tcl socket: Address already in use"
        ));
        assert!(is_probe_disconnect("Error: USB is disconnected"));
        assert!(is_probe_disconnect(
            "Error: CMSIS-DAP command CMD_CONNECT failed."
        ));
        assert!(is_probe_disconnect(
            "Error: couldn't bind tcl socket: Address already in use\nError: USB is disconnected"
        ));
        assert!(!is_probe_disconnect("Error: target examination failed"));
    }

    #[test]
    fn tcl_quotes_probe_serial_without_command_injection() {
        assert_eq!(tcl_quote("A$[B]\\\""), "\"A\\$\\[B\\]\\\\\\\"\"");
    }
}
