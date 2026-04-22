//! Centralized tool output truncation.
//!
//! Each tool may truncate its own output and set `metadata["truncated"] = true`
//! to opt out of the centralized layer. If a tool does NOT self-truncate, the
//! fallback in [`maybe_truncate`] applies automatically.
//!
//! When truncation occurs AND a `spill_dir` is provided, the full output is
//! written to disk so the LLM can access it via read_file/grep.

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::ToolOutput;

pub const MAX_BYTES: usize = 50 * 1024;
pub const MAX_LINES: usize = 2000;
pub const MAX_LINE_LENGTH: usize = 2000;

/// Apply centralized truncation to a [`ToolOutput`] if the tool did not
/// already handle it (i.e. `metadata["truncated"]` is absent).
///
/// * `tool_name` — used for the spill filename.
/// * `spill_dir` — if `Some`, full output is written to this directory when
///   truncation occurs. The path is included in the truncation marker so the
///   LLM can use `read_file` or `grep` to inspect it.
pub fn maybe_truncate(
    mut output: ToolOutput,
    tool_name: &str,
    call_id: &str,
    spill_dir: Option<&Path>,
) -> ToolOutput {
    if output.metadata.contains_key("truncated") {
        return output;
    }

    let raw_len = output.content.len();
    if raw_len <= MAX_BYTES {
        return output;
    }

    let spill_path = if let Some(dir) = spill_dir {
        match spill_to_disk(dir, tool_name, call_id, &output.content) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!("failed to spill tool output to disk: {e}");
                None
            }
        }
    } else {
        None
    };

    output.content = truncate_middle(&output.content, MAX_BYTES, spill_path.as_deref());
    output.metadata.insert("truncated".into(), json!(true));
    if let Some(p) = &spill_path {
        output
            .metadata
            .insert("output_path".into(), json!(p.display().to_string()));
    }
    output
}

// ---------------------------------------------------------------------------

/// Truncate `s` keeping the first 80% and last 20% of `max_bytes`,
/// inserting a marker in the middle.
pub fn truncate_middle(s: &str, max_bytes: usize, spill_path: Option<&Path>) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }

    let head_budget = max_bytes * 4 / 5;
    let tail_budget = max_bytes / 5;

    let head_end = char_boundary_before(s, head_budget);
    let tail_start = char_boundary_after(s, s.len().saturating_sub(tail_budget));

    let omitted = s.len() - head_end - (s.len() - tail_start);

    let hint = match spill_path {
        Some(p) => format!(
            "Full output saved to: {}\nUse read_file or grep to inspect it.",
            p.display()
        ),
        None => "Use read_file with offset/limit or grep to access specific sections.".into(),
    };

    format!(
        "{}\n\n[OUTPUT TRUNCATED: {} bytes total, {omitted} bytes omitted (showing head+tail of {max_bytes} bytes)]\n[{hint}]\n\n{}",
        &s[..head_end],
        s.len(),
        &s[tail_start..]
    )
}

/// Truncate a single line to `MAX_LINE_LENGTH` characters.
pub fn truncate_line(line: &str) -> String {
    if line.len() <= MAX_LINE_LENGTH {
        return line.to_string();
    }
    let boundary = char_boundary_before(line, MAX_LINE_LENGTH);
    format!(
        "{}... (line truncated, {} chars total)",
        &line[..boundary],
        line.len()
    )
}

fn spill_to_disk(
    dir: &Path,
    tool_name: &str,
    call_id: &str,
    content: &str,
) -> std::io::Result<PathBuf> {
    let spill_dir = dir.join("tool-output");
    std::fs::create_dir_all(&spill_dir)?;
    let short_id = &call_id[..call_id.len().min(12)];
    let path = spill_dir.join(format!("{tool_name}_{short_id}.txt"));
    std::fs::write(&path, content)?;
    Ok(path)
}

fn char_boundary_before(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    s.floor_char_boundary(pos)
}

fn char_boundary_after(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    s.ceil_char_boundary(pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_truncation_when_within_limit() {
        let output = ToolOutput::text("hello");
        let result = maybe_truncate(output, "test", "id1", None);
        assert_eq!(result.content, "hello");
        assert!(!result.metadata.contains_key("truncated"));
    }

    #[test]
    fn skips_if_already_truncated() {
        let mut output = ToolOutput::text("x".repeat(MAX_BYTES + 1));
        output.metadata.insert("truncated".into(), json!(true));
        let result = maybe_truncate(output.clone(), "test", "id2", None);
        assert_eq!(result.content, output.content);
    }

    #[test]
    fn truncates_large_output() {
        let big = "a".repeat(MAX_BYTES * 2);
        let output = ToolOutput::text(big);
        let result = maybe_truncate(output, "test", "id3", None);
        assert!(result.content.len() < MAX_BYTES * 2);
        assert!(result.content.contains("[OUTPUT TRUNCATED:"));
        assert_eq!(result.metadata["truncated"], json!(true));
    }

    #[test]
    fn truncate_line_caps_long_lines() {
        let long = "x".repeat(MAX_LINE_LENGTH + 500);
        let truncated = truncate_line(&long);
        assert!(truncated.len() < long.len());
        assert!(truncated.contains("line truncated"));
    }

    #[test]
    fn truncate_line_passes_short_lines() {
        let short = "hello world";
        assert_eq!(truncate_line(short), short);
    }

    #[test]
    fn spill_to_disk_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = spill_to_disk(dir.path(), "bash", "call_abc123def", "full content").unwrap();
        assert!(path.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "full content");
    }
}
