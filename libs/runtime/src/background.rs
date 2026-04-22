use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use bus::{AgentEvent, EventBus};
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

pub type TaskId = String;

pub struct BackgroundTaskManager {
    tasks: Arc<Mutex<HashMap<TaskId, TaskHandle>>>,
    max_concurrent: usize,
    results: Arc<Mutex<HashMap<TaskId, Option<std::result::Result<String, String>>>>>,
    bus: EventBus,
}

pub struct TaskHandle {
    pub id: TaskId,
    pub agent_name: String,
    pub session_id: String,
    pub cancel_token: CancellationToken,
    pub status: TaskStatus,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed(String),
    Cancelled,
}

impl BackgroundTaskManager {
    pub fn new(max_concurrent: usize, bus: EventBus) -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
            max_concurrent,
            results: Arc::new(Mutex::new(HashMap::new())),
            bus,
        }
    }

    pub fn spawn<F>(&self, agent_name: String, session_id: String, fut: F) -> Result<TaskId>
    where
        F: Future<Output = Result<String>> + Send + 'static,
    {
        let active = self
            .tasks
            .lock()
            .map_err(|_| anyhow!("background task registry lock poisoned"))?
            .values()
            .filter(|task| matches!(task.status, TaskStatus::Pending | TaskStatus::Running))
            .count();

        if active >= self.max_concurrent {
            bail!(
                "background task limit reached: {active} active, max {}",
                self.max_concurrent
            );
        }

        let id = Ulid::new().to_string();
        let cancel_token = CancellationToken::new();

        self.tasks
            .lock()
            .map_err(|_| anyhow!("background task registry lock poisoned"))?
            .insert(
                id.clone(),
                TaskHandle {
                    id: id.clone(),
                    agent_name: agent_name.clone(),
                    session_id: session_id.clone(),
                    cancel_token: cancel_token.clone(),
                    status: TaskStatus::Running,
                },
            );

        self.results
            .lock()
            .map_err(|_| anyhow!("background task result registry lock poisoned"))?
            .insert(id.clone(), None);

        self.bus.publish(AgentEvent::SubagentSpawned {
            parent_id: String::new(),
            child_id: id.clone(),
            agent: agent_name.clone(),
        });

        let tasks = Arc::clone(&self.tasks);
        let results = Arc::clone(&self.results);
        let task_id = id.clone();
        let task_cancel = cancel_token.clone();
        let bus = self.bus.clone();

        tokio::spawn(async move {
            tokio::pin!(fut);

            let outcome = tokio::select! {
                _ = task_cancel.cancelled() => Err(anyhow!("cancelled")),
                result = &mut fut => result,
            };

            if let Ok(mut handles) = tasks.lock() {
                if let Some(handle) = handles.get_mut(&task_id) {
                    handle.status = match &outcome {
                        Ok(_) => TaskStatus::Completed,
                        Err(error) if error.to_string() == "cancelled" => TaskStatus::Cancelled,
                        Err(error) => TaskStatus::Failed(error.to_string()),
                    };
                }
            }

            let (stored, bus_event) = match outcome {
                Ok(output) => {
                    let event = AgentEvent::SubagentCompleted {
                        child_id: task_id.clone(),
                        result: output.clone(),
                    };
                    (Some(Ok(output)), Some(event))
                }
                Err(error) if error.to_string() == "cancelled" => (None, None),
                Err(error) => {
                    let msg = error.to_string();
                    let event = AgentEvent::SubagentFailed {
                        child_id: task_id.clone(),
                        error: msg.clone(),
                    };
                    (Some(Err(msg)), Some(event))
                }
            };

            if let Ok(mut stored_results) = results.lock() {
                stored_results.insert(task_id, stored);
            }

            if let Some(event) = bus_event {
                bus.publish(event);
            }
        });

        Ok(id)
    }

    pub fn cancel(&self, id: &str) -> Result<()> {
        let mut tasks = self
            .tasks
            .lock()
            .map_err(|_| anyhow!("background task registry lock poisoned"))?;
        let Some(task) = tasks.get_mut(id) else {
            bail!("background task not found: {id}");
        };

        task.cancel_token.cancel();
        if matches!(task.status, TaskStatus::Pending | TaskStatus::Running) {
            task.status = TaskStatus::Cancelled;
        }

        Ok(())
    }

    pub fn status(&self, id: &str) -> Option<TaskStatus> {
        self.tasks
            .lock()
            .ok()?
            .get(id)
            .map(|task| task.status.clone())
    }

    pub fn result(&self, id: &str) -> Option<Result<String>> {
        let stored = self.results.lock().ok()?.get_mut(id)?.take()?;
        Some(match stored {
            Ok(output) => Ok(output),
            Err(error) => Err(anyhow!(error)),
        })
    }

    pub fn list(&self) -> Vec<(TaskId, String, TaskStatus)> {
        let Ok(tasks) = self.tasks.lock() else {
            return Vec::new();
        };

        tasks
            .values()
            .map(|task| {
                (
                    task.id.clone(),
                    task.agent_name.clone(),
                    task.status.clone(),
                )
            })
            .collect()
    }

    /// Wait for all running/pending tasks to complete (or fail/cancel).
    /// Returns the number of tasks that were waited on.
    pub async fn wait_all(&self, timeout: std::time::Duration) -> usize {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut waited = 0usize;
        loop {
            let pending: Vec<TaskId> = {
                let Ok(tasks) = self.tasks.lock() else {
                    break;
                };
                tasks
                    .values()
                    .filter(|t| matches!(t.status, TaskStatus::Pending | TaskStatus::Running))
                    .map(|t| t.id.clone())
                    .collect()
            };
            if pending.is_empty() {
                break;
            }
            waited = pending.len();
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!("wait_all timed out with {} tasks still running", pending.len());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        waited
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn spawn_transitions_to_completed_and_returns_result() {
        let manager = BackgroundTaskManager::new(2, bus::EventBus::new(16));
        let task_id = manager
            .spawn("worker".to_string(), "ses_1".to_string(), async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok("done".to_string())
            })
            .unwrap();

        assert_eq!(manager.status(&task_id), Some(TaskStatus::Running));

        tokio::time::sleep(Duration::from_millis(40)).await;

        assert_eq!(manager.status(&task_id), Some(TaskStatus::Completed));
        assert_eq!(manager.result(&task_id).unwrap().unwrap(), "done");
        assert!(manager.result(&task_id).is_none());
    }

    #[tokio::test]
    async fn cancel_transitions_to_cancelled() {
        let manager = BackgroundTaskManager::new(1, bus::EventBus::new(16));
        let task_id = manager
            .spawn("worker".to_string(), "ses_2".to_string(), async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok("late".to_string())
            })
            .unwrap();

        manager.cancel(&task_id).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert_eq!(manager.status(&task_id), Some(TaskStatus::Cancelled));
        assert!(manager.result(&task_id).is_none());
    }
}
