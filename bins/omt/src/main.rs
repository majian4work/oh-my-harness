mod api;
mod events;
mod executor;
mod planner;
mod recovery;
mod registry;
mod retry;
mod scheduler;
mod state;
mod task;
mod team;
mod tui;
mod worktree;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::events::OmtBus;

#[derive(Debug, Parser)]
#[command(name = "omt", about = "oh-my-team: multi-agent task orchestrator")]
struct Args {
    #[command(subcommand)]
    mode: Mode,

    /// Log level: error, warn, info, debug, trace
    #[arg(long, default_value = "info")]
    log: String,
}

#[derive(Debug, Subcommand)]
enum Mode {
    /// Interactive TUI dashboard (default)
    Tui {
        /// Max concurrent omh processes
        #[arg(short, long, default_value_t = num_cpus())]
        concurrency: usize,
    },
    /// Decompose a prompt into parallel tasks and execute (headless)
    Run {
        /// The prompt describing the work
        prompt: String,

        /// Max concurrent omh processes
        #[arg(short, long, default_value_t = num_cpus())]
        concurrency: usize,

        /// Auto-approve the generated plan without user confirmation
        #[arg(long)]
        auto_approve: bool,
    },
    /// Resume a crashed or interrupted run
    Resume {
        /// Run ID to resume (interactive selection if omitted)
        run_id: Option<String>,
    },
    /// List recent runs and their status
    Status,
    /// Clean up stale worktrees and orphaned run state
    Cleanup {
        /// Remove without prompting
        #[arg(long)]
        force: bool,
    },
    /// Start omt as an A2A server so other agents can send tasks to it
    Serve {
        /// Address to bind (e.g. 0.0.0.0:8080)
        #[arg(short, long, default_value = "127.0.0.1:9120")]
        bind: String,

        /// Max concurrent omh processes per task
        #[arg(short, long, default_value_t = num_cpus())]
        concurrency: usize,
    },
    /// Manage registered A2A agents
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
}

#[derive(Debug, Subcommand)]
enum AgentAction {
    /// Discover and register a remote A2A agent
    Add {
        /// Base URL of the agent (e.g. http://host:8080)
        url: String,
    },
    /// List all registered agents
    List,
    /// Remove a registered agent
    Remove {
        /// Agent name
        name: String,
    },
    /// Health-check all registered agents
    Check,
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().max(2) / 2)
        .unwrap_or(2)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    match args.mode {
        Mode::Tui { concurrency } => {
            let team = api::spawn_background(concurrency)
                .await
                .ok()
                .map(|(_, t)| t);
            tui::run_tui(concurrency, team).await
        }
        Mode::Run {
            prompt,
            concurrency,
            auto_approve,
        } => {
            let team = api::spawn_background(concurrency)
                .await
                .ok()
                .map(|(_, t)| t);
            eprintln!("omt: planning tasks for prompt...");
            let plan = planner::plan(&prompt).await?;

            if !auto_approve {
                planner::print_plan(&plan);
                eprint!("Approve plan? [Y/n] ");
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                if input.trim().eq_ignore_ascii_case("n") {
                    eprintln!("Aborted.");
                    return Ok(());
                }
            }

            let run_id = state::create_run(&plan)?;
            let config = scheduler::SchedulerConfig {
                max_concurrent: concurrency,
                team,
            };
            let bus = OmtBus::new();

            // Print events to stderr in headless mode
            let mut rx = bus.subscribe();
            tokio::spawn(async move {
                while let Ok(event) = rx.recv().await {
                    events::print_event(&event);
                }
            });

            // Install signal handler for graceful shutdown
            let cancel = tokio_util::sync::CancellationToken::new();
            let cancel2 = cancel.clone();
            tokio::spawn(async move {
                recovery::wait_for_shutdown_signal().await;
                cancel2.cancel();
            });

            scheduler::run(run_id, config, cancel, bus).await
        }
        Mode::Resume { run_id } => {
            let run_id = match run_id {
                Some(id) => id,
                None => recovery::pick_resumable_run()?,
            };
            let config = scheduler::SchedulerConfig {
                max_concurrent: num_cpus(),
                team: None,
            };
            let bus = OmtBus::new();
            let mut rx = bus.subscribe();
            tokio::spawn(async move {
                while let Ok(event) = rx.recv().await {
                    events::print_event(&event);
                }
            });
            let cancel = tokio_util::sync::CancellationToken::new();
            let cancel2 = cancel.clone();
            tokio::spawn(async move {
                recovery::wait_for_shutdown_signal().await;
                cancel2.cancel();
            });
            scheduler::run(run_id, config, cancel, bus).await
        }
        Mode::Status => {
            state::print_runs()?;
            Ok(())
        }
        Mode::Cleanup { force } => recovery::cleanup_stale(force).await,
        Mode::Serve { bind, concurrency } => {
            eprintln!("omt: starting A2A server on {bind}");
            api::serve(&bind, concurrency).await
        }
        Mode::Agent { action } => match action {
            AgentAction::Add { url } => registry::add_agent(&url).await,
            AgentAction::List => registry::list_agents(),
            AgentAction::Remove { name } => registry::remove_agent(&name),
            AgentAction::Check => registry::check_agents().await,
        },
    }
}
