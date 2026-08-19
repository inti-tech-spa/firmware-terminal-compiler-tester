use std::{path::Path, process::Command};

use samdebug_core::{
    ErrorCategory, SamdebugError, SamdebugResult, SystemToolConfig,
    ports::{CommandSpec, ProbeInfo, ProbeProvider, ProcessRunner},
};
use serde::Serialize;

use crate::{
    Platform, ToolManifest,
    installer::{install_is_valid, sha256_file, validate_manifest},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    pub tool_channel: String,
    pub platform: PlatformReport,
    pub manifest_installable: bool,
    pub root: String,
    pub tools: Vec<ToolHealth>,
    pub probe: ProbeHealth,
    pub guidance: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlatformReport {
    pub os: String,
    pub architecture: String,
    pub supported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolHealth {
    pub name: String,
    pub version: String,
    pub install: String,
    pub cache: String,
    pub executables: Vec<ExecutableHealth>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutableHealth {
    pub name: String,
    pub status: String,
    pub reported_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProbeHealth {
    pub status: String,
    pub probes: Vec<ProbeInfoReport>,
    pub target_connectivity: String,
    pub target_voltage: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProbeInfoReport {
    pub serial: String,
    pub product: String,
}

pub fn run_doctor(
    manifest: &ToolManifest,
    root: &Path,
    platform: &Platform,
    probes: &dyn ProbeProvider,
    runner: &dyn ProcessRunner,
) -> SamdebugResult<DoctorReport> {
    validate_manifest(manifest)?;
    let selected: Vec<_> = manifest
        .artifacts
        .iter()
        .filter(|artifact| {
            artifact.os == platform.os && artifact.architecture == platform.architecture
        })
        .collect();
    let supported = !selected.is_empty();
    let mut guidance = Vec::new();
    let tools = inspect_tools(&selected, root, runner);

    if !manifest.installable {
        guidance.push(
            manifest
                .reason
                .clone()
                .unwrap_or_else(|| "managed tool installation is disabled".into()),
        );
    }
    if !supported {
        guidance.push(format!(
            "no managed tools are published for {}-{}",
            platform.os, platform.architecture
        ));
    }

    let probe = inspect_probe(&selected, root, probes, runner, &mut guidance);

    Ok(DoctorReport {
        tool_channel: "pinned".into(),
        platform: PlatformReport {
            os: platform.os.clone(),
            architecture: platform.architecture.clone(),
            supported,
        },
        manifest_installable: manifest.installable,
        root: root.to_string_lossy().into_owned(),
        tools,
        probe,
        guidance,
    })
}

pub fn run_system_doctor(
    tools: &SystemToolConfig,
    platform: &Platform,
    probes: &dyn ProbeProvider,
    runner: &dyn ProcessRunner,
) -> SamdebugResult<DoctorReport> {
    let definitions = [
        ("arm-none-eabi-gcc", &tools.gcc, "arm-none-eabi-gcc"),
        ("arm-none-eabi-gdb", &tools.gdb, "GNU gdb"),
        ("openocd", &tools.openocd, "Open On-Chip Debugger"),
        ("arm-none-eabi-objcopy", &tools.objcopy, "GNU objcopy"),
        ("arm-none-eabi-objdump", &tools.objdump, "GNU objdump"),
        ("arm-none-eabi-size", &tools.size, "GNU size"),
    ];
    let executables: Vec<_> = definitions
        .iter()
        .map(|(name, path, expected)| inspect_system_executable(name, path, expected, runner))
        .collect();
    let openocd_verified = executables
        .iter()
        .find(|executable| executable.name == "openocd")
        .is_some_and(|executable| executable.status == "verified");
    let mut guidance = vec![
        "system tool override is active; builds are not reproducible from the pinned manifest"
            .into(),
    ];
    let probe = inspect_probe_with_openocd(
        probes,
        runner,
        openocd_verified.then_some(Path::new(&tools.openocd)),
        None,
        &mut guidance,
    );
    Ok(DoctorReport {
        tool_channel: "system".into(),
        platform: PlatformReport {
            os: platform.os.clone(),
            architecture: platform.architecture.clone(),
            supported: platform.os == "macos" && platform.architecture == "aarch64",
        },
        manifest_installable: false,
        root: "system".into(),
        tools: vec![ToolHealth {
            name: "system-tools".into(),
            version: "unmanaged".into(),
            install: if executables
                .iter()
                .all(|executable| executable.status == "verified")
            {
                "verified".into()
            } else {
                "invalid".into()
            },
            cache: "not_applicable".into(),
            executables,
        }],
        probe,
        guidance,
    })
}

fn inspect_system_executable(
    name: &str,
    path: &str,
    expected: &str,
    runner: &dyn ProcessRunner,
) -> ExecutableHealth {
    if !Path::new(path).is_file() {
        return ExecutableHealth {
            name: name.into(),
            status: "missing".into(),
            reported_version: None,
        };
    }
    match runner.run(&CommandSpec {
        program: path.into(),
        args: vec!["--version".into()],
        current_dir: None,
    }) {
        Ok(output) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            ExecutableHealth {
                name: name.into(),
                status: if output.exit_code == Some(0) && text.contains(expected) {
                    "verified".into()
                } else {
                    "version_mismatch".into()
                },
                reported_version: Some(text.lines().next().unwrap_or_default().into()),
            }
        }
        Err(error) => ExecutableHealth {
            name: name.into(),
            status: "failed".into(),
            reported_version: Some(error.to_string()),
        },
    }
}

fn inspect_tools(
    artifacts: &[&crate::ToolArtifact],
    root: &Path,
    runner: &dyn ProcessRunner,
) -> Vec<ToolHealth> {
    artifacts
        .iter()
        .map(|artifact| {
            let destination = root
                .join("tools")
                .join(&artifact.name)
                .join(&artifact.version);
            let install = match install_is_valid(&destination, artifact) {
                Ok(true) => "verified",
                Ok(false) if destination.exists() => "corrupt",
                Ok(false) => "missing",
                Err(_) => "corrupt",
            };
            let cache_path = root
                .join("downloads")
                .join(format!("{}-{}.tar.xz", artifact.name, artifact.sha256));
            let cache = if cache_path.is_file() {
                match sha256_file(&cache_path) {
                    Ok(actual) if actual == artifact.sha256 => "verified",
                    _ => "corrupt",
                }
            } else {
                "missing"
            };
            let executables = artifact
                .executables
                .iter()
                .map(|executable| inspect_executable(&destination, executable, runner))
                .collect();
            ToolHealth {
                name: artifact.name.clone(),
                version: artifact.version.clone(),
                install: install.into(),
                cache: cache.into(),
                executables,
            }
        })
        .collect()
}

fn inspect_executable(
    destination: &Path,
    executable: &crate::ExecutableSpec,
    runner: &dyn ProcessRunner,
) -> ExecutableHealth {
    let path = destination.join(&executable.path);
    if !path.is_file() {
        return ExecutableHealth {
            name: executable.name.clone(),
            status: "missing".into(),
            reported_version: None,
        };
    }
    match runner.run(&CommandSpec {
        program: path.to_string_lossy().into_owned(),
        args: executable.version_args.clone(),
        current_dir: None,
    }) {
        Ok(output) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let status =
                if output.exit_code == Some(0) && text.contains(&executable.version_contains) {
                    "verified"
                } else {
                    "version_mismatch"
                };
            ExecutableHealth {
                name: executable.name.clone(),
                status: status.into(),
                reported_version: Some(text.lines().next().unwrap_or_default().into()),
            }
        }
        Err(error) => ExecutableHealth {
            name: executable.name.clone(),
            status: "failed".into(),
            reported_version: Some(error.to_string()),
        },
    }
}

fn inspect_probe(
    artifacts: &[&crate::ToolArtifact],
    root: &Path,
    probes: &dyn ProbeProvider,
    runner: &dyn ProcessRunner,
    guidance: &mut Vec<String>,
) -> ProbeHealth {
    let managed = artifacts
        .iter()
        .find(|artifact| artifact.name == "openocd")
        .and_then(|artifact| {
            let installation = root
                .join("tools")
                .join(&artifact.name)
                .join(&artifact.version);
            if install_is_valid(&installation, artifact).ok() != Some(true) {
                return None;
            }
            let executable = artifact
                .executables
                .iter()
                .find(|executable| executable.name == "openocd")?;
            Some((
                installation.join(&executable.path),
                installation.join("share/openocd/scripts"),
            ))
        });
    inspect_probe_with_openocd(
        probes,
        runner,
        managed.as_ref().map(|(executable, _)| executable.as_path()),
        managed.as_ref().map(|(_, scripts)| scripts.as_path()),
        guidance,
    )
}

fn inspect_probe_with_openocd(
    probes: &dyn ProbeProvider,
    runner: &dyn ProcessRunner,
    openocd: Option<&Path>,
    scripts: Option<&Path>,
    guidance: &mut Vec<String>,
) -> ProbeHealth {
    let (status, probe_list): (String, Vec<ProbeInfo>) = match probes.list() {
        Ok(found) if found.is_empty() => ("absent".into(), Vec::new()),
        Ok(found) if found.len() == 1 => ("visible".into(), found),
        Ok(found) => ("multiple".into(), found),
        Err(error) => {
            guidance.push(format!("probe discovery failed: {error}"));
            ("discovery_failed".into(), Vec::new())
        }
    };
    if status == "absent" {
        guidance.push("connect Atmel-ICE over USB; no separate macOS driver is required".into());
    }
    let (target_connectivity, target_voltage) = diagnose_target(openocd, scripts, runner, &status);
    ProbeHealth {
        status,
        probes: probe_list
            .into_iter()
            .map(|probe| ProbeInfoReport {
                serial: probe.serial,
                product: probe.product,
            })
            .collect(),
        target_connectivity,
        target_voltage,
    }
}

fn diagnose_target(
    openocd: Option<&Path>,
    scripts: Option<&Path>,
    runner: &dyn ProcessRunner,
    probe_status: &str,
) -> (String, Option<String>) {
    if probe_status != "visible" {
        return ("not_checked".into(), None);
    }
    let Some(openocd) = openocd else {
        return ("openocd_unavailable".into(), None);
    };
    let mut args = Vec::new();
    if let Some(scripts) = scripts {
        args.extend(["-s".into(), scripts.to_string_lossy().into_owned()]);
    }
    args.extend([
        "-f".into(),
        "interface/cmsis-dap.cfg".into(),
        "-c".into(),
        "transport select swd".into(),
        "-f".into(),
        "target/at91sam4sXX.cfg".into(),
        "-c".into(),
        "adapter speed 1000".into(),
        "-c".into(),
        "init".into(),
        "-c".into(),
        "shutdown".into(),
    ]);
    let output = runner.run(&CommandSpec {
        program: openocd.to_string_lossy().into_owned(),
        args,
        current_dir: None,
    });
    match output {
        Ok(output) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let voltage = text.lines().find_map(|line| {
                line.find("VTarget =")
                    .map(|index| line[index + "VTarget =".len()..].trim().to_owned())
            });
            if output.exit_code == Some(0) {
                ("connected".into(), voltage)
            } else if text.to_ascii_lowercase().contains("target voltage") {
                ("missing_target_power".into(), voltage)
            } else {
                ("connection_failed".into(), voltage)
            }
        }
        Err(_) => ("connection_failed".into(), None),
    }
}

#[derive(Debug, Default)]
pub struct MacUsbProbeProvider;

impl ProbeProvider for MacUsbProbeProvider {
    fn list(&self) -> SamdebugResult<Vec<ProbeInfo>> {
        if std::env::consts::OS != "macos" {
            return Err(SamdebugError::new(
                ErrorCategory::Connection,
                "PROBE_DISCOVERY_UNSUPPORTED",
                "Atmel-ICE discovery is supported on macOS in version 1",
            ));
        }
        let output = Command::new("/usr/sbin/system_profiler")
            .args(["SPUSBDataType", "-json"])
            .output()
            .map_err(|error| {
                SamdebugError::new(
                    ErrorCategory::Connection,
                    "PROBE_DISCOVERY_FAILED",
                    error.to_string(),
                )
            })?;
        if !output.status.success() {
            return Err(SamdebugError::new(
                ErrorCategory::Connection,
                "PROBE_DISCOVERY_FAILED",
                String::from_utf8_lossy(&output.stderr),
            ));
        }
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|error| {
            SamdebugError::new(
                ErrorCategory::Connection,
                "PROBE_DISCOVERY_FAILED",
                error.to_string(),
            )
        })?;
        let mut found = Vec::new();
        collect_atmel_ice(&value, &mut found);
        found.sort_by(|left, right| left.serial.cmp(&right.serial));
        found.dedup_by(|left, right| left.serial == right.serial);
        Ok(found)
    }
}

fn collect_atmel_ice(value: &serde_json::Value, found: &mut Vec<ProbeInfo>) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                collect_atmel_ice(value, found);
            }
        }
        serde_json::Value::Object(object) => {
            let name = object
                .get("_name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let product_id = object
                .get("product_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let vendor_id = object
                .get("vendor_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if name.to_ascii_lowercase().contains("atmel-ice")
                || (vendor_id.contains("0x03eb") && product_id.contains("0x2141"))
            {
                let serial = object
                    .get("serial_num")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                found.push(ProbeInfo {
                    serial,
                    product: if name.is_empty() {
                        "Atmel-ICE".into()
                    } else {
                        name.into()
                    },
                });
            }
            for value in object.values() {
                collect_atmel_ice(value, found);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use samdebug_core::{
        SamdebugResult,
        ports::{CommandOutput, CommandSpec, ProbeInfo, ProbeProvider, ProcessRunner},
    };
    use tempfile::TempDir;

    use super::{collect_atmel_ice, run_doctor};
    use crate::{Platform, ToolManifest};

    #[derive(Debug)]
    struct FakeProbes(Vec<ProbeInfo>);

    impl ProbeProvider for FakeProbes {
        fn list(&self) -> SamdebugResult<Vec<ProbeInfo>> {
            Ok(self.0.clone())
        }
    }

    #[derive(Debug)]
    struct FakeRunner;

    impl ProcessRunner for FakeRunner {
        fn run(&self, _command: &CommandSpec) -> SamdebugResult<CommandOutput> {
            Ok(CommandOutput {
                exit_code: Some(0),
                stdout: b"fixture 1.0".to_vec(),
                stderr: Vec::new(),
            })
        }

        fn spawn(
            &self,
            _command: &CommandSpec,
        ) -> SamdebugResult<Box<dyn samdebug_core::ports::ManagedChild>> {
            unreachable!("doctor does not spawn")
        }
    }

    #[test]
    fn doctor_reports_enabled_manifest_missing_tool_and_absent_probe() {
        let temp = TempDir::new().expect("tempdir");
        let manifest: ToolManifest =
            serde_json::from_str(include_str!("../../../tools/manifest-v1.json"))
                .expect("embedded manifest parses");
        let report = run_doctor(
            &manifest,
            temp.path(),
            &Platform {
                os: "macos".into(),
                architecture: "aarch64".into(),
            },
            &FakeProbes(Vec::new()),
            &FakeRunner,
        )
        .expect("doctor report");
        assert!(report.manifest_installable);
        assert_eq!(report.tools[0].install, "missing");
        assert_eq!(report.tools[0].cache, "missing");
        assert_eq!(report.probe.status, "absent");
        assert!(report.guidance.iter().any(|line| line.contains("driver")));
    }

    #[test]
    fn system_profiler_tree_extracts_serial_and_deduplicates() {
        let fixture = serde_json::json!({
            "SPUSBDataType": [{
                "_items": [{
                    "_name": "Atmel-ICE CMSIS-DAP",
                    "vendor_id": "0x03eb",
                    "product_id": "0x2141",
                    "serial_num": "ICE123"
                }]
            }]
        });
        let mut found = Vec::new();
        collect_atmel_ice(&fixture, &mut found);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].serial, "ICE123");
    }

    #[test]
    fn unsupported_platform_is_actionable() {
        let temp = TempDir::new().expect("tempdir");
        let manifest: ToolManifest =
            serde_json::from_str(include_str!("../../../tools/manifest-v1.json"))
                .expect("embedded manifest parses");
        let report = run_doctor(
            &manifest,
            temp.path(),
            &Platform {
                os: "windows".into(),
                architecture: "x86_64".into(),
            },
            &FakeProbes(Vec::new()),
            &FakeRunner,
        )
        .expect("doctor report");
        assert!(!report.platform.supported);
        assert!(
            report
                .guidance
                .iter()
                .any(|line| line.contains("windows-x86_64"))
        );
    }

    #[test]
    fn visible_probe_with_missing_managed_openocd_is_not_a_target_failure() {
        let temp = TempDir::new().expect("tempdir");
        let manifest: ToolManifest =
            serde_json::from_str(include_str!("../../../tools/manifest-v1.json"))
                .expect("embedded manifest parses");
        let report = run_doctor(
            &manifest,
            temp.path(),
            &Platform {
                os: "macos".into(),
                architecture: "aarch64".into(),
            },
            &FakeProbes(vec![ProbeInfo {
                serial: "ICE123".into(),
                product: "Atmel-ICE".into(),
            }]),
            &FakeRunner,
        )
        .expect("doctor report");
        assert_eq!(report.probe.target_connectivity, "openocd_unavailable");
    }
}
