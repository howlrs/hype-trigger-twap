//! Read-only inspection of durable TWAP execution journals.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "hype-twap-runs",
    about = "List, inspect, and verify durable TWAP run journals"
)]
struct Cli {
    /// State root (default: HYPE_TWAP's normal XDG state directory).
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    List,
    Inspect { run_id: String },
    Verify { run_id: String },
}

fn write_json(value: &impl serde::Serialize) -> Result<(), serde_json::Error> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let state_dir = hype_trigger_twap::journal::state_dir(cli.state_dir.as_deref());
    let result = match cli.command {
        Command::List => match hype_trigger_twap::run_reports::list(&state_dir) {
            Ok(report) => write_json(&report).map(|_| ExitCode::SUCCESS),
            Err(_) => write_json(&hype_trigger_twap::run_reports::RunReportError {
                schema_version: hype_trigger_twap::run_reports::RUN_REPORT_SCHEMA_VERSION,
                error: hype_trigger_twap::run_reports::RunReportErrorBody {
                    run_id: "".into(),
                    classification:
                        hype_trigger_twap::run_reports::ValidationClassification::JournalReadError,
                },
            })
            .map(|_| ExitCode::from(1)),
        },
        Command::Inspect { run_id } => write_json(&hype_trigger_twap::run_reports::inspect(
            &state_dir, &run_id,
        ))
        .map(|_| ExitCode::SUCCESS),
        Command::Verify { run_id } => {
            match hype_trigger_twap::run_reports::verify(&state_dir, &run_id) {
                Ok(report) => write_json(&report).map(|_| ExitCode::SUCCESS),
                Err(error) => write_json(&error).map(|_| ExitCode::from(1)),
            }
        }
    };
    result.unwrap_or(ExitCode::from(1))
}
