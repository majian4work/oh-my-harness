use anyhow::{Context, Result, bail};

use crate::events::RunState;
use crate::state;
use crate::task::TaskState;
use crate::worktree;

/// Wait for SIGINT or SIGTERM.
/// First signal: returns so caller can do graceful shutdown.
pub async fn wait_for_shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM");
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        ctrl_c.await.ok();
    }
}

/// Graceful shutdown: update run state, release lock.
pub fn graceful_shutdown(run_id: &str) {
    if let Ok(mut run_state) = state::load_state(run_id) {
        // Mark running tasks as pending so they can be resumed
        for task in run_state.graph.tasks.values_mut() {
            if task.state == TaskState::Running {
                task.state = TaskState::Pending;
            }
            if task.state == TaskState::Queued {
                task.state = TaskState::Pending;
            }
        }
        run_state.state = RunState::Interrupted;
        run_state.updated_at = chrono::Utc::now();
        let _ = state::save_state(&run_state);
    }
    state::release_lock(run_id);
}

/// Detect crashed runs (lock file with dead PID) and mark them.
pub fn detect_crashed_runs() -> Result<Vec<String>> {
    let runs = state::list_runs()?;
    let mut crashed = Vec::new();

    for run_id in &runs {
        if let Some(true) = state::is_lock_stale(run_id) {
            // Stale lock → process crashed
            if let Ok(mut run_state) = state::load_state(run_id) {
                if run_state.state == RunState::Running {
                    run_state.state = RunState::Crashed;
                    for task in run_state.graph.tasks.values_mut() {
                        if task.state == TaskState::Running {
                            task.state = TaskState::Pending;
                        }
                        if task.state == TaskState::Queued {
                            task.state = TaskState::Pending;
                        }
                    }
                    run_state.updated_at = chrono::Utc::now();
                    let _ = state::save_state(&run_state);
                    state::release_lock(run_id);
                    crashed.push(run_id.clone());
                }
            }
        }
    }

    Ok(crashed)
}

/// Let user pick a resumable run (crashed or interrupted).
pub fn pick_resumable_run() -> Result<String> {
    // First detect any crashed runs
    let _ = detect_crashed_runs();

    let runs = state::list_runs()?;
    let mut resumable = Vec::new();

    for run_id in &runs {
        if let Ok(run_state) = state::load_state(run_id) {
            match run_state.state {
                RunState::Crashed | RunState::Interrupted => {
                    resumable.push((run_id.clone(), run_state));
                }
                _ => {}
            }
        }
    }

    if resumable.is_empty() {
        bail!("no crashed or interrupted runs to resume");
    }

    eprintln!("Resumable runs:");
    for (i, (run_id, state)) in resumable.iter().enumerate() {
        let summary = state.graph.summary();
        let completed = summary.get(&TaskState::Completed).unwrap_or(&0);
        let total = state.graph.tasks.len();
        eprintln!(
            "  [{}] {} ({}, {}/{} completed)",
            i + 1,
            run_id,
            state.state,
            completed,
            total
        );
    }

    eprint!("Select run [1]: ");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let idx: usize = input.trim().parse().unwrap_or(1);
    let idx = idx.saturating_sub(1);

    resumable
        .get(idx)
        .map(|(id, _)| id.clone())
        .with_context(|| "invalid selection")
}

/// Clean up stale worktrees that have no matching active run.
pub async fn cleanup_stale(force: bool) -> Result<()> {
    let _ = detect_crashed_runs();

    let workspace_root =
        std::env::current_dir().context("failed to determine current directory")?;
    let omt_dir = workspace_root.join(".omt");

    if !omt_dir.exists() {
        eprintln!("No .omt/ directory found. Nothing to clean.");
        return Ok(());
    }

    // Collect active task worktree paths
    let runs = state::list_runs()?;
    let mut active_paths = std::collections::HashSet::new();
    for run_id in &runs {
        if let Ok(run_state) = state::load_state(run_id) {
            if run_state.state == RunState::Running {
                for task in run_state.graph.tasks.values() {
                    if let Some(ref path) = task.worktree_path {
                        active_paths.insert(path.clone());
                    }
                }
            }
        }
    }

    // Scan .omt/ for worktrees
    let mut stale = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&omt_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let path_str = path.to_string_lossy().to_string();
                if !active_paths.contains(&path_str) {
                    stale.push(path);
                }
            }
        }
    }

    if stale.is_empty() {
        eprintln!("No stale worktrees found.");
        return Ok(());
    }

    eprintln!("Found {} stale worktree(s):", stale.len());
    for path in &stale {
        eprintln!("  {}", path.display());
    }

    if !force {
        eprint!("Remove stale worktrees? [y/N] ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            eprintln!("Aborted.");
            return Ok(());
        }
    }

    for path in &stale {
        // Check for uncommitted changes
        if worktree::has_uncommitted_changes(path)? {
            eprintln!("  SKIP {} (has uncommitted changes)", path.display());
            continue;
        }

        match worktree::remove_worktree(path) {
            Ok(()) => eprintln!("  Removed {}", path.display()),
            Err(e) => eprintln!("  FAILED {}: {}", path.display(), e),
        }
    }

    Ok(())
}
