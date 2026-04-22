use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    Timeout,
    RateLimit,
    Permission,
    ToolNotFound,
    InvalidInput,
    ModelAccess,
    ContextWindow,
    Provider,
    ToolExecution,
    MaxTurnsReached,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnTelemetry {
    pub session_id: String,
    pub agent_name: String,
    pub provider_id: String,
    pub model_id: String,
    pub started_at: i64,
    pub completed_at: i64,
    pub elapsed_ms: u64,
    pub loop_turns: u32,
    pub tool_calls: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub completed: bool,
    pub response_chars: usize,
    pub error: Option<String>,
    pub error_category: Option<ErrorCategory>,
}

impl TurnTelemetry {
    pub fn append_jsonl(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create telemetry directory {}", parent.display())
            })?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open telemetry file {}", path.display()))?;
        serde_json::to_writer(&mut file, self).with_context(|| {
            format!("failed to serialize telemetry record to {}", path.display())
        })?;
        file.write_all(b"\n")
            .with_context(|| format!("failed to write telemetry newline to {}", path.display()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolTelemetry {
    pub session_id: String,
    pub agent_name: String,
    pub turn: u32,
    pub tool_call_id: String,
    pub tool_name: String,
    pub started_at: i64,
    pub completed_at: i64,
    pub duration_ms: u64,
    pub input_bytes: usize,
    pub output_chars: usize,
    pub success: bool,
    pub error_category: Option<ErrorCategory>,
    pub error: Option<String>,
}

impl ToolTelemetry {
    pub fn append_jsonl(&self, path: &Path) -> Result<()> {
        append_jsonl_record(self, path)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TelemetrySummary {
    pub records: usize,
    pub completed: usize,
    pub failed: usize,
    pub total_elapsed_ms: u64,
    pub total_loop_turns: u64,
    pub total_tool_calls: usize,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_response_chars: usize,
}

impl TelemetrySummary {
    pub fn from_records(records: &[TurnTelemetry]) -> Self {
        let mut summary = Self::default();
        summary.records = records.len();

        for record in records {
            if record.completed && record.error.is_none() {
                summary.completed += 1;
            } else {
                summary.failed += 1;
            }
            summary.total_elapsed_ms += record.elapsed_ms;
            summary.total_loop_turns += record.loop_turns as u64;
            summary.total_tool_calls += record.tool_calls;
            summary.total_input_tokens += record.input_tokens;
            summary.total_output_tokens += record.output_tokens;
            summary.total_response_chars += record.response_chars;
        }

        summary
    }

    pub fn avg_elapsed_ms(&self) -> u64 {
        if self.records == 0 {
            0
        } else {
            self.total_elapsed_ms / self.records as u64
        }
    }

    pub fn avg_tool_calls(&self) -> f64 {
        if self.records == 0 {
            0.0
        } else {
            self.total_tool_calls as f64 / self.records as f64
        }
    }

    pub fn avg_loop_turns(&self) -> f64 {
        if self.records == 0 {
            0.0
        } else {
            self.total_loop_turns as f64 / self.records as f64
        }
    }
}

pub fn read_jsonl(path: &Path) -> Result<Vec<TurnTelemetry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("failed to open telemetry file {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();

    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| {
            format!(
                "failed to read telemetry line {} from {}",
                index + 1,
                path.display()
            )
        })?;
        if line.trim().is_empty() {
            continue;
        }
        let record = serde_json::from_str::<TurnTelemetry>(&line).with_context(|| {
            format!(
                "failed to parse telemetry line {} from {}",
                index + 1,
                path.display()
            )
        })?;
        records.push(record);
    }

    Ok(records)
}

pub fn read_tool_jsonl(path: &Path) -> Result<Vec<ToolTelemetry>> {
    read_jsonl_records(path)
}

pub fn classify_error(message: &str) -> ErrorCategory {
    let lower = message.to_ascii_lowercase();

    if lower.contains("timed out") || lower.contains("timeout") {
        ErrorCategory::Timeout
    } else if lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("429")
        || lower.contains("529")
    {
        ErrorCategory::RateLimit
    } else if lower.contains("approval required")
        || lower.contains("permission")
        || lower.contains("denied")
        || lower.contains("not allowed")
    {
        ErrorCategory::Permission
    } else if lower.contains("unknown tool") {
        ErrorCategory::ToolNotFound
    } else if lower.contains("missing field")
        || lower.contains("invalid type")
        || lower.contains("expected")
        || lower.contains("parse")
        || lower.contains("invalid input")
    {
        ErrorCategory::InvalidInput
    } else if (lower.contains("not accessible") || lower.contains("model not found"))
        && lower.contains("model")
    {
        ErrorCategory::ModelAccess
    } else if lower.contains("context window")
        || lower.contains("maximum context")
        || lower.contains("prompt is too long")
        || lower.contains("context length")
    {
        ErrorCategory::ContextWindow
    } else if lower.contains("connection")
        || lower.contains("dns")
        || lower.contains("overloaded")
        || lower.contains("service unavailable")
        || lower.contains("reset by peer")
        || lower.contains("provider")
        || lower.contains("anthropic")
        || lower.contains("openai")
        || lower.contains("copilot")
    {
        ErrorCategory::Provider
    } else if lower.is_empty() {
        ErrorCategory::Unknown
    } else {
        ErrorCategory::ToolExecution
    }
}

fn append_jsonl_record(record: &impl Serialize, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("failed to create telemetry directory {}", parent.display())
        })?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open telemetry file {}", path.display()))?;
    serde_json::to_writer(&mut file, record)
        .with_context(|| format!("failed to serialize telemetry record to {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("failed to write telemetry newline to {}", path.display()))?;
    Ok(())
}

fn read_jsonl_records<T>(path: &Path) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("failed to open telemetry file {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();

    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| {
            format!(
                "failed to read telemetry line {} from {}",
                index + 1,
                path.display()
            )
        })?;
        if line.trim().is_empty() {
            continue;
        }
        let record = serde_json::from_str::<T>(&line).with_context(|| {
            format!(
                "failed to parse telemetry line {} from {}",
                index + 1,
                path.display()
            )
        })?;
        records.push(record);
    }

    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_read_telemetry_jsonl() {
        let dir = std::env::temp_dir().join(format!("omh_runtime_telemetry_{}", ulid::Ulid::new()));
        let path = dir.join("telemetry.jsonl");

        let record = TurnTelemetry {
            session_id: "ses_1".into(),
            agent_name: "worker".into(),
            provider_id: "mock".into(),
            model_id: "mock-model".into(),
            started_at: 10,
            completed_at: 20,
            elapsed_ms: 10,
            loop_turns: 2,
            tool_calls: 3,
            input_tokens: 11,
            output_tokens: 7,
            completed: true,
            response_chars: 12,
            error: None,
            error_category: None,
        };

        record.append_jsonl(&path).unwrap();
        let loaded = read_jsonl(&path).unwrap();
        assert_eq!(loaded, vec![record]);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn summarizes_records() {
        let records = vec![
            TurnTelemetry {
                session_id: "ses_1".into(),
                agent_name: "worker".into(),
                provider_id: "mock".into(),
                model_id: "m1".into(),
                started_at: 10,
                completed_at: 20,
                elapsed_ms: 10,
                loop_turns: 1,
                tool_calls: 2,
                input_tokens: 10,
                output_tokens: 4,
                completed: true,
                response_chars: 20,
                error: None,
                error_category: None,
            },
            TurnTelemetry {
                session_id: "ses_1".into(),
                agent_name: "worker".into(),
                provider_id: "mock".into(),
                model_id: "m1".into(),
                started_at: 30,
                completed_at: 60,
                elapsed_ms: 30,
                loop_turns: 3,
                tool_calls: 1,
                input_tokens: 12,
                output_tokens: 5,
                completed: false,
                response_chars: 5,
                error: Some("boom".into()),
                error_category: Some(ErrorCategory::ToolExecution),
            },
        ];

        let summary = TelemetrySummary::from_records(&records);
        assert_eq!(summary.records, 2);
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.total_elapsed_ms, 40);
        assert_eq!(summary.total_tool_calls, 3);
        assert_eq!(summary.total_input_tokens, 22);
        assert_eq!(summary.total_output_tokens, 9);
        assert_eq!(summary.avg_elapsed_ms(), 20);
    }
}
