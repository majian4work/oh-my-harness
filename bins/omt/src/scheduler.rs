use std::collections::HashMap;

use anyhow::Result;
use chrono::Utc;
use tokio_util::sync::CancellationToken;

use crate::events::{OmtBus, OmtEvent, RunState};
use crate::executor;
use crate::recovery;
use crate::retry;
use crate::state;
use crate::task::TaskState;
use crate::team::TeamManager;
use crate::worktree;

pub struct SchedulerConfig {
    pub max_concurrent: usize,
    /// When set, tasks may be dispatched to remote team members via A2A.
    pub team: Option<TeamManager>,
}

/// Main scheduling loop. Runs tasks according to the DAG, respecting
/// concurrency limits, retry policy, and cancellation.
pub async fn run(
    run_id: String,
    config: SchedulerConfig,
    cancel: CancellationToken,
    bus: OmtBus,
) -> Result<()> {
    let mut run_state = state::load_state(&run_id)?;
    run_state.state = RunState::Running;
    run_state.updated_at = Utc::now();
    state::save_state(&run_state)?;
    state::acquire_lock(&run_id)?;

    // Map of task_id → JoinHandle for currently running tasks
    let mut running: HashMap<String, tokio::task::JoinHandle<TaskOutcome>> = HashMap::new();
    // Per-task cancellation tokens
    let mut task_cancels: HashMap<String, CancellationToken> = HashMap::new();

    let run_dir = state::runs_dir().join(&run_id);

    loop {
        // Check for global cancellation
        if cancel.is_cancelled() {
            eprintln!("omt: shutting down gracefully...");
            // Cancel all running tasks
            for (_, ct) in &task_cancels {
                ct.cancel();
            }
            // Wait for them to finish (with timeout)
            for (task_id, handle) in running.drain() {
                match tokio::time::timeout(std::time::Duration::from_secs(30), handle).await {
                    Ok(Ok(outcome)) => {
                        apply_outcome(&mut run_state, &task_id, outcome, &run_dir, &bus);
                    }
                    _ => {
                        if let Some(task) = run_state.graph.tasks.get_mut(&task_id) {
                            task.state = TaskState::Pending;
                        }
                    }
                }
            }
            recovery::graceful_shutdown(&run_id);
            return Ok(());
        }

        // Reload state for retryable tasks check
        let now = Utc::now();

        // Check if budget is exhausted
        if run_state.token_budget > 0 && run_state.tokens_used >= run_state.token_budget {
            if running.is_empty() {
                eprintln!(
                    "omt: token budget exhausted ({}/{})",
                    run_state.tokens_used, run_state.token_budget
                );
                // Cancel remaining tasks
                for (tid, task) in &mut run_state.graph.tasks {
                    if !task.state.is_terminal() {
                        task.state = TaskState::Cancelled;
                        bus.publish(
                            &run_dir,
                            OmtEvent::TaskCancelled {
                                task_id: tid.clone(),
                                reason: "token budget exhausted".to_string(),
                            },
                        );
                    }
                }
                break;
            }
            // Don't launch new tasks, just wait for running ones
        } else {
            // Collect tasks eligible to run: ready (deps met) + retryable (delay passed)
            let mut eligible: Vec<String> = run_state.graph.ready_tasks();
            eligible.extend(run_state.graph.retryable_tasks(now));

            // Launch tasks up to concurrency limit
            let available_slots = config.max_concurrent.saturating_sub(running.len());
            for task_id in eligible.into_iter().take(available_slots) {
                // Mutate task state and extract what we need, then drop the borrow
                let worktree_ok = {
                    let task = match run_state.graph.tasks.get_mut(&task_id) {
                        Some(t) => t,
                        None => continue,
                    };

                    // Set up worktree if not already present
                    if task.worktree_path.is_none() {
                        let branch_name = format!("omt/{}", sanitize_branch(&task.name));
                        match worktree::create_worktree(&task_id, &branch_name) {
                            Ok(path) => {
                                task.worktree_path = Some(path);
                                true
                            }
                            Err(e) => {
                                eprintln!("  [{task_id}] failed to create worktree: {e}");
                                task.state = TaskState::Failed;
                                task.last_error = Some(e.to_string());
                                false
                            }
                        }
                    } else {
                        true
                    }
                };

                // Handle worktree creation failure outside the borrow
                if !worktree_ok {
                    run_state.graph.cascade_cancel(&task_id);
                    state::save_state(&run_state)?;
                    continue;
                }

                // Now mutate task for launch
                let (prompt, agent, worktree_path, continue_session, old_state) = {
                    let task = run_state.graph.tasks.get_mut(&task_id).unwrap();
                    let old_state = task.state;
                    task.state = TaskState::Running;
                    task.attempt_count += 1;
                    task.started_at = Some(now);

                    (
                        task.prompt.clone(),
                        task.agent.clone(),
                        task.worktree_path.clone().unwrap(),
                        task.attempt_count > 1,
                        old_state,
                    )
                };

                let event = OmtEvent::TaskStateChanged {
                    task_id: task_id.clone(),
                    old_state,
                    new_state: TaskState::Running,
                };
                bus.publish(&run_dir, event);

                state::save_state(&run_state)?;

                // Spawn the task
                let task_cancel = CancellationToken::new();
                let task_cancel2 = task_cancel.clone();
                let tid = task_id.clone();
                let task_bus = bus.clone();
                let task_run_dir = run_dir.clone();
                let team_ref = config.team.clone();

                let handle = tokio::spawn(async move {
                    // Try remote dispatch via team if available
                    let remote_endpoint = if let Some(ref team) = team_ref {
                        // Use agent name as role for team matching
                        team.pick_member(&agent).await
                    } else {
                        None
                    };

                    let result = if let Some((instance_id, endpoint)) = remote_endpoint {
                        let team_for_done = team_ref.clone();
                        let tid_cb = tid.clone();
                        let res = executor::run_remote_task(
                            &tid,
                            &prompt,
                            &endpoint,
                            task_cancel2,
                            move |line| {
                                task_bus.publish(
                                    &task_run_dir,
                                    OmtEvent::TaskOutput {
                                        task_id: tid_cb.clone(),
                                        text: line,
                                    },
                                );
                            },
                        )
                        .await;

                        // Release the member slot
                        if let Some(ref team) = team_for_done {
                            team.task_done(&instance_id).await;
                        }

                        res
                    } else {
                        executor::run_omh_task(
                            &prompt,
                            &agent,
                            &worktree_path,
                            continue_session,
                            task_cancel2,
                            move |line| {
                                task_bus.publish(
                                    &task_run_dir,
                                    OmtEvent::TaskOutput {
                                        task_id: tid.clone(),
                                        text: line,
                                    },
                                );
                            },
                        )
                        .await
                    };

                    match result {
                        Ok(r) => TaskOutcome {
                            exit_code: r.exit_code,
                            stdout: r.stdout,
                            stderr: r.stderr,
                            duration_secs: r.duration_secs,
                            input_tokens: r.input_tokens,
                            output_tokens: r.output_tokens,
                        },
                        Err(e) => TaskOutcome {
                            exit_code: -1,
                            stdout: String::new(),
                            stderr: e.to_string(),
                            duration_secs: 0.0,
                            input_tokens: 0,
                            output_tokens: 0,
                        },
                    }
                });

                running.insert(task_id.clone(), handle);
                task_cancels.insert(task_id, task_cancel);
            }
        } // end budget else

        // If nothing is running and no tasks are eligible, we're done
        if running.is_empty() {
            if run_state.graph.is_complete() {
                break;
            }
            // Check for deadlock (all remaining tasks are Pending but have unmet deps)
            let still_pending = run_state
                .graph
                .tasks
                .values()
                .any(|t| t.state == TaskState::Pending || t.state == TaskState::Retrying);
            if !still_pending {
                break;
            }
            // Retrying tasks exist but not yet eligible — wait a bit
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            continue;
        }

        // Wait for any task to complete
        let (completed_id, outcome) = wait_any(&mut running).await;
        task_cancels.remove(&completed_id);

        apply_outcome(&mut run_state, &completed_id, outcome, &run_dir, &bus);
        state::save_state(&run_state)?;
    }

    // All tasks done — merge worktrees and clean up
    finalize(&mut run_state, &run_dir, &bus)?;

    run_state.state = RunState::Finished;
    run_state.updated_at = Utc::now();
    state::save_state(&run_state)?;
    state::release_lock(&run_id);

    // Print summary
    let summary = run_state.graph.summary();
    let completed = summary.get(&TaskState::Completed).unwrap_or(&0);
    let failed = summary.get(&TaskState::Failed).unwrap_or(&0);
    let cancelled = summary.get(&TaskState::Cancelled).unwrap_or(&0);
    let total = run_state.graph.tasks.len();
    eprintln!(
        "\nomt: finished — {completed}/{total} completed, {failed} failed, {cancelled} cancelled"
    );

    Ok(())
}

struct TaskOutcome {
    exit_code: i32,
    stdout: String,
    stderr: String,
    duration_secs: f64,
    input_tokens: u64,
    output_tokens: u64,
}

fn apply_outcome(
    run_state: &mut state::OmtRunState,
    task_id: &str,
    outcome: TaskOutcome,
    run_dir: &std::path::Path,
    bus: &OmtBus,
) {
    let task = match run_state.graph.tasks.get_mut(task_id) {
        Some(t) => t,
        None => return,
    };

    if outcome.exit_code == 0 {
        task.state = TaskState::Completed;
        task.completed_at = Some(Utc::now());
        task.result = Some(truncate(&outcome.stdout, 4096));
        task.input_tokens = outcome.input_tokens;
        task.output_tokens = outcome.output_tokens;

        run_state.tokens_used += outcome.input_tokens + outcome.output_tokens;

        let event = OmtEvent::TaskCompleted {
            task_id: task_id.to_string(),
            duration_secs: outcome.duration_secs,
            input_tokens: outcome.input_tokens,
            output_tokens: outcome.output_tokens,
        };
        bus.publish(run_dir, event);
    } else {
        // Classify and possibly retry
        let error_class = retry::classify_error(outcome.exit_code, &outcome.stderr);
        task.last_error = Some(truncate(&outcome.stderr, 2048));

        let should_retry = retry::should_retry(
            task.attempt_count,
            task.max_attempts,
            error_class,
            run_state.global_retries_used,
            run_state.retry_policy.global_budget,
        );

        if should_retry {
            let delay_ms =
                retry::retry_delay(&run_state.retry_policy, task.attempt_count, error_class);
            task.state = TaskState::Retrying;
            task.next_retry_at = Some(Utc::now() + chrono::Duration::milliseconds(delay_ms as i64));
            run_state.global_retries_used += 1;

            let event = OmtEvent::TaskRetrying {
                task_id: task_id.to_string(),
                attempt: task.attempt_count,
                max_attempts: task.max_attempts,
                delay_ms,
                error: task.last_error.clone().unwrap_or_default(),
            };
            bus.publish(run_dir, event);
        } else {
            task.state = TaskState::Failed;
            task.completed_at = Some(Utc::now());

            let event = OmtEvent::TaskFailed {
                task_id: task_id.to_string(),
                error: task.last_error.clone().unwrap_or_default(),
            };
            bus.publish(run_dir, event);

            // Cancel dependents
            let tid = task_id.to_string();
            run_state.graph.cascade_cancel(&tid);
        }
    }
}

/// Wait for any running task to complete, return its ID and outcome.
async fn wait_any(
    running: &mut HashMap<String, tokio::task::JoinHandle<TaskOutcome>>,
) -> (String, TaskOutcome) {
    loop {
        for (id, handle) in running.iter_mut() {
            if handle.is_finished() {
                let id = id.clone();
                let handle = running.remove(&id).unwrap();
                let outcome = handle.await.unwrap_or(TaskOutcome {
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: "task panicked".to_string(),
                    duration_secs: 0.0,
                    input_tokens: 0,
                    output_tokens: 0,
                });
                return (id, outcome);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// After all tasks complete, merge worktrees back and clean up.
fn finalize(
    run_state: &mut state::OmtRunState,
    run_dir: &std::path::Path,
    bus: &OmtBus,
) -> Result<()> {
    // Merge completed tasks in topological order
    let order = run_state.graph.topological_sort().unwrap_or_default();

    for task_id in &order {
        let task = match run_state.graph.tasks.get(task_id) {
            Some(t) if t.state == TaskState::Completed => t,
            _ => continue,
        };

        let branch_name = format!("omt/{}", sanitize_branch(&task.name));

        match worktree::merge_branch(&branch_name) {
            Ok(worktree::MergeResult::Success) => {
                let event = OmtEvent::MergeCompleted {
                    task_id: task_id.clone(),
                };
                bus.publish(run_dir, event);
            }
            Ok(worktree::MergeResult::Conflict(files)) => {
                let event = OmtEvent::MergeConflict {
                    task_id: task_id.clone(),
                    files,
                };
                bus.publish(run_dir, event);
            }
            Err(e) => {
                eprintln!("  [{task_id}] merge error: {e}");
            }
        }
    }

    // Clean up worktrees and branches
    for task in run_state.graph.tasks.values() {
        if let Some(ref path) = task.worktree_path {
            let _ = worktree::remove_worktree(std::path::Path::new(path));
        }
        let branch_name = format!("omt/{}", sanitize_branch(&task.name));
        let _ = worktree::delete_branch(&branch_name);
    }

    Ok(())
}

fn sanitize_branch(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .to_ascii_lowercase()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...[truncated]", &s[..max])
    }
}
