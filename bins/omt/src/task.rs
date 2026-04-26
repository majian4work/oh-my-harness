use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Unique task identifier (ULID string).
pub type TaskId = String;

/// The state of a single task in the DAG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Waiting for dependencies to complete.
    Pending,
    /// Dependencies satisfied, queued for execution.
    Queued,
    /// Currently executing.
    Running,
    /// Waiting for retry after a transient failure.
    Retrying,
    /// Finished successfully.
    Completed,
    /// Failed after exhausting retries.
    Failed,
    /// Cancelled (dependency failed or user-cancelled).
    Cancelled,
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Queued => write!(f, "queued"),
            Self::Running => write!(f, "running"),
            Self::Retrying => write!(f, "retrying"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// A single task in the execution plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OmtTask {
    pub id: TaskId,
    pub name: String,
    pub prompt: String,
    pub agent: String,
    pub state: TaskState,
    pub depends_on: Vec<TaskId>,

    /// Git worktree path (set when execution begins).
    #[serde(default)]
    pub worktree_path: Option<String>,

    /// omh session id (set when execution begins).
    #[serde(default)]
    pub session_id: Option<String>,

    /// Final result summary (set on completion).
    #[serde(default)]
    pub result: Option<String>,

    // ── Retry fields ────────────────────────────────────
    /// Number of attempts made (starts at 0, incremented on each run).
    #[serde(default)]
    pub attempt_count: u32,

    /// Max attempts allowed (from RetryPolicy, copied at creation).
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,

    /// Last error message (set on failure).
    #[serde(default)]
    pub last_error: Option<String>,

    /// When the next retry is eligible (set on retryable failure).
    #[serde(default)]
    pub next_retry_at: Option<DateTime<Utc>>,

    // ── Timestamps ──────────────────────────────────────
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,

    // ── Token budget ────────────────────────────────────
    /// Max tokens allocated to this task (0 = unlimited).
    #[serde(default)]
    pub token_budget: u64,
    /// Actual input tokens consumed (set on completion).
    #[serde(default)]
    pub input_tokens: u64,
    /// Actual output tokens consumed (set on completion).
    #[serde(default)]
    pub output_tokens: u64,
}

fn default_max_attempts() -> u32 {
    3
}

/// A directed acyclic graph of tasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskGraph {
    pub tasks: HashMap<TaskId, OmtTask>,
    /// Forward edges: task_id → set of tasks that depend on it.
    pub dependents: HashMap<TaskId, HashSet<TaskId>>,
}

impl TaskGraph {
    /// Build a TaskGraph from a list of tasks. Validates no cycles.
    pub fn new(tasks: Vec<OmtTask>) -> anyhow::Result<Self> {
        let mut map = HashMap::with_capacity(tasks.len());
        let mut dependents: HashMap<TaskId, HashSet<TaskId>> = HashMap::new();

        for task in tasks {
            let id = task.id.clone();
            map.insert(id.clone(), task);
            dependents.entry(id).or_default();
        }

        // Build forward-edge map (parent → children who depend on parent).
        for task in map.values() {
            for dep in &task.depends_on {
                if !map.contains_key(dep) {
                    anyhow::bail!("task '{}' depends on unknown task '{}'", task.name, dep);
                }
                dependents
                    .entry(dep.clone())
                    .or_default()
                    .insert(task.id.clone());
            }
        }

        let graph = Self {
            tasks: map,
            dependents,
        };
        graph.validate_no_cycles()?;
        Ok(graph)
    }

    /// Return tasks that are Pending and have all dependencies Completed.
    pub fn ready_tasks(&self) -> Vec<TaskId> {
        self.tasks
            .values()
            .filter(|t| t.state == TaskState::Pending)
            .filter(|t| {
                t.depends_on.iter().all(|dep_id| {
                    self.tasks
                        .get(dep_id)
                        .is_some_and(|d| d.state == TaskState::Completed)
                })
            })
            .map(|t| t.id.clone())
            .collect()
    }

    /// Return tasks in Retrying state whose next_retry_at has passed.
    pub fn retryable_tasks(&self, now: DateTime<Utc>) -> Vec<TaskId> {
        self.tasks
            .values()
            .filter(|t| t.state == TaskState::Retrying)
            .filter(|t| t.next_retry_at.map(|at| now >= at).unwrap_or(true))
            .map(|t| t.id.clone())
            .collect()
    }

    /// Topological sort (Kahn's algorithm). Returns task IDs in execution order.
    pub fn topological_sort(&self) -> anyhow::Result<Vec<TaskId>> {
        let mut in_degree: HashMap<&TaskId, usize> = HashMap::new();
        for task in self.tasks.values() {
            in_degree.entry(&task.id).or_insert(0);
            for _dep in &task.depends_on {
                // dep is depended on by task → task's in-degree increases
            }
        }
        for task in self.tasks.values() {
            *in_degree.entry(&task.id).or_insert(0) += task.depends_on.len();
        }

        let mut queue: VecDeque<&TaskId> = in_degree
            .iter()
            .filter(|&(_, deg)| *deg == 0)
            .map(|(&id, _)| id)
            .collect();

        let mut result = Vec::with_capacity(self.tasks.len());

        while let Some(id) = queue.pop_front() {
            result.push(id.clone());
            if let Some(children) = self.dependents.get(id) {
                for child in children {
                    if let Some(deg) = in_degree.get_mut(child) {
                        *deg -= 1;
                        if *deg == 0 {
                            queue.push_back(child);
                        }
                    }
                }
            }
        }

        if result.len() != self.tasks.len() {
            anyhow::bail!("cycle detected in task graph");
        }

        Ok(result)
    }

    /// Cancel a task and all its transitive dependents.
    pub fn cascade_cancel(&mut self, task_id: &str) {
        let mut to_cancel = VecDeque::new();
        to_cancel.push_back(task_id.to_string());

        while let Some(id) = to_cancel.pop_front() {
            if let Some(task) = self.tasks.get_mut(&id) {
                if !task.state.is_terminal() {
                    task.state = TaskState::Cancelled;
                }
            }
            if let Some(children) = self.dependents.get(&id) {
                for child in children {
                    if let Some(t) = self.tasks.get(child) {
                        if !t.state.is_terminal() {
                            to_cancel.push_back(child.clone());
                        }
                    }
                }
            }
        }
    }

    /// Returns true if all tasks are in a terminal state.
    pub fn is_complete(&self) -> bool {
        self.tasks.values().all(|t| t.state.is_terminal())
    }

    /// Count tasks in each state.
    pub fn summary(&self) -> HashMap<TaskState, usize> {
        let mut counts = HashMap::new();
        for task in self.tasks.values() {
            *counts.entry(task.state).or_insert(0) += 1;
        }
        counts
    }

    fn validate_no_cycles(&self) -> anyhow::Result<()> {
        self.topological_sort().map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_task(id: &str, name: &str, deps: &[&str]) -> OmtTask {
        OmtTask {
            id: id.to_string(),
            name: name.to_string(),
            prompt: format!("do {name}"),
            agent: "orchestrator".to_string(),
            state: TaskState::Pending,
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            worktree_path: None,
            session_id: None,
            result: None,
            attempt_count: 0,
            max_attempts: 3,
            last_error: None,
            next_retry_at: None,
            created_at: Some(Utc::now()),
            started_at: None,
            completed_at: None,
            token_budget: 0,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    #[test]
    fn topological_sort_linear() {
        let tasks = vec![
            make_task("a", "first", &[]),
            make_task("b", "second", &["a"]),
            make_task("c", "third", &["b"]),
        ];
        let graph = TaskGraph::new(tasks).unwrap();
        let order = graph.topological_sort().unwrap();
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn topological_sort_parallel() {
        let tasks = vec![
            make_task("a", "first", &[]),
            make_task("b", "second", &[]),
            make_task("c", "third", &["a", "b"]),
        ];
        let graph = TaskGraph::new(tasks).unwrap();
        let order = graph.topological_sort().unwrap();
        // a and b can be in either order, but c must be last
        assert_eq!(order.last().unwrap(), "c");
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn detect_cycle() {
        let tasks = vec![
            make_task("a", "first", &["b"]),
            make_task("b", "second", &["a"]),
        ];
        assert!(TaskGraph::new(tasks).is_err());
    }

    #[test]
    fn unknown_dependency() {
        let tasks = vec![make_task("a", "first", &["nonexistent"])];
        assert!(TaskGraph::new(tasks).is_err());
    }

    #[test]
    fn ready_tasks() {
        let mut tasks = vec![
            make_task("a", "first", &[]),
            make_task("b", "second", &["a"]),
            make_task("c", "independent", &[]),
        ];
        // a and c have no deps → ready
        let graph = TaskGraph::new(tasks).unwrap();
        let mut ready = graph.ready_tasks();
        ready.sort();
        assert_eq!(ready, vec!["a", "c"]);
    }

    #[test]
    fn cascade_cancel() {
        let tasks = vec![
            make_task("a", "first", &[]),
            make_task("b", "second", &["a"]),
            make_task("c", "third", &["b"]),
        ];
        let mut graph = TaskGraph::new(tasks).unwrap();
        graph.cascade_cancel("a");
        assert_eq!(graph.tasks["a"].state, TaskState::Cancelled);
        assert_eq!(graph.tasks["b"].state, TaskState::Cancelled);
        assert_eq!(graph.tasks["c"].state, TaskState::Cancelled);
    }

    #[test]
    fn is_complete() {
        let tasks = vec![make_task("a", "first", &[]), make_task("b", "second", &[])];
        let mut graph = TaskGraph::new(tasks).unwrap();
        assert!(!graph.is_complete());

        graph.tasks.get_mut("a").unwrap().state = TaskState::Completed;
        graph.tasks.get_mut("b").unwrap().state = TaskState::Failed;
        assert!(graph.is_complete());
    }
}
