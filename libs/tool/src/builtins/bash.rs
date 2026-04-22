use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::process::Command;
use tracing::debug;

use crate::{PermissionLevel, ToolContext, ToolHandler, ToolOutput, ToolSpec};
use crate::truncate::{self, MAX_BYTES};

pub struct BashTool;

const DEFAULT_MAX_OUTPUT: usize = MAX_BYTES;

#[derive(Deserialize)]
struct BashInput {
    command: String,
    timeout_ms: Option<u64>,
    max_output: Option<usize>,
}

#[async_trait]
impl ToolHandler for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Execute a shell command. AVOID using ls/find/grep/cat for file discovery or search — use the dedicated glob and grep tools instead (they respect .gitignore). Use bash only for build, test, git, and other non-search operations.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_ms": { "type": "integer" },
                    "max_output": { "type": "integer", "description": "Max output bytes (default 51200)" }
                },
                "required": ["command"]
            }),
            required_permission: PermissionLevel::FullAccess,
            supports_parallel: false,
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolOutput> {
        let input: BashInput = serde_json::from_value(input)?;
        let timeout_ms = input.timeout_ms.unwrap_or(120_000);
        let max_output = input.max_output.unwrap_or(DEFAULT_MAX_OUTPUT);

        debug!(command = %input.command, timeout_ms, "executing bash tool");

        let mut command = Command::new("bash");
        command.arg("-c").arg(&input.command);
        command.kill_on_drop(true);

        let output = tokio::time::timeout(Duration::from_millis(timeout_ms), command.output())
            .await
            .with_context(|| format!("bash command timed out after {timeout_ms}ms"))??;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        debug!(
            command = %input.command,
            status = ?output.status,
            "bash tool completed"
        );

        let mut content = stdout.to_string();
        if !stderr.is_empty() {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&stderr);
        }

        let truncated = if content.len() > max_output {
            let total_len = content.len();
            content = truncate::truncate_middle(&content, max_output, None);
            content.push_str(&format!(
                "\n\n[BASH OUTPUT: {total_len} bytes → {max_output} bytes]"
            ));
            true
        } else {
            false
        };

        let mut result = if output.status.success() {
            ToolOutput::text(content)
        } else {
            ToolOutput::error(content)
        };

        result
            .metadata
            .insert("exit_code".into(), json!(output.status.code()));
        if truncated {
            result.metadata.insert("truncated".into(), json!(true));
        }

        Ok(result)
    }
}

