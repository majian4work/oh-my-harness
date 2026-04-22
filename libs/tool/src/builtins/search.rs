use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{PermissionLevel, ToolContext, ToolHandler, ToolOutput, ToolSpec};
use crate::truncate::MAX_BYTES;

pub struct GlobTool;
pub struct GrepTool;

const MAX_RESULTS: usize = 200;

#[derive(Deserialize)]
struct GlobInput {
    pattern: String,
    path: Option<String>,
    max_results: Option<usize>,
}

#[derive(Deserialize)]
struct GrepInput {
    pattern: String,
    path: Option<String>,
    include: Option<String>,
    max_results: Option<usize>,
}

fn has_command(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[async_trait]
impl ToolHandler for GlobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".into(),
            description: "Find files by glob pattern (respects .gitignore). Use instead of ls/find. Supports ** for recursive matching.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" },
                    "max_results": { "type": "integer", "description": "Max files to return (default 200)" }
                },
                "required": ["pattern"]
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
        let input: GlobInput = serde_json::from_value(input)?;
        let search_root = resolve_search_root(ctx, input.path.as_deref());
        let max = input.max_results.unwrap_or(MAX_RESULTS);

        if has_command("fd") {
            return glob_via_fd(&search_root, &input.pattern, max);
        }

        glob_fallback(ctx, input.path.as_deref(), &input.pattern, max)
    }
}

fn glob_via_fd(root: &Path, pattern: &str, max: usize) -> anyhow::Result<ToolOutput> {
    let output = Command::new("fd")
        .args(["--glob", pattern, "--type", "f"])
        .current_dir(root)
        .output()
        .context("failed to execute fd")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut matches: Vec<String> = stdout.lines().filter(|l| !l.is_empty()).map(String::from).collect();
    matches.sort();
    Ok(truncate_results(matches, max))
}

fn glob_fallback(ctx: &ToolContext, path: Option<&str>, pattern: &str, max: usize) -> anyhow::Result<ToolOutput> {
    let root = resolve_search_root(ctx, path);
    let glob_matcher = glob::Pattern::new(pattern)
        .with_context(|| format!("invalid glob pattern: {pattern}"))?;

    let mut matches = Vec::new();
    for entry in ignore::WalkBuilder::new(&root).build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            if glob_matcher.matches_path(rel) {
                matches.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    matches.sort();
    Ok(truncate_results(matches, max))
}

#[async_trait]
impl ToolHandler for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "Search file contents with regex (respects .gitignore). Use instead of bash grep/rg. Optional include filter for file patterns.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" },
                    "include": { "type": "string" },
                    "max_results": { "type": "integer", "description": "Max matching lines to return (default 200)" }
                },
                "required": ["pattern"]
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
        let input: GrepInput = serde_json::from_value(input)?;
        let root = resolve_search_root(ctx, input.path.as_deref());
        let max = input.max_results.unwrap_or(MAX_RESULTS);

        if has_command("rg") {
            return grep_via_rg(&root, &input.pattern, input.include.as_deref(), max);
        }

        grep_fallback(&root, &input.pattern, input.include.as_deref(), max).await
    }
}

fn grep_via_rg(root: &Path, pattern: &str, include: Option<&str>, max: usize) -> anyhow::Result<ToolOutput> {
    let mut cmd = Command::new("rg");
    cmd.args(["--no-heading", "--line-number", pattern]);
    if let Some(glob_pattern) = include {
        cmd.args(["--glob", glob_pattern]);
    }
    cmd.current_dir(root);

    let output = cmd.output().context("failed to execute rg")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<String> = stdout.lines().filter(|l| !l.is_empty()).map(String::from).collect();
    Ok(truncate_results(lines, max))
}

async fn grep_fallback(root: &Path, pattern: &str, include: Option<&str>, max: usize) -> anyhow::Result<ToolOutput> {
    let regex = regex::Regex::new(pattern)?;
    let include = include.map(glob::Pattern::new).transpose()?;

    let mut files = Vec::new();
    for entry in ignore::WalkBuilder::new(root).build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            files.push(entry.into_path());
        }
    }
    files.sort();

    let mut matches = Vec::new();
    for file in files {
        if let Some(include) = &include {
            let relative = file.strip_prefix(root).unwrap_or(&file);
            if !include.matches_path(relative) && !include.matches_path(&file) {
                continue;
            }
        }

        let Ok(content) = tokio::fs::read_to_string(&file).await else {
            continue;
        };

        for (index, line) in content.lines().enumerate() {
            if regex.is_match(line) {
                matches.push(format!("{}:{}:{}", file.display(), index + 1, line));
            }
        }
    }

    Ok(truncate_results(matches, max))
}

fn resolve_search_root(ctx: &ToolContext, path: Option<&str>) -> PathBuf {
    match path {
        Some(path) => resolve_path(ctx, path),
        None => ctx.workspace_root.clone(),
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

fn truncate_results(lines: Vec<String>, max_results: usize) -> ToolOutput {
    let total = lines.len();
    if total <= max_results {
        let text = lines.join("\n");
        if text.len() <= MAX_BYTES {
            return ToolOutput::text(text);
        }
        let truncated = &text[..text.floor_char_boundary(MAX_BYTES)];
        let mut output = ToolOutput::text(format!(
            "{truncated}\n\n[OUTPUT TRUNCATED: {total} results, {} bytes → {MAX_BYTES} bytes. Use grep with a more specific pattern to narrow results.]",
            text.len()
        ));
        output.metadata.insert("truncated".into(), serde_json::json!(true));
        return output;
    }

    let head_count = max_results * 4 / 5;
    let tail_count = max_results - head_count;
    let omitted = total - head_count - tail_count;

    let mut out = lines[..head_count].join("\n");
    out.push_str(&format!("\n\n... [{omitted} results omitted] ...\n\n"));
    out.push_str(&lines[total - tail_count..].join("\n"));
    out.push_str(&format!("\n\n[{total} total results, showing {max_results}. Use a more specific pattern to narrow results.]"));
    let mut output = ToolOutput::text(out);
    output.metadata.insert("truncated".into(), serde_json::json!(true));
    output
}
