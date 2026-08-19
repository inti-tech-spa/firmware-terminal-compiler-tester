use std::{path::PathBuf, process::ExitCode};

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum, error::ErrorKind};
use samdebug_core::{CancellationToken, ErrorCategory, FiniteResult, SamdebugError};
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
            let code = error.exit_code;
            emit_failure(cli.output, command, error);
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        }
    }
}

fn parse_cli() -> Result<Cli, ExitCode> {
    match Cli::try_parse() {
        Ok(cli) => Ok(cli),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            print!("{error}");
            Err(ExitCode::SUCCESS)
        }
        Err(error) => {
            let json_requested = std::env::args()
                .collect::<Vec<_>>()
                .windows(2)
                .any(|pair| pair == ["--output", "json"]);
            if json_requested {
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
        Command::Debug(args) if !args.agent => samdebug_tui::run()
            .map(|()| ("debug", json!({})))
            .map_err(|error| ("debug", error)),
        Command::Erase(args) => authorized_reserved_command("erase", args),
        Command::Flash(args) => authorized_reserved_command("flash", args),
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
    }
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
