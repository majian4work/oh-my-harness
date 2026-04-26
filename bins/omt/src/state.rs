use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::events::{OmtEvent, RunState};
use crate::retry::RetryPolicy;
use crate::task::{OmtTask, TaskGraph};

/// Persistent state of an omt run.
#[derive(Debug, Serialize, Deserialize)]
pub struct OmtRunState {
    pub run_id: String,
    pub prompt: String,
    pub state: RunState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub graph: TaskGraph,
    pub retry_policy: RetryPolicy,
    pub global_retries_used: u32,
    /// Total token budget for this run (0 = unlimited).
    #[serde(default)]
    pub token_budget: u64,
    /// Total tokens consumed across all tasks.
    #[serde(default)]
    pub tokens_used: u64,
}

/// Base directory for omt run state: `~/.cache/omt/runs`.
/// Base directory for omt run state: `~/.cache/omt/runs`.
///
/// `dirs::cache_dir()` returns `~/.cache/omh` (the workspace `dirs` crate),
/// so we go up one level to `~/.cache` then join `omt/runs`.
pub fn runs_dir() -> PathBuf {
    dirs::cache_dir()
        .parent()
        .unwrap_or(Path::new("."))
        .join("omt")
        .join("runs")
}

fn run_dir(run_id: &str) -> PathBuf {
    runs_dir().join(run_id)
}

fn state_path(run_id: &str) -> PathBuf {
    run_dir(run_id).join("state.json")
}

fn lock_path(run_id: &str) -> PathBuf {
    run_dir(run_id).join("lock")
}

/// Create a new run from a plan (list of tasks).
pub fn create_run(tasks: &[OmtTask]) -> Result<String> {
    let run_id = ulid::Ulid::new().to_string().to_ascii_lowercase();
    let dir = run_dir(&run_id);
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create run dir: {}", dir.display()))?;

    let graph = TaskGraph::new(tasks.to_vec())?;
    let policy = RetryPolicy::with_task_count(tasks.len());

    let total_budget: u64 = std::env::var("OMT_TOKEN_BUDGET")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let run_state = OmtRunState {
        run_id: run_id.clone(),
        prompt: tasks.first().map(|t| t.prompt.clone()).unwrap_or_default(),
        state: RunState::Planned,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        graph,
        retry_policy: policy,
        global_retries_used: 0,
        token_budget: total_budget,
        tokens_used: 0,
    };

    save_state(&run_state)?;
    acquire_lock(&run_id)?;

    crate::events::log_event(
        &dir,
        &OmtEvent::PlanGenerated {
            run_id: run_id.clone(),
            task_count: tasks.len(),
        },
    );

    Ok(run_id)
}

/// Load run state from disk.
pub fn load_state(run_id: &str) -> Result<OmtRunState> {
    let path = state_path(run_id);
    let data = fs::read_to_string(&path)
        .with_context(|| format!("failed to read state: {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| "failed to parse run state")
}

/// Save run state to disk (atomic write via temp file).
pub fn save_state(run_state: &OmtRunState) -> Result<()> {
    let dir = run_dir(&run_state.run_id);
    fs::create_dir_all(&dir)?;

    let path = state_path(&run_state.run_id);
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_string_pretty(run_state)?;

    fs::write(&tmp, data.as_bytes())
        .with_context(|| format!("failed to write temp state: {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("failed to rename state file: {}", path.display()))?;

    Ok(())
}

/// Acquire a lock file with the current PID.
pub fn acquire_lock(run_id: &str) -> Result<()> {
    let path = lock_path(run_id);
    let mut f = fs::File::create(&path)?;
    write!(f, "{}", std::process::id())?;
    Ok(())
}

/// Release the lock file.
pub fn release_lock(run_id: &str) {
    let _ = fs::remove_file(lock_path(run_id));
}

/// Check if a lock file exists and if the PID is still alive.
pub fn is_lock_stale(run_id: &str) -> Option<bool> {
    let path = lock_path(run_id);
    match fs::read_to_string(&path) {
        Ok(pid_str) => {
            let pid: u32 = pid_str.trim().parse().ok()?;
            // Check if process is alive via kill(pid, 0)
            let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
            Some(!alive)
        }
        Err(_) => None, // No lock file
    }
}

/// List all run IDs, most recent first.
pub fn list_runs() -> Result<Vec<String>> {
    let dir = runs_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut runs: Vec<(String, std::time::SystemTime)> = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            let modified = entry
                .metadata()?
                .modified()
                .unwrap_or(std::time::UNIX_EPOCH);
            runs.push((name, modified));
        }
    }

    runs.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(runs.into_iter().map(|(name, _)| name).collect())
}

/// Print a summary of all runs.
pub fn print_runs() -> Result<()> {
    let runs = list_runs()?;
    if runs.is_empty() {
        eprintln!("No omt runs found.");
        return Ok(());
    }

    eprintln!("{:<30} {:<12} {:<8}", "RUN ID", "STATE", "TASKS");
    eprintln!("{}", "-".repeat(54));

    for run_id in &runs {
        match load_state(run_id) {
            Ok(state) => {
                let task_count = state.graph.tasks.len();
                let summary = state.graph.summary();
                let completed = summary
                    .get(&crate::task::TaskState::Completed)
                    .unwrap_or(&0);
                eprintln!(
                    "{:<30} {:<12} {}/{}",
                    run_id, state.state, completed, task_count
                );
            }
            Err(_) => {
                eprintln!("{:<30} {:<12} {:<8}", run_id, "corrupt", "?");
            }
        }
    }

    Ok(())
}
