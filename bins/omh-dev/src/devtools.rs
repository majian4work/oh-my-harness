use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use runtime::{
    AgentRuntime, ErrorCategory, TelemetrySummary, read_telemetry_jsonl, read_tool_telemetry_jsonl,
};

use crate::bootstrap::{init_harness, register_providers_from_env};

pub async fn cmd_diagnose(session_id: &str) -> Result<()> {
    let harness = init_harness()?;
    let dump_dir = harness.session_manager.dump_dir(session_id);

    if !dump_dir.exists() {
        bail!("No dumps found for session {session_id}. Run with OMH_LOG=trace to enable dumps.");
    }

    let mut agents: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&dump_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            agents.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    agents.sort();

    let mut total_anomalies = 0;

    for agent in &agents {
        let agent_dir = dump_dir.join(agent);
        let mut turn = 1u32;

        println!("═══ Agent: {} ═══", agent);

        loop {
            let request_path = agent_dir.join(format!("turn_{turn:03}_request.json"));
            let response_path = agent_dir.join(format!("turn_{turn:03}_response.json"));
            let tool_results_path = agent_dir.join(format!("turn_{turn:03}_tool_results.json"));

            if !request_path.exists() {
                break;
            }

            let request_text = std::fs::read_to_string(&request_path)?;
            let request: serde_json::Value = serde_json::from_str(&request_text)?;

            let model = request.get("model").and_then(|v| v.as_str()).unwrap_or("?");
            let msg_count = request
                .get("messages")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);

            if response_path.exists() {
                let response_text = std::fs::read_to_string(&response_path)?;
                let response: serde_json::Value = serde_json::from_str(&response_text)?;
                let tool_calls = count_tool_calls(&response);
                let response_text_content = extract_text_content(&response);

                println!("  Turn {turn}: model={model} msgs={msg_count} tool_calls={tool_calls}");

                let anomalies = check_turn_anomalies(
                    turn,
                    &request,
                    &response,
                    tool_results_path
                        .exists()
                        .then(|| tool_results_path.as_path()),
                )?;

                for anomaly in &anomalies {
                    println!("    ⚠ {anomaly}");
                    total_anomalies += 1;
                }

                if !response_text_content.is_empty() {
                    let preview = if response_text_content.len() > 120 {
                        format!("{}...", &response_text_content[..120])
                    } else {
                        response_text_content.clone()
                    };
                    println!("    text: {preview}");
                }
            }

            turn += 1;
        }

        println!();
    }

    if total_anomalies == 0 {
        println!("✓ No anomalies detected.");
    } else {
        println!("⚠ {} anomaly(ies) detected.", total_anomalies);
    }

    Ok(())
}

pub async fn cmd_telemetry(session_id: Option<&str>, limit: usize) -> Result<()> {
    let harness = init_harness()?;

    match session_id {
        Some(session_id) => {
            let path = harness.session_manager.telemetry_path(session_id);
            let records = read_telemetry_jsonl(&path)?;
            if records.is_empty() {
                println!("No telemetry records found for session {session_id}.");
                return Ok(());
            }

            let summary = TelemetrySummary::from_records(&records);
            println!("Session telemetry: {session_id}");
            println!("  records: {}", summary.records);
            println!("  completed: {}", summary.completed);
            println!("  failed: {}", summary.failed);
            println!("  avg latency: {} ms", summary.avg_elapsed_ms());
            println!("  avg loop turns: {:.2}", summary.avg_loop_turns());
            println!("  avg tool calls: {:.2}", summary.avg_tool_calls());
            println!(
                "  total tokens: {} in / {} out",
                summary.total_input_tokens, summary.total_output_tokens
            );

            println!("\nRecent records:");
            for record in records.iter().rev().take(10) {
                println!(
                    "  - agent={} model={} provider={} latency={}ms turns={} tools={} completed={} error={}",
                    record.agent_name,
                    record.model_id,
                    record.provider_id,
                    record.elapsed_ms,
                    record.loop_turns,
                    record.tool_calls,
                    record.completed,
                    record.error.as_deref().unwrap_or("-")
                );
            }
        }
        None => {
            let sessions = harness.session_manager.list(limit)?;
            if sessions.is_empty() {
                println!("No sessions found.");
                return Ok(());
            }

            println!(
                "{:<40} {:<12} {:>7} {:>10} {:>8} {:>12}",
                "ID", "Agent", "Records", "Avg ms", "Tools", "Tokens"
            );
            println!("{}", "─".repeat(100));

            let mut all_records = Vec::new();
            let mut sessions_with_data = 0usize;

            for session in sessions {
                let path = harness.session_manager.telemetry_path(&session.id);
                let records = read_telemetry_jsonl(&path)?;
                if records.is_empty() {
                    continue;
                }

                sessions_with_data += 1;
                let summary = TelemetrySummary::from_records(&records);
                let total_tokens = summary.total_input_tokens + summary.total_output_tokens;
                println!(
                    "{:<40} {:<12} {:>7} {:>10} {:>8.2} {:>12}",
                    session.id,
                    session.agent_name,
                    summary.records,
                    summary.avg_elapsed_ms(),
                    summary.avg_tool_calls(),
                    total_tokens,
                );
                all_records.extend(records);
            }

            if sessions_with_data == 0 {
                println!("No telemetry records found in the most recent {limit} sessions.");
                return Ok(());
            }

            let summary = TelemetrySummary::from_records(&all_records);
            println!("\nAggregate:");
            println!("  sessions with data: {}", sessions_with_data);
            println!("  records: {}", summary.records);
            println!("  completed: {}", summary.completed);
            println!("  failed: {}", summary.failed);
            println!("  avg latency: {} ms", summary.avg_elapsed_ms());
            println!("  avg loop turns: {:.2}", summary.avg_loop_turns());
            println!("  avg tool calls: {:.2}", summary.avg_tool_calls());
            println!(
                "  total tokens: {} in / {} out",
                summary.total_input_tokens, summary.total_output_tokens
            );
        }
    }

    Ok(())
}

pub async fn cmd_eval(path: Option<&str>) -> Result<()> {
    let mut harness = init_harness()?;
    register_providers_from_env(&mut harness)?;

    if harness.provider_registry.list().is_empty() {
        bail!(
            "no provider configured. Set OPENAI_API_KEY or ANTHROPIC_API_KEY, or run 'omh auth login <provider> --key <api_key>'."
        );
    }

    let workspace_root = std::env::current_dir()?;
    let suite = load_eval_suite(&workspace_root, path)?;
    if suite.cases.is_empty() {
        bail!("no eval cases found");
    }

    let mut case_results = Vec::new();

    for case in &suite.cases {
        let agent_def = harness
            .agent_registry
            .get(&case.agent)
            .with_context(|| format!("unknown agent: {}", case.agent))?;
        let session = harness
            .session_manager
            .create(&case.agent, "", &workspace_root)?;

        let runtime = AgentRuntime::new(
            case.agent.clone(),
            session.id.clone(),
            agent_def.max_turns.unwrap_or(30),
        );
        let mut runtime = runtime.with_logger(&harness);
        runtime.interactive = false;

        let execution = runtime.run_turn(&harness, &case.prompt).await;
        let turn_records =
            read_telemetry_jsonl(&harness.session_manager.telemetry_path(&session.id))?;
        let tool_records =
            read_tool_telemetry_jsonl(&harness.session_manager.tool_telemetry_path(&session.id))?;
        let turn_record = turn_records.last().cloned();

        let result = evaluate_case(
            case,
            &session.id,
            execution.as_ref().ok(),
            turn_record,
            &tool_records,
        );
        print_eval_case(&result);
        case_results.push(result);
    }

    let report = EvalReport {
        generated_at: now_millis(),
        suite_path: suite.source.display().to_string(),
        total: case_results.len(),
        passed: case_results.iter().filter(|r| r.passed).count(),
        failed: case_results.iter().filter(|r| !r.passed).count(),
        aggregates: build_eval_aggregates(&case_results),
        cases: case_results,
    };

    let report_path = write_eval_report(&workspace_root, &report)?;
    println!("\nEval summary: {}/{} passed", report.passed, report.total);
    println!("Report written to {}", report_path.display());

    if report.failed > 0 {
        bail!("{} eval case(s) failed", report.failed);
    }

    Ok(())
}

fn count_tool_calls(response: &serde_json::Value) -> usize {
    response
        .get("parts")
        .and_then(|v| v.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                .count()
        })
        .unwrap_or(0)
}

#[derive(Debug, Clone, serde::Deserialize)]
struct EvalSuiteFile {
    #[serde(default)]
    cases: Vec<EvalCase>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct EvalCase {
    name: String,
    prompt: String,
    #[serde(default = "default_eval_agent")]
    agent: String,
    #[serde(default = "default_true")]
    require_completed: bool,
    #[serde(default)]
    contains_all: Vec<String>,
    #[serde(default)]
    contains_any: Vec<String>,
    #[serde(default)]
    not_contains: Vec<String>,
    min_tool_calls: Option<usize>,
    max_tool_calls: Option<usize>,
    max_tool_errors: Option<usize>,
    #[serde(default)]
    disallow_error_categories: Vec<ErrorCategory>,
}

#[derive(Debug)]
struct LoadedEvalSuite {
    source: std::path::PathBuf,
    cases: Vec<EvalCase>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct EvalCaseResult {
    name: String,
    agent: String,
    session_id: String,
    passed: bool,
    failures: Vec<String>,
    completed: bool,
    tool_calls: usize,
    tool_errors: usize,
    turn_error_category: Option<ErrorCategory>,
    tool_error_categories: Vec<ErrorCategory>,
    observed_tools: Vec<String>,
    tool_observations: Vec<EvalToolObservation>,
    response_preview: String,
}

#[derive(Debug, Clone, serde::Serialize)]
struct EvalReport {
    generated_at: i64,
    suite_path: String,
    total: usize,
    passed: usize,
    failed: usize,
    aggregates: EvalAggregates,
    cases: Vec<EvalCaseResult>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct EvalAggregates {
    by_agent: Vec<AgentAggregate>,
    by_tool: Vec<ToolAggregate>,
    by_error_category: Vec<ErrorCategoryAggregate>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct AgentAggregate {
    agent: String,
    cases: usize,
    passed: usize,
    failed: usize,
    total_tool_calls: usize,
    total_tool_errors: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ToolAggregate {
    tool: String,
    calls: usize,
    successes: usize,
    failures: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ErrorCategoryAggregate {
    error_category: ErrorCategory,
    count: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
struct EvalToolObservation {
    tool: String,
    success: bool,
    error_category: Option<ErrorCategory>,
}

fn default_eval_agent() -> String {
    "orchestrator".to_string()
}

fn default_true() -> bool {
    true
}

fn load_eval_suite(
    workspace_root: &std::path::Path,
    path: Option<&str>,
) -> Result<LoadedEvalSuite> {
    let source = path
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("tests/evals"));

    let files = if source.is_dir() {
        let mut files: Vec<_> = std::fs::read_dir(&source)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("toml"))
            .collect();
        files.sort();
        files
    } else {
        vec![source.clone()]
    };

    if files.is_empty() {
        bail!("no eval TOML files found at {}", source.display());
    }

    let mut cases = Vec::new();
    for file in &files {
        let raw = std::fs::read_to_string(file)
            .with_context(|| format!("failed to read eval file {}", file.display()))?;
        let suite: EvalSuiteFile = toml::from_str(&raw)
            .with_context(|| format!("failed to parse eval file {}", file.display()))?;
        cases.extend(suite.cases);
    }

    Ok(LoadedEvalSuite { source, cases })
}

fn evaluate_case(
    case: &EvalCase,
    session_id: &str,
    result: Option<&runtime::TurnResult>,
    turn_record: Option<runtime::TurnTelemetry>,
    tool_records: &[runtime::ToolTelemetry],
) -> EvalCaseResult {
    let response = result.map(|r| r.response.clone()).unwrap_or_default();
    let response_lower = response.to_lowercase();
    let completed = result.map(|r| r.completed).unwrap_or(false);
    let tool_calls = result
        .map(|r| r.tool_calls_made)
        .unwrap_or(tool_records.len());
    let observed_tools: Vec<String> = tool_records
        .iter()
        .map(|record| record.tool_name.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let tool_observations: Vec<EvalToolObservation> = tool_records
        .iter()
        .map(|record| EvalToolObservation {
            tool: record.tool_name.clone(),
            success: record.success,
            error_category: record.error_category,
        })
        .collect();
    let tool_error_categories: Vec<ErrorCategory> = tool_records
        .iter()
        .filter_map(|record| record.error_category)
        .collect();
    let tool_errors = tool_records.iter().filter(|record| !record.success).count();
    let turn_error_category = turn_record
        .as_ref()
        .and_then(|record| record.error_category);

    let mut failures = Vec::new();
    if case.require_completed && !completed {
        failures.push("turn did not complete".to_string());
    }

    for needle in &case.contains_all {
        if !response_lower.contains(&needle.to_lowercase()) {
            failures.push(format!("response missing required text: {needle}"));
        }
    }

    if !case.contains_any.is_empty()
        && !case
            .contains_any
            .iter()
            .any(|needle| response_lower.contains(&needle.to_lowercase()))
    {
        failures.push(format!(
            "response did not contain any of the expected strings: {}",
            case.contains_any.join(", ")
        ));
    }

    for needle in &case.not_contains {
        if response_lower.contains(&needle.to_lowercase()) {
            failures.push(format!("response contained forbidden text: {needle}"));
        }
    }

    if let Some(min_tool_calls) = case.min_tool_calls {
        if tool_calls < min_tool_calls {
            failures.push(format!(
                "tool calls below minimum: {tool_calls} < {min_tool_calls}"
            ));
        }
    }

    if let Some(max_tool_calls) = case.max_tool_calls {
        if tool_calls > max_tool_calls {
            failures.push(format!(
                "tool calls exceeded limit: {tool_calls} > {max_tool_calls}"
            ));
        }
    }

    if let Some(max_tool_errors) = case.max_tool_errors {
        if tool_errors > max_tool_errors {
            failures.push(format!(
                "tool errors exceeded limit: {tool_errors} > {max_tool_errors}"
            ));
        }
    }

    for category in &case.disallow_error_categories {
        if turn_error_category == Some(*category) || tool_error_categories.contains(category) {
            failures.push(format!(
                "disallowed error category observed: {:?}",
                category
            ));
        }
    }

    EvalCaseResult {
        name: case.name.clone(),
        agent: case.agent.clone(),
        session_id: session_id.to_string(),
        passed: failures.is_empty(),
        failures,
        completed,
        tool_calls,
        tool_errors,
        turn_error_category,
        tool_error_categories,
        observed_tools,
        tool_observations,
        response_preview: truncate(&response, 120),
    }
}

fn build_eval_aggregates(case_results: &[EvalCaseResult]) -> EvalAggregates {
    let mut by_agent: BTreeMap<String, AgentAggregate> = BTreeMap::new();
    let mut by_tool: BTreeMap<String, ToolAggregate> = BTreeMap::new();
    let mut by_error_category: BTreeMap<ErrorCategory, usize> = BTreeMap::new();

    for result in case_results {
        let agent = by_agent
            .entry(result.agent.clone())
            .or_insert_with(|| AgentAggregate {
                agent: result.agent.clone(),
                cases: 0,
                passed: 0,
                failed: 0,
                total_tool_calls: 0,
                total_tool_errors: 0,
            });
        agent.cases += 1;
        if result.passed {
            agent.passed += 1;
        } else {
            agent.failed += 1;
        }
        agent.total_tool_calls += result.tool_calls;
        agent.total_tool_errors += result.tool_errors;

        if let Some(category) = result.turn_error_category {
            *by_error_category.entry(category).or_insert(0) += 1;
        }
        for category in &result.tool_error_categories {
            *by_error_category.entry(*category).or_insert(0) += 1;
        }

        for observation in &result.tool_observations {
            let tool = by_tool
                .entry(observation.tool.clone())
                .or_insert_with(|| ToolAggregate {
                    tool: observation.tool.clone(),
                    calls: 0,
                    successes: 0,
                    failures: 0,
                });
            tool.calls += 1;
            if observation.success {
                tool.successes += 1;
            } else {
                tool.failures += 1;
            }
        }
    }

    EvalAggregates {
        by_agent: by_agent.into_values().collect(),
        by_tool: by_tool.into_values().collect(),
        by_error_category: by_error_category
            .into_iter()
            .map(|(error_category, count)| ErrorCategoryAggregate {
                error_category,
                count,
            })
            .collect(),
    }
}

fn print_eval_case(result: &EvalCaseResult) {
    let status = if result.passed { "PASS" } else { "FAIL" };
    println!("[{status}] {} ({})", result.name, result.agent);
    println!(
        "  session={} completed={} tool_calls={} tool_errors={} turn_error={}",
        result.session_id,
        result.completed,
        result.tool_calls,
        result.tool_errors,
        result
            .turn_error_category
            .map(|category| format!("{:?}", category))
            .unwrap_or_else(|| "-".to_string())
    );
    println!("  response={}", result.response_preview);
    for failure in &result.failures {
        println!("  failure: {failure}");
    }
}

fn write_eval_report(
    workspace_root: &std::path::Path,
    report: &EvalReport,
) -> Result<std::path::PathBuf> {
    let dir = workspace_root.join("tests/evals/reports");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!(
        "eval_{}.json",
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    ));
    let json = serde_json::to_string_pretty(report)?;
    std::fs::write(&path, json)?;
    Ok(path)
}

fn extract_text_content(response: &serde_json::Value) -> String {
    response
        .get("parts")
        .and_then(|v| v.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| {
                    if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                        p.get("text").and_then(|t| t.as_str()).map(String::from)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn check_turn_anomalies(
    turn: u32,
    request: &serde_json::Value,
    response: &serde_json::Value,
    tool_results_path: Option<&std::path::Path>,
) -> Result<Vec<String>> {
    let mut anomalies = Vec::new();

    let messages = request
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut tool_result_data: Vec<(String, usize)> = Vec::new();
    for msg in &messages {
        if let Some(parts) = msg.get("parts").and_then(|v| v.as_array()) {
            for part in parts {
                if part.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                    let content = part.get("content").and_then(|c| c.as_str()).unwrap_or("");
                    let is_error = part
                        .get("is_error")
                        .and_then(|e| e.as_bool())
                        .unwrap_or(false);
                    if !is_error && !content.is_empty() {
                        let id = part
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or("?")
                            .to_string();
                        tool_result_data.push((id, content.len()));
                    }
                }
            }
        }
    }

    let response_text = extract_text_content(response).to_lowercase();
    let negative_phrases = [
        "no usable file",
        "could not inspect",
        "inaccessible",
        "no readable",
        "no visible",
        "could not reliably",
        "did not return",
        "returned no",
        "no project files",
    ];

    let has_substantial_data = tool_result_data.iter().any(|(_, len)| *len > 100);
    let has_negative_conclusion = negative_phrases
        .iter()
        .any(|phrase| response_text.contains(phrase));

    if has_substantial_data && has_negative_conclusion {
        let total_bytes: usize = tool_result_data.iter().map(|(_, len)| *len).sum();
        anomalies.push(format!(
            "DATA_IGNORED: Model received {} bytes of tool_result data across {} results but concluded negatively",
            total_bytes,
            tool_result_data.len()
        ));
    }

    if turn > 1 {
        let response_tools = extract_tool_calls(response);
        let prev_tool_calls = extract_request_tool_calls(&messages);

        for (name, args) in &response_tools {
            for (prev_name, prev_args) in &prev_tool_calls {
                if name == prev_name && args == prev_args {
                    anomalies.push(format!(
                        "DUPLICATE_CALL: Model re-called {name} with identical args as a previous turn"
                    ));
                    break;
                }
            }
        }
    }

    if let Some(path) = tool_results_path {
        let tr_text = std::fs::read_to_string(path)?;
        let tr: serde_json::Value = serde_json::from_str(&tr_text)?;
        if let Some(parts) = tr.get("parts").and_then(|v| v.as_array()) {
            for part in parts {
                let is_error = part
                    .get("is_error")
                    .and_then(|e| e.as_bool())
                    .unwrap_or(false);
                let content = part.get("content").and_then(|c| c.as_str()).unwrap_or("");
                if is_error && content.contains("token count") && content.contains("exceeds") {
                    anomalies.push(format!(
                        "TOKEN_OVERFLOW: Subagent failed with token limit: {}",
                        &content[..content.len().min(120)]
                    ));
                }
                if is_error && content.contains("failed") {
                    anomalies.push(format!(
                        "TOOL_ERROR: {}",
                        &content[..content.len().min(120)]
                    ));
                }
            }
        }
    }

    Ok(anomalies)
}

fn extract_tool_calls(response: &serde_json::Value) -> Vec<(String, String)> {
    response
        .get("parts")
        .and_then(|v| v.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| {
                    if p.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                        let input = p.get("input").map(|i| i.to_string()).unwrap_or_default();
                        Some((name.to_string(), input))
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn extract_request_tool_calls(messages: &[serde_json::Value]) -> Vec<(String, String)> {
    let mut calls = Vec::new();
    for msg in messages {
        if let Some(parts) = msg.get("parts").and_then(|v| v.as_array()) {
            for part in parts {
                if part.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    let name = part.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                    let input = part.get("input").map(|i| i.to_string()).unwrap_or_default();
                    calls.push((name.to_string(), input));
                }
            }
        }
    }
    calls
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.replace('\n', " ")
    } else {
        format!("{}…", s[..max].replace('\n', " "))
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluate_case_passes_when_constraints_match() {
        let case = EvalCase {
            name: "hello".into(),
            prompt: "say hello".into(),
            agent: "orchestrator".into(),
            require_completed: true,
            contains_all: vec!["hello".into()],
            contains_any: Vec::new(),
            not_contains: vec!["error".into()],
            min_tool_calls: None,
            max_tool_calls: Some(1),
            max_tool_errors: Some(0),
            disallow_error_categories: vec![ErrorCategory::Timeout],
        };

        let result = runtime::TurnResult {
            response: "hello".into(),
            tool_calls_made: 0,
            tool_calls: Vec::new(),
            tokens_used: Some(2),
            input_tokens: 1,
            output_tokens: 1,
            completed: true,
        };
        let turn = runtime::TurnTelemetry {
            session_id: "ses_1".into(),
            agent_name: "orchestrator".into(),
            session_primary_agent: None,
            provider_id: "mock".into(),
            model_id: "mock-model".into(),
            started_at: 1,
            completed_at: 2,
            elapsed_ms: 1,
            loop_turns: 1,
            tool_calls: 0,
            input_tokens: 1,
            output_tokens: 1,
            completed: true,
            response_chars: 5,
            error: None,
            error_category: None,
        };

        let evaluated = evaluate_case(&case, "ses_1", Some(&result), Some(turn), &[]);
        assert!(evaluated.passed);
        assert!(evaluated.failures.is_empty());
        assert!(evaluated.observed_tools.is_empty());
    }

    #[test]
    fn evaluate_case_fails_on_disallowed_tool_error_category() {
        let case = EvalCase {
            name: "no timeout".into(),
            prompt: "run tool".into(),
            agent: "worker".into(),
            require_completed: true,
            contains_all: Vec::new(),
            contains_any: Vec::new(),
            not_contains: Vec::new(),
            min_tool_calls: Some(1),
            max_tool_calls: None,
            max_tool_errors: Some(0),
            disallow_error_categories: vec![ErrorCategory::Timeout],
        };

        let result = runtime::TurnResult {
            response: "done".into(),
            tool_calls_made: 1,
            tool_calls: Vec::new(),
            tokens_used: Some(2),
            input_tokens: 1,
            output_tokens: 1,
            completed: true,
        };
        let tool = runtime::ToolTelemetry {
            session_id: "ses_2".into(),
            agent_name: "worker".into(),
            turn: 1,
            tool_call_id: "tool-1".into(),
            tool_name: "bash".into(),
            started_at: 1,
            completed_at: 2,
            duration_ms: 1,
            input_bytes: 10,
            output_chars: 7,
            success: false,
            error_category: Some(ErrorCategory::Timeout),
            error: Some("timed out".into()),
        };

        let evaluated = evaluate_case(&case, "ses_2", Some(&result), None, &[tool]);
        assert!(!evaluated.passed);
        assert_eq!(evaluated.tool_errors, 1);
        assert!(
            evaluated
                .failures
                .iter()
                .any(|failure| failure.contains("disallowed error category"))
        );
        assert_eq!(evaluated.observed_tools, vec!["bash".to_string()]);
    }

    #[test]
    fn build_eval_aggregates_groups_agent_tool_and_errors() {
        let results = vec![
            EvalCaseResult {
                name: "case-a".into(),
                agent: "orchestrator".into(),
                session_id: "ses_a".into(),
                passed: true,
                failures: Vec::new(),
                completed: true,
                tool_calls: 1,
                tool_errors: 0,
                turn_error_category: None,
                tool_error_categories: Vec::new(),
                observed_tools: vec!["read_file".into()],
                tool_observations: vec![EvalToolObservation {
                    tool: "read_file".into(),
                    success: true,
                    error_category: None,
                }],
                response_preview: "ok".into(),
            },
            EvalCaseResult {
                name: "case-b".into(),
                agent: "worker".into(),
                session_id: "ses_b".into(),
                passed: false,
                failures: vec!["timeout".into()],
                completed: false,
                tool_calls: 2,
                tool_errors: 1,
                turn_error_category: Some(ErrorCategory::Timeout),
                tool_error_categories: vec![ErrorCategory::ToolExecution],
                observed_tools: vec!["bash".into()],
                tool_observations: vec![
                    EvalToolObservation {
                        tool: "bash".into(),
                        success: false,
                        error_category: Some(ErrorCategory::ToolExecution),
                    },
                    EvalToolObservation {
                        tool: "bash".into(),
                        success: true,
                        error_category: None,
                    },
                ],
                response_preview: "fail".into(),
            },
        ];

        let aggregates = build_eval_aggregates(&results);
        assert_eq!(aggregates.by_agent.len(), 2);
        assert_eq!(aggregates.by_tool.len(), 2);
        assert!(
            aggregates
                .by_error_category
                .iter()
                .any(|item| item.error_category == ErrorCategory::Timeout && item.count == 1)
        );
        assert!(
            aggregates
                .by_tool
                .iter()
                .any(|item| item.tool == "bash" && item.calls == 2 && item.failures == 1)
        );
    }
}
