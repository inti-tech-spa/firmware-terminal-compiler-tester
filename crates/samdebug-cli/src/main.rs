use std::{ffi::OsString, path::PathBuf, process::ExitCode, time::Duration};

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum, error::ErrorKind};
use samdebug_core::{
    CancellationToken, Configuration, ErrorCategory, FiniteResult, SamdebugConfig, SamdebugError,
    SamdebugResult,
    ports::{CommandSpec, DownloadReceipt, Downloader, FileSystem, ProcessRunner},
};
use samdebug_debug::{
    FirmwareArtifact, OpenOcdConfig, OpenOcdProgrammer, ProgramOperation, list_probes,
};
use samdebug_project::{BuildToolPaths, artifacts, build, clean, import_cproj, initialize_project};
use samdebug_tools::{
    ChildSupervisor, CurlDownloader, Installer, MacUsbProbeProvider, Platform, SystemProcessRunner,
    ToolManifest, run_doctor, run_system_doctor,
};
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Human,
    Json,
}

#[derive(Debug, Parser)]
#[command(
    name = "samdebug",
    version,
    about = "ATSAM4SD32C terminal compiler and debugger"
)]
struct Cli {
    #[arg(long, value_enum, default_value_t = OutputFormat::Human, global = true)]
    output: OutputFormat,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Version,
    Setup {
        #[arg(long)]
        offline: bool,
    },
    Doctor,
    Init(InitArgs),
    Build,
    Clean,
    Artifacts,
    Probe {
        #[command(subcommand)]
        command: ProbeCommand,
    },
    Erase(AuthorizedArgs),
    Flash(AuthorizedArgs),
    Debug(DebugArgs),
    #[command(name = "__test-block", hide = true)]
    InternalTestBlock,
    #[command(name = "__test-setup-cancel", hide = true)]
    InternalTestSetupCancel,
}

#[derive(Debug, Args)]
struct InitArgs {
    #[arg(long)]
    from_cproj: PathBuf,
    #[arg(long, value_parser = ["Debug", "Release"])]
    configuration: String,
}

#[derive(Debug, Subcommand)]
enum ProbeCommand {
    List,
}

#[derive(Debug, Args)]
struct AuthorizedArgs {
    #[arg(long)]
    probe: String,
    #[arg(long)]
    confirm: String,
}

#[derive(Debug, Args)]
struct DebugArgs {
    #[arg(long, requires = "stdio")]
    agent: bool,
    #[arg(long, requires = "agent")]
    stdio: bool,
}

#[derive(Debug, Serialize)]
struct VersionData<'a> {
    version: &'a str,
    target: &'a str,
}

fn main() -> ExitCode {
    init_logging();
    let cli = match parse_cli() {
        Ok(cli) => cli,
        Err(code) => return code,
    };
    let cancellation = CancellationToken::new();
    let signal_token = cancellation.clone();
    if let Err(error) = ctrlc::set_handler(move || signal_token.cancel()) {
        eprintln!("failed to install signal handler: {error}");
        return ExitCode::from(2);
    }

    match dispatch(&cli, &cancellation) {
        Ok((command, data)) => {
            emit_success(cli.output, command, data);
            ExitCode::SUCCESS
        }
        Err((command, error)) => {
            let code = error.exit_code();
            emit_failure(cli.output, command, error);
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        }
    }
}

fn parse_cli() -> Result<Cli, ExitCode> {
    let args: Vec<OsString> = std::env::args_os().collect();
    let output = requested_output(&args);
    match Cli::try_parse_from(&args) {
        Ok(cli) => Ok(cli),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            if matches!(output, OutputFormat::Json) {
                if error.kind() == ErrorKind::DisplayVersion {
                    emit_success(
                        output,
                        "version",
                        serde_json::to_value(VersionData {
                            version: env!("CARGO_PKG_VERSION"),
                            target: std::env::consts::ARCH,
                        })
                        .expect("version serializes"),
                    );
                } else {
                    emit_success(output, "help", json!({"text": error.to_string()}));
                }
            } else {
                print!("{error}");
            }
            Err(ExitCode::SUCCESS)
        }
        Err(error) => {
            if matches!(output, OutputFormat::Json) {
                emit_failure(
                    OutputFormat::Json,
                    "cli",
                    SamdebugError::new(
                        ErrorCategory::Command,
                        "INVALID_COMMAND",
                        error.to_string(),
                    ),
                );
            } else {
                let mut command = Cli::command();
                let _ = command.print_help();
                eprintln!("\n{error}");
            }
            Err(ExitCode::from(2))
        }
    }
}

fn requested_output(args: &[OsString]) -> OutputFormat {
    let json_pair = args.windows(2).any(|pair| {
        pair[0] == std::ffi::OsStr::new("--output") && pair[1] == std::ffi::OsStr::new("json")
    });
    let json_equals = args
        .iter()
        .any(|arg| arg == std::ffi::OsStr::new("--output=json"));
    if json_pair || json_equals {
        OutputFormat::Json
    } else {
        OutputFormat::Human
    }
}

#[allow(clippy::too_many_lines)]
fn dispatch(
    cli: &Cli,
    cancellation: &CancellationToken,
) -> Result<(&'static str, serde_json::Value), (&'static str, SamdebugError)> {
    if cancellation.is_cancelled() {
        return Err((
            "cancelled",
            SamdebugError::new(
                ErrorCategory::Interrupted,
                "INTERRUPTED",
                "operation interrupted",
            ),
        ));
    }
    match &cli.command {
        Command::Version => Ok((
            "version",
            serde_json::to_value(VersionData {
                version: env!("CARGO_PKG_VERSION"),
                target: std::env::consts::ARCH,
            })
            .expect("version serializes"),
        )),
        Command::Setup { offline } => setup_command(*offline, cancellation)
            .map(|report| {
                (
                    "setup",
                    serde_json::to_value(report).expect("setup report serializes"),
                )
            })
            .map_err(|error| ("setup", error)),
        Command::Doctor => doctor_command()
            .map(|report| {
                (
                    "doctor",
                    serde_json::to_value(report).expect("doctor report serializes"),
                )
            })
            .map_err(|error| ("doctor", error)),
        Command::Init(args) => {
            let configuration = match args.configuration.as_str() {
                "Debug" => Configuration::Debug,
                "Release" => Configuration::Release,
                _ => unreachable!("clap validates configurations"),
            };
            initialize_project(&args.from_cproj, configuration)
                .map(|report| {
                    (
                        "init",
                        serde_json::to_value(report).expect("init report serializes"),
                    )
                })
                .map_err(|error| ("init", error))
        }
        Command::Build => build_command(cancellation)
            .map(|report| {
                (
                    "build",
                    serde_json::to_value(report).expect("build report serializes"),
                )
            })
            .map_err(|error| ("build", error)),
        Command::Clean => project_command_context()
            .and_then(|(root, config)| clean(&root, config.project.configuration))
            .map(|report| {
                (
                    "clean",
                    serde_json::to_value(report).expect("clean report serializes"),
                )
            })
            .map_err(|error| ("clean", error)),
        Command::Artifacts => project_command_context()
            .and_then(|(root, config)| artifacts(&root, config.project.configuration))
            .map(|report| {
                (
                    "artifacts",
                    serde_json::to_value(report).expect("artifacts report serializes"),
                )
            })
            .map_err(|error| ("artifacts", error)),
        Command::Debug(args) if !args.agent => samdebug_tui::run()
            .map(|()| ("debug", json!({})))
            .map_err(|error| ("debug", error)),
        Command::Probe {
            command: ProbeCommand::List,
        } => list_probes(&MacUsbProbeProvider)
            .map(|report| {
                (
                    "probe",
                    serde_json::to_value(report).expect("probe report serializes"),
                )
            })
            .map_err(|error| ("probe", error)),
        Command::Erase(args) => programming_command(ProgramOperation::Erase, args, cancellation),
        Command::Flash(args) => programming_command(ProgramOperation::Flash, args, cancellation),
        Command::InternalTestBlock => run_internal_blocking_test(cancellation)
            .map(|code| ("__test-block", json!({"child_exit_code": code})))
            .map_err(|error| ("__test-block", error)),
        Command::InternalTestSetupCancel => run_internal_setup_cancel(cancellation)
            .map(|report| {
                (
                    "__test-setup-cancel",
                    serde_json::to_value(report).expect("report serializes"),
                )
            })
            .map_err(|error| ("__test-setup-cancel", error)),
        command => Err((
            command_name(command),
            SamdebugError::new(
                ErrorCategory::Command,
                "NOT_IMPLEMENTED",
                "command is reserved for a later audited milestone",
            ),
        )),
    }
}

fn build_command(
    cancellation: &CancellationToken,
) -> Result<samdebug_project::BuildReport, SamdebugError> {
    let (root, config) = project_command_context()?;
    let project_file = root.join(&config.project.path);
    let tools = resolve_build_tools(&config)?;
    build(
        &project_file,
        config.project.configuration,
        &tools,
        &SystemProcessRunner,
        cancellation,
    )
}

fn project_command_context() -> Result<(PathBuf, SamdebugConfig), SamdebugError> {
    let root = std::env::current_dir().map_err(|error| {
        SamdebugError::new(
            ErrorCategory::Command,
            "CURRENT_DIRECTORY_FAILED",
            error.to_string(),
        )
    })?;
    let loaded = SamdebugConfig::load(&LocalFileSystem, &root.join("samdebug.toml"))?;
    Ok((root, loaded.config))
}

fn resolve_build_tools(config: &SamdebugConfig) -> Result<BuildToolPaths, SamdebugError> {
    if let Some(system) = config.tools.system.as_ref() {
        return Ok(BuildToolPaths {
            gcc: system.gcc.clone(),
            objcopy: system.objcopy.clone(),
            objdump: system.objdump.clone(),
            size: system.size.clone(),
        });
    }
    let bin = managed_root()?.join("tools/arm-gnu-toolchain/15.2.Rel1/bin");
    let path = |name: &str| bin.join(name).to_string_lossy().into_owned();
    Ok(BuildToolPaths {
        gcc: path("arm-none-eabi-gcc"),
        objcopy: path("arm-none-eabi-objcopy"),
        objdump: path("arm-none-eabi-objdump"),
        size: path("arm-none-eabi-size"),
    })
}

fn setup_command(
    offline: bool,
    cancellation: &CancellationToken,
) -> Result<samdebug_tools::InstallReport, SamdebugError> {
    let manifest = embedded_manifest()?;
    Installer::new(
        managed_root()?,
        Platform::current(),
        &CurlDownloader,
        cancellation,
    )
    .install(&manifest, offline)
}

fn doctor_command() -> Result<samdebug_tools::DoctorReport, SamdebugError> {
    let config_path = PathBuf::from("samdebug.toml");
    if config_path.is_file() {
        let loaded = SamdebugConfig::load(&LocalFileSystem, &config_path)?;
        if let Some(system) = loaded.config.tools.system.as_ref() {
            return run_system_doctor(
                system,
                &Platform::current(),
                &MacUsbProbeProvider,
                &SystemProcessRunner,
            );
        }
    }
    let manifest = embedded_manifest()?;
    run_doctor(
        &manifest,
        &managed_root()?,
        &Platform::current(),
        &MacUsbProbeProvider,
        &SystemProcessRunner,
    )
}

#[derive(Debug)]
struct LocalFileSystem;

impl FileSystem for LocalFileSystem {
    fn read_to_string(&self, path: &std::path::Path) -> SamdebugResult<String> {
        std::fs::read_to_string(path).map_err(|error| {
            SamdebugError::new(
                ErrorCategory::Command,
                "CONFIG_READ_FAILED",
                error.to_string(),
            )
        })
    }
}

fn embedded_manifest() -> Result<ToolManifest, SamdebugError> {
    serde_json::from_str(include_str!("../../../tools/manifest-v1.json")).map_err(|error| {
        SamdebugError::new(
            ErrorCategory::Tool,
            "INVALID_TOOL_MANIFEST",
            error.to_string(),
        )
    })
}

fn managed_root() -> Result<PathBuf, SamdebugError> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        SamdebugError::new(
            ErrorCategory::Tool,
            "USER_DATA_DIRECTORY_UNAVAILABLE",
            "HOME is unavailable; cannot resolve the user-local tool directory",
        )
    })?;
    let home = PathBuf::from(home);
    if std::env::consts::OS == "macos" {
        Ok(home.join("Library/Application Support/samdebug"))
    } else {
        Ok(home.join(".local/share/samdebug"))
    }
}

fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(true)
        .with_max_level(tracing::Level::INFO)
        .try_init();
}

fn programming_command(
    operation: ProgramOperation,
    args: &AuthorizedArgs,
    cancellation: &CancellationToken,
) -> Result<(&'static str, serde_json::Value), (&'static str, SamdebugError)> {
    let name = match operation {
        ProgramOperation::Erase => "erase",
        ProgramOperation::Flash => "flash",
        ProgramOperation::Reset | ProgramOperation::Halt => unreachable!("not CLI operations"),
    };
    let result = (|| {
        let (root, config) = project_command_context()?;
        let openocd = resolve_openocd(&config)?;
        let elf = if operation == ProgramOperation::Flash {
            let project_file = root.join(&config.project.path);
            let imported = import_cproj(&project_file, config.project.configuration)?;
            let build_directory = root
                .join(".samdebug/build")
                .join(&imported.plan.configuration);
            Some(FirmwareArtifact {
                path: build_directory.join(format!(
                    "{}{}",
                    imported.plan.output_name, imported.plan.output_extension
                )),
                build_directory,
            })
        } else {
            None
        };
        OpenOcdProgrammer::new(&MacUsbProbeProvider, &SystemProcessRunner, openocd).execute(
            operation,
            &args.probe,
            Some(&args.confirm),
            elf.as_ref(),
            cancellation,
        )
    })();
    result
        .map(|report| {
            (
                name,
                serde_json::to_value(report).expect("programming report serializes"),
            )
        })
        .map_err(|error| (name, error))
}

fn resolve_openocd(config: &SamdebugConfig) -> Result<OpenOcdConfig, SamdebugError> {
    let executable = if let Some(system) = &config.tools.system {
        PathBuf::from(&system.openocd)
    } else {
        managed_root()?.join("tools/openocd/0.12.0/bin/openocd")
    };
    let root = executable
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or_else(|| {
            SamdebugError::new(
                ErrorCategory::Tool,
                "OPENOCD_CONFIGURATION_INVALID",
                "OpenOCD path has no installation root",
            )
        })?
        .to_path_buf();
    Ok(OpenOcdConfig::new(
        executable,
        root.join("share/openocd/scripts"),
        config.probe.speed_khz,
    ))
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Version => "version",
        Command::Setup { .. } => "setup",
        Command::Doctor => "doctor",
        Command::Init(_) => "init",
        Command::Build => "build",
        Command::Clean => "clean",
        Command::Artifacts => "artifacts",
        Command::Probe { .. } => "probe",
        Command::Erase(_) => "erase",
        Command::Flash(_) => "flash",
        Command::Debug(_) => "debug",
        Command::InternalTestBlock => "__test-block",
        Command::InternalTestSetupCancel => "__test-setup-cancel",
    }
}

#[derive(Debug)]
struct SlowTestDownloader {
    ready_file: PathBuf,
}

impl Downloader for SlowTestDownloader {
    fn download(
        &self,
        _url: &str,
        _allowed_hosts: &[String],
        destination: &std::path::Path,
        cancellation: &CancellationToken,
    ) -> SamdebugResult<DownloadReceipt> {
        std::fs::write(destination, b"partial").map_err(|error| {
            SamdebugError::new(
                ErrorCategory::Tool,
                "TEST_DOWNLOAD_FAILED",
                error.to_string(),
            )
        })?;
        std::fs::write(&self.ready_file, b"ready").map_err(|error| {
            SamdebugError::new(ErrorCategory::Tool, "TEST_READY_FAILED", error.to_string())
        })?;
        loop {
            if cancellation.is_cancelled() {
                return Err(SamdebugError::new(
                    ErrorCategory::Interrupted,
                    "INTERRUPTED",
                    "operation interrupted",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn run_internal_setup_cancel(
    cancellation: &CancellationToken,
) -> Result<samdebug_tools::InstallReport, SamdebugError> {
    let root = PathBuf::from(std::env::var_os("SAMDEBUG_TEST_SETUP_ROOT").ok_or_else(|| {
        SamdebugError::new(
            ErrorCategory::Command,
            "TEST_ROOT_MISSING",
            "test root is required",
        )
    })?);
    let ready_file = PathBuf::from(
        std::env::var_os("SAMDEBUG_TEST_SETUP_READY_FILE").ok_or_else(|| {
            SamdebugError::new(
                ErrorCategory::Command,
                "TEST_READY_MISSING",
                "test ready file is required",
            )
        })?,
    );
    let artifact = samdebug_tools::ToolArtifact {
        name: "cancel-fixture".into(),
        version: "1".into(),
        os: std::env::consts::OS.into(),
        architecture: std::env::consts::ARCH.into(),
        url: "https://downloads.example.test/cancel.tar.xz".into(),
        allowed_hosts: vec!["downloads.example.test".into()],
        sha256: "0".repeat(64),
        archive: samdebug_tools::ArchiveSpec {
            kind: "tar.xz".into(),
            root: "bundle".into(),
        },
        executables: vec![samdebug_tools::ExecutableSpec {
            name: "fixture".into(),
            path: "bin/fixture".into(),
            version_args: vec!["--version".into()],
            version_contains: "fixture".into(),
        }],
        licenses: vec![samdebug_tools::LicenseSpec {
            spdx: "MIT".into(),
            path: "LICENSE".into(),
        }],
        source_url: "https://sources.example.test/cancel.tar.xz".into(),
        source_sha256: "1".repeat(64),
        source_offer: "accompanying-source".into(),
    };
    let manifest = ToolManifest {
        schema_version: 1,
        channel: "pinned".into(),
        installable: true,
        reason: None,
        required_tools: vec!["cancel-fixture".into()],
        artifacts: vec![artifact],
    };
    Installer::new(
        root,
        Platform::current(),
        &SlowTestDownloader { ready_file },
        cancellation,
    )
    .install(&manifest, false)
}

fn run_internal_blocking_test(cancellation: &CancellationToken) -> Result<i32, SamdebugError> {
    let runner = SystemProcessRunner;
    let child = runner.spawn(&CommandSpec {
        program: "/bin/sleep".to_owned(),
        args: vec!["30".to_owned()],
        current_dir: None,
    })?;
    let mut supervisor = ChildSupervisor::new(child, Duration::from_secs(1));
    if let Ok(path) = std::env::var("SAMDEBUG_TEST_CHILD_PID_FILE") {
        std::fs::write(
            path,
            supervisor.id().expect("supervisor owns child").to_string(),
        )
        .map_err(|error| {
            SamdebugError::new(
                ErrorCategory::Tool,
                "TEST_PID_FILE_FAILED",
                error.to_string(),
            )
        })?;
    }
    supervisor.wait_until_exit(cancellation, Duration::from_millis(10))
}

fn emit_success(format: OutputFormat, command: &str, data: serde_json::Value) {
    match format {
        OutputFormat::Human => println!("{command}: ok\n{data}"),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string(&FiniteResult::success(command, data))
                .expect("result serializes")
        ),
    }
}

fn emit_failure(format: OutputFormat, command: &str, error: SamdebugError) {
    match format {
        OutputFormat::Human => eprintln!("{command}: {error}"),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string(&FiniteResult::<serde_json::Value>::failure(command, error))
                .expect("result serializes")
        ),
    }
}
