use std::process::Stdio;
use std::time::Instant;

use anyhow::{Result, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Result of executing a single omh task.
pub struct TaskResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_secs: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Execute `omh run` as a child process in the given worktree.
///
/// Streams stderr lines via `on_output` callback for real-time progress.
pub async fn run_omh_task(
    prompt: &str,
    agent: &str,
    worktree_path: &str,
    continue_session: bool,
    cancel: CancellationToken,
    on_output: impl Fn(String) + Send + 'static,
) -> Result<TaskResult> {
    let omh_bin = find_omh_binary()?;

    let mut args = vec!["run".to_string(), prompt.to_string()];
    args.push("--agent".to_string());
    args.push(agent.to_string());

    let mut cmd = Command::new(&omh_bin);
    if continue_session {
        cmd.arg("--continue");
    }
    cmd.args(&args)
        .current_dir(worktree_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;

    let stderr_pipe = child.stderr.take().expect("stderr piped");
    let stdout_pipe = child.stdout.take().expect("stdout piped");

    // Stream stderr for progress
    let stderr_handle = tokio::spawn(async move {
        let mut lines = Vec::new();
        let reader = BufReader::new(stderr_pipe);
        let mut line_stream = reader.lines();
        while let Ok(Some(line)) = line_stream.next_line().await {
            on_output(line.clone());
            lines.push(line);
        }
        lines.join("\n")
    });

    // Collect stdout
    let stdout_handle = tokio::spawn(async move {
        let reader = BufReader::new(stdout_pipe);
        let mut lines = Vec::new();
        let mut line_stream = reader.lines();
        while let Ok(Some(line)) = line_stream.next_line().await {
            lines.push(line);
        }
        lines.join("\n")
    });

    let start = Instant::now();

    // Wait for process or cancellation
    let status = tokio::select! {
        status = child.wait() => status?,
        _ = cancel.cancelled() => {
            // Kill the child process
            child.kill().await.ok();
            child.wait().await?
        }
    };

    let duration_secs = start.elapsed().as_secs_f64();
    let stderr = stderr_handle.await.unwrap_or_default();
    let stdout = stdout_handle.await.unwrap_or_default();
    let exit_code = status.code().unwrap_or(-1);

    let (input_tokens, output_tokens) = parse_token_usage(&stderr);

    Ok(TaskResult {
        exit_code,
        stdout,
        stderr,
        duration_secs,
        input_tokens,
        output_tokens,
    })
}

/// Parse omh's stderr summary line for token counts.
///
/// Format: `─── N tool call(s) (Xs) │ Nin + Nout tokens │ Xs ───`
fn parse_token_usage(stderr: &str) -> (u64, u64) {
    for line in stderr.lines().rev() {
        if let Some(tok_part) = line.split('│').nth(1) {
            // "  1234in + 567out tokens  "
            let tok_part = tok_part.trim();
            let mut input = 0u64;
            let mut output = 0u64;
            for word in tok_part.split_whitespace() {
                if let Some(n) = word.strip_suffix("in") {
                    input = n.parse().unwrap_or(0);
                } else if let Some(n) = word.strip_suffix("out") {
                    output = n.parse().unwrap_or(0);
                }
            }
            if input > 0 || output > 0 {
                return (input, output);
            }
        }
    }
    (0, 0)
}

/// Find the omh binary. Looks for it next to the omt binary first, then in PATH.
fn find_omh_binary() -> Result<String> {
    // Try sibling of current executable
    if let Ok(current) = std::env::current_exe() {
        let dir = current.parent().unwrap_or(std::path::Path::new("."));
        let candidate = dir.join("omh");
        if candidate.exists() {
            return Ok(candidate.to_string_lossy().to_string());
        }
    }

    // Try PATH
    let output = std::process::Command::new("which").arg("omh").output();

    if let Ok(output) = output {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(path);
            }
        }
    }

    // Fallback: try cargo build target
    let workspace_root = std::env::current_dir()?;
    let release = workspace_root.join("target").join("release").join("omh");
    if release.exists() {
        return Ok(release.to_string_lossy().to_string());
    }

    // Try musl target
    let musl = workspace_root
        .join("target")
        .join("x86_64-unknown-linux-musl")
        .join("release")
        .join("omh");
    if musl.exists() {
        return Ok(musl.to_string_lossy().to_string());
    }

    bail!("omh binary not found. Build it first with: cargo build -r -p omh");
}

/// Execute a task on a remote omh agent via A2A `tasks/send`.
///
/// Returns a `TaskResult` mirroring the local execution interface.
pub async fn run_remote_task(
    task_id: &str,
    prompt: &str,
    endpoint: &str,
    cancel: CancellationToken,
    on_output: impl Fn(String) + Send + 'static,
) -> Result<TaskResult> {
    let client = a2a::A2aClient::new();
    let params = a2a::TaskSendParams {
        id: task_id.to_string(),
        session_id: None,
        message: a2a::Message::user_text(prompt),
        metadata: None,
    };

    on_output(format!("→ dispatching to remote agent at {endpoint}"));

    let start = Instant::now();

    let result = tokio::select! {
        res = client.send_task(endpoint, params) => res,
        _ = cancel.cancelled() => {
            // Best-effort cancel on the remote
            let _ = client.cancel_task(
                endpoint,
                a2a::TaskIdParams { id: task_id.to_string() },
            ).await;
            bail!("task cancelled");
        }
    };

    let duration_secs = start.elapsed().as_secs_f64();

    match result {
        Ok(task) => {
            let response_text = task
                .status
                .message
                .as_ref()
                .map(|m| m.text())
                .unwrap_or_default();

            let artifact_text: String = task
                .artifacts
                .iter()
                .flat_map(|a| a.parts.iter())
                .filter_map(|p| match p {
                    a2a::Part::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");

            let stdout = if artifact_text.is_empty() {
                response_text.clone()
            } else {
                artifact_text
            };

            on_output(format!("← remote completed: {}", &response_text));

            let exit_code = match task.status.state {
                a2a::A2aTaskState::Completed => 0,
                a2a::A2aTaskState::Failed => 1,
                a2a::A2aTaskState::Canceled => 130,
                _ => 0,
            };

            Ok(TaskResult {
                exit_code,
                stdout,
                stderr: response_text,
                duration_secs,
                input_tokens: 0,
                output_tokens: 0,
            })
        }
        Err(e) => {
            on_output(format!("← remote error: {e:#}"));
            Ok(TaskResult {
                exit_code: 1,
                stdout: String::new(),
                stderr: format!("{e:#}"),
                duration_secs,
                input_tokens: 0,
                output_tokens: 0,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_token_usage_from_omh_stderr() {
        let stderr = "some output\n─── 3 tool call(s) (2.1s) │ 1234in + 567out tokens │ 5.3s ───\n";
        assert_eq!(parse_token_usage(stderr), (1234, 567));
    }

    #[test]
    fn parse_token_usage_no_tokens() {
        assert_eq!(parse_token_usage("no token info here"), (0, 0));
    }

    #[test]
    fn parse_token_usage_empty() {
        assert_eq!(parse_token_usage(""), (0, 0));
    }
}
