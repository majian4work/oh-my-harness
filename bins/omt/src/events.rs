use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::task::{TaskId, TaskState};

/// Events emitted by the omt scheduler.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OmtEvent {
    /// A new run has been created with a plan.
    PlanGenerated { run_id: String, task_count: usize },

    /// A task's state changed.
    TaskStateChanged {
        task_id: TaskId,
        old_state: TaskState,
        new_state: TaskState,
    },

    /// A task is being retried after a transient failure.
    TaskRetrying {
        task_id: TaskId,
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error: String,
    },

    /// Streaming output from a running task (parsed from omh stderr).
    TaskOutput { task_id: TaskId, text: String },

    /// A task completed successfully.
    TaskCompleted {
        task_id: TaskId,
        duration_secs: f64,
        input_tokens: u64,
        output_tokens: u64,
    },

    /// A task failed permanently.
    TaskFailed { task_id: TaskId, error: String },

    /// A task was cancelled (dependency failed or user-cancelled).
    TaskCancelled { task_id: TaskId, reason: String },

    /// Run state changed (for crash recovery tracking).
    RunStateChanged { run_id: String, state: RunState },

    /// Stale worktree detected during cleanup.
    StaleWorktreeDetected { path: String, action: String },

    /// Git merge completed for a task's worktree.
    MergeCompleted { task_id: TaskId },

    /// Git merge conflict detected.
    MergeConflict { task_id: TaskId, files: Vec<String> },
}

/// Overall state of an omt run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Plan created, not yet executing.
    Planned,
    /// Tasks are being executed.
    Running,
    /// All tasks completed (some may have failed).
    Finished,
    /// Gracefully interrupted by user (SIGINT).
    Interrupted,
    /// Crashed (lock file with dead PID).
    Crashed,
}

impl std::fmt::Display for RunState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Planned => write!(f, "planned"),
            Self::Running => write!(f, "running"),
            Self::Finished => write!(f, "finished"),
            Self::Interrupted => write!(f, "interrupted"),
            Self::Crashed => write!(f, "crashed"),
        }
    }
}

/// Log an event to the run's events.jsonl file.
pub fn log_event(run_dir: &std::path::Path, event: &OmtEvent) {
    let path = run_dir.join("events.jsonl");
    let line = serde_json::to_string(event).unwrap_or_default();

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{}", line);
    }
}

/// Print an event to stderr for headless mode.
pub fn print_event(event: &OmtEvent) {
    match event {
        OmtEvent::TaskStateChanged {
            task_id, new_state, ..
        } => {
            eprintln!("  [{task_id}] → {new_state}");
        }
        OmtEvent::TaskRetrying {
            task_id,
            attempt,
            max_attempts,
            delay_ms,
            error,
        } => {
            eprintln!("  [{task_id}] retrying ({attempt}/{max_attempts}) in {delay_ms}ms: {error}");
        }
        OmtEvent::TaskCompleted {
            task_id,
            duration_secs,
            input_tokens,
            output_tokens,
        } => {
            let token_info = if *input_tokens > 0 || *output_tokens > 0 {
                format!(" │ {input_tokens}in + {output_tokens}out tokens")
            } else {
                String::new()
            };
            eprintln!("  [{task_id}] completed in {duration_secs:.1}s{token_info}");
        }
        OmtEvent::TaskFailed { task_id, error } => {
            eprintln!("  [{task_id}] FAILED: {error}");
        }
        OmtEvent::TaskCancelled { task_id, reason } => {
            eprintln!("  [{task_id}] cancelled: {reason}");
        }
        OmtEvent::MergeConflict { task_id, files } => {
            eprintln!("  [{task_id}] merge conflict in: {}", files.join(", "));
        }
        _ => {}
    }
}

/// Broadcast-based event bus for omt events.
#[derive(Clone)]
pub struct OmtBus {
    sender: broadcast::Sender<OmtEvent>,
}

impl OmtBus {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(256);
        Self { sender }
    }

    /// Publish an event to all subscribers and log to disk.
    pub fn publish(&self, run_dir: &std::path::Path, event: OmtEvent) {
        log_event(run_dir, &event);
        let _ = self.sender.send(event);
    }

    /// Subscribe to events.
    pub fn subscribe(&self) -> broadcast::Receiver<OmtEvent> {
        self.sender.subscribe()
    }
}
