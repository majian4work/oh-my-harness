mod auth;
mod commands;
mod run;
mod slash;
mod tui;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use runtime::Harness;
use tracing::Level;

use crate::{
    commands::{cmd_auth, cmd_evolution, cmd_memory, cmd_sessions, cmd_update_best_models},
    run::run_oneshot,
    tui::run_tui,
};

#[derive(Debug, Subcommand)]
pub enum MemoryCmd {
    List,
    Search { query: String },
    Add { content: String },
    Forget { id: String },
}

#[derive(Debug, Subcommand)]
pub enum EvolutionCmd {
    Log,
    Revert { id: String },
    Consolidate,
    Pause,
    Resume,
}

#[derive(Debug, Subcommand)]
pub enum AuthCmd {
    Login {
        provider: Option<String>,
        #[arg(short, long)]
        key: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
    Logout {
        provider: String,
    },
    List,
    Status,
}

pub fn parse_log_level(log: &str) -> Level {
    match log.to_ascii_lowercase().as_str() {
        "error" => Level::ERROR,
        "warn" => Level::WARN,
        "info" => Level::INFO,
        "debug" => Level::DEBUG,
        "trace" => Level::TRACE,
        _ => Level::INFO,
    }
}

pub fn init_harness() -> Result<Harness> {
    let workspace_root: PathBuf =
        std::env::current_dir().context("failed to determine current directory")?;
    Harness::init(&workspace_root).with_context(|| {
        format!(
            "failed to initialize harness at {}",
            workspace_root.display()
        )
    })
}

#[derive(Debug, Parser)]
#[command(name = "omh", about = "The orchestration framework for AI agents")]
struct Args {
    #[command(subcommand)]
    mode: Option<Mode>,

    /// Log level: error, warn, info, debug, trace
    #[arg(long, default_value = "info")]
    log: String,

    /// Continue the most recent session
    #[arg(short, long)]
    r#continue: bool,
}

#[derive(Debug, Subcommand)]
enum Mode {
    /// Terminal UI (default when no command given)
    Tui {
        /// Resume a specific session (interactive selection)
        #[arg(short, long)]
        resume: bool,
    },
    /// One-shot non-interactive run
    Run {
        /// The prompt to send
        prompt: String,
        /// Agent to use
        #[arg(short, long, default_value = "orchestrator")]
        agent: String,
    },
    /// Provider authentication management
    #[command(subcommand)]
    Auth(AuthCmd),
    /// List recent sessions
    Sessions {
        /// Max sessions to show
        #[arg(short, long, default_value = "20")]
        limit: usize,
    },
    /// Memory management
    #[command(subcommand)]
    Memory(MemoryCmd),
    /// Evolution management
    #[command(subcommand)]
    Evolution(EvolutionCmd),
    /// Refresh model cache and optimize agent model assignments
    UpdateBestModels {
        #[arg(short, long)]
        global: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let log_level = parse_log_level(&args.log);

    match args.mode {
        None | Some(Mode::Tui { .. }) => {
            omh_trace::init_file(log_level);
            let resume = match &args.mode {
                Some(Mode::Tui { resume }) => *resume,
                _ => false,
            };
            run_tui(args.r#continue, resume).await
        }
        _ => {
            omh_trace::init(log_level);
            match args.mode.unwrap() {
                Mode::Tui { .. } => unreachable!(),
                Mode::Run { prompt, agent } => run_oneshot(&prompt, &agent, args.r#continue).await,
                Mode::Sessions { limit } => cmd_sessions(limit).await,
                Mode::Memory(cmd) => cmd_memory(cmd).await,
                Mode::Evolution(cmd) => cmd_evolution(cmd).await,
                Mode::Auth(cmd) => cmd_auth(cmd).await,
                Mode::UpdateBestModels { global } => cmd_update_best_models(global).await,
            }
        }
    }
}
