use std::{ffi::OsString, path::PathBuf, process::ExitCode, time::Duration};

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum, error::ErrorKind};
use samdebug_core::{
    CancellationToken, ErrorCategory, FiniteResult, SamdebugError,
    ports::{CommandSpec, ProcessRunner},
};
use samdebug_tools::{
    ChildSupervisor, CurlDownloader, Installer, MacUsbProbeProvider, Platform, SystemProcessRunner,
    ToolManifest, run_doctor,
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
        Command::Setup { offline } => setup_command(*offline)
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
        Command::Debug(args) if !args.agent => samdebug_tui::run()
            .map(|()| ("debug", json!({})))
            .map_err(|error| ("debug", error)),
        Command::Erase(args) => authorized_reserved_command("erase", args),
        Command::Flash(args) => authorized_reserved_command("flash", args),
        Command::InternalTestBlock => run_internal_blocking_test(cancellation)
            .map(|code| ("__test-block", json!({"child_exit_code": code})))
            .map_err(|error| ("__test-block", error)),
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

fn setup_command(offline: bool) -> Result<samdebug_tools::InstallReport, SamdebugError> {
    let manifest = embedded_manifest()?;
    Installer::new(managed_root()?, Platform::current(), &CurlDownloader)
        .install(&manifest, offline)
}

fn doctor_command() -> Result<samdebug_tools::DoctorReport, SamdebugError> {
    let manifest = embedded_manifest()?;
    run_doctor(
        &manifest,
        &managed_root()?,
        &Platform::current(),
        &MacUsbProbeProvider,
        &SystemProcessRunner,
    )
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

fn authorized_reserved_command(
    operation: &'static str,
    args: &AuthorizedArgs,
) -> Result<(&'static str, serde_json::Value), (&'static str, SamdebugError)> {
    validate_authorization(operation, args).map_err(|error| (operation, error))?;
    Err((
        operation,
        SamdebugError::new(
            ErrorCategory::Programming,
            "NOT_IMPLEMENTED",
            "authorization accepted, but programming is reserved for milestone M5",
        ),
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
    }
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

fn validate_authorization(operation: &str, args: &AuthorizedArgs) -> Result<(), SamdebugError> {
    let expected = format!("{operation}:{}", args.probe);
    if args.confirm == expected {
        Ok(())
    } else {
        Err(SamdebugError::new(
            ErrorCategory::Authorization,
            "AUTHORIZATION_REJECTED",
            format!("expected --confirm {expected}"),
        ))
    }
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
