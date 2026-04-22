mod bootstrap;
mod devtools;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::{
    bootstrap::parse_log_level,
    devtools::{cmd_diagnose, cmd_eval, cmd_telemetry},
};

#[derive(Debug, Parser)]
#[command(name = "omh-dev", about = "Developer tooling for the omh framework")]
struct Args {
    #[command(subcommand)]
    mode: Mode,

    /// Log level: error, warn, info, debug, trace
    #[arg(long, default_value = "info")]
    log: String,
}

#[derive(Debug, Subcommand)]
enum Mode {
    /// Analyze session dumps for model behavior anomalies
    Diagnose {
        /// Session ID to analyze
        session_id: String,
    },
    /// Summarize turn telemetry collected from sessions
    Telemetry {
        /// Session ID to inspect. If omitted, summarizes the most recent sessions.
        session_id: Option<String>,
        /// Max recent sessions to summarize when no session_id is given
        #[arg(short, long, default_value = "20")]
        limit: usize,
    },
    /// Run task eval cases from a TOML file or directory
    Eval {
        /// Eval TOML file or directory. Defaults to tests/evals
        path: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    omh_trace::init(parse_log_level(&args.log));

    match args.mode {
        Mode::Diagnose { session_id } => cmd_diagnose(&session_id).await,
        Mode::Telemetry { session_id, limit } => cmd_telemetry(session_id.as_deref(), limit).await,
        Mode::Eval { path } => cmd_eval(path.as_deref()).await,
    }
}
