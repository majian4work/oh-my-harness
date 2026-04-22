use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{PermissionLevel, ToolContext, ToolHandler, ToolOutput, ToolSpec};
use crate::truncate::{self, MAX_BYTES, MAX_LINES};

pub struct ReadFileTool;
pub struct WriteFileTool;
pub struct EditFileTool;

#[derive(Deserialize)]
struct ReadFileInput {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct WriteFileInput {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct EditFileInput {
    path: String,
    old_string: String,
    new_string: String,
}

#[async_trait]
impl ToolHandler for ReadFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Read a file by path with optional offset/limit. Use for inspecting known files. For file discovery use glob; for content search use grep.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer" },
                    "limit": { "type": "integer" }
                },
                "required": ["path"]
            }),
            required_permission: PermissionLevel::ReadOnly,
            supports_parallel: true,
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolOutput> {
        let input: ReadFileInput = serde_json::from_value(input)?;
        let path = resolve_path(ctx, &input.path);
        let content = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;

        let start = input.offset.unwrap_or(1).saturating_sub(1);
        let limit = input.limit.unwrap_or(MAX_LINES);
        let total_lines = content.lines().count();

        let mut byte_count = 0usize;
        let mut lines_taken = 0usize;
        let mut result_lines = Vec::new();

        for (index, line) in content.lines().enumerate().skip(start).take(limit) {
            let truncated_line = truncate::truncate_line(line);
            let formatted = format!("{}: {}", index + 1, truncated_line);
            byte_count += formatted.len() + 1;
            if byte_count > MAX_BYTES && !result_lines.is_empty() {
                result_lines.push(format!(
                    "\n[OUTPUT CAPPED at {} bytes. Showing lines {}-{}. Use offset={} to continue. Total lines: {}]",
                    MAX_BYTES,
                    start + 1,
                    index,
                    index + 1,
                    total_lines
                ));
                let mut output = ToolOutput::text(result_lines.join("\n"));
                output.metadata.insert("truncated".into(), serde_json::json!(true));
                return Ok(output);
            }
            result_lines.push(formatted);
            lines_taken += 1;
        }

        let mut text = result_lines.join("\n");
        if lines_taken < total_lines.saturating_sub(start) {
            text.push_str(&format!(
                "\n\n(Showing lines {}-{} of {}. Use offset={} to continue.)",
                start + 1,
                start + lines_taken,
                total_lines,
                start + lines_taken + 1
            ));
        }

        Ok(ToolOutput::text(text))
    }
}

#[async_trait]
impl ToolHandler for WriteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Create or overwrite a file. Use for new files or full rewrites. For partial edits use edit_file instead.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
            required_permission: PermissionLevel::WorkspaceWrite,
            supports_parallel: false,
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolOutput> {
        let input: WriteFileInput = serde_json::from_value(input)?;
        let path = resolve_path(ctx, &input.path);
        tokio::fs::write(&path, input.content)
            .await
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(ToolOutput::text(format!("wrote {}", path.display())))
    }
}

#[async_trait]
impl ToolHandler for EditFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Replace exact text in a file. Provide old_string (must match exactly) and new_string. For full file rewrites use write_file.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
            required_permission: PermissionLevel::WorkspaceWrite,
            supports_parallel: false,
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolOutput> {
        let input: EditFileInput = serde_json::from_value(input)?;
        let path = resolve_path(ctx, &input.path);
        let content = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;

        let matches = content.matches(&input.old_string).count();
        match matches {
            0 => bail!("old_string not found in {}", path.display()),
            1 => {}
            _ => bail!("old_string matched multiple times in {}", path.display()),
        }

        let updated = content.replacen(&input.old_string, &input.new_string, 1);
        tokio::fs::write(&path, updated)
            .await
            .with_context(|| format!("failed to write {}", path.display()))?;

        Ok(ToolOutput::text(format!("edited {}", path.display())))
    }
}

fn resolve_path(ctx: &ToolContext, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        ctx.workspace_root.join(path)
    }
}
