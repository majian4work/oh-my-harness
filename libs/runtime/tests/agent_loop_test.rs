use std::sync::atomic::{AtomicUsize, Ordering};

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use message::ContentPart;
use provider::mock::{MockProvider, MockResponse, MockToolCall};
use runtime::{AgentRuntime, ErrorCategory, Harness, read_tool_telemetry_jsonl};

#[tokio::test]
async fn agent_turn_returns_text_response() {
    let workspace = TestWorkspace::new("runtime-agent-loop-text");
    workspace.write_agent("test-agent", agent_markdown("test-agent", "ReadOnly"));

    let mut harness = Harness::init(workspace.root()).unwrap();
    let provider = MockProvider::text("hello from mock");
    harness
        .provider_registry
        .register("mock", Box::new(provider.clone()));

    let session = harness
        .session_manager
        .create("test-agent", "mock-model", workspace.root())
        .unwrap();
    let mut runtime = AgentRuntime::new("test-agent".to_string(), session.id, 4);

    let result = runtime.run_turn(&harness, "hello").await.unwrap();

    assert_eq!(result.response, "hello from mock");
    assert_eq!(result.tool_calls_made, 0);
    assert!(result.completed);

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "mock-model");
    assert_eq!(requests[0].messages.len(), 1);
    assert_eq!(requests[0].messages[0].text(), "hello");
}

#[tokio::test]
async fn agent_turn_executes_tool_calls() {
    let workspace = TestWorkspace::new("runtime-agent-loop-tools");
    workspace.write_agent("tool-agent", agent_markdown("tool-agent", "FullAccess"));

    let mut harness = Harness::init(workspace.root()).unwrap();
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            id: "tool-1".to_string(),
            name: "bash".to_string(),
            arguments: serde_json::json!({
                "command": "printf 'tool-ok'"
            }),
        }]),
        MockResponse::Text("tool finished".to_string()),
    ]);
    harness
        .provider_registry
        .register("mock", Box::new(provider.clone()));

    let session = harness
        .session_manager
        .create("tool-agent", "mock-model", workspace.root())
        .unwrap();
    let mut runtime = AgentRuntime::new("tool-agent".to_string(), session.id, 4);

    let result = runtime.run_turn(&harness, "run tool").await.unwrap();

    assert_eq!(result.response, "tool finished");
    assert!(result.tool_calls_made > 0);
    assert!(result.completed);

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.iter().any(|message| {
        message.parts.iter().any(|part| {
            matches!(
                part,
                ContentPart::ToolResult {
                    id,
                    content,
                    is_error
                } if id == "tool-1" && content == "tool-ok" && !is_error
            )
        })
    }));
}

#[tokio::test]
async fn agent_turn_respects_max_turns() {
    let workspace = TestWorkspace::new("runtime-agent-loop-max-turns");
    workspace.write_agent("loop-agent", agent_markdown("loop-agent", "FullAccess"));

    let mut harness = Harness::init(workspace.root()).unwrap();
    let provider = MockProvider::new(vec![
        looping_tool_call("tool-1"),
        looping_tool_call("tool-2"),
    ]);
    harness
        .provider_registry
        .register("mock", Box::new(provider.clone()));

    let session = harness
        .session_manager
        .create("loop-agent", "mock-model", workspace.root())
        .unwrap();
    let mut runtime = AgentRuntime::new("loop-agent".to_string(), session.id, 2);

    let result = runtime.run_turn(&harness, "keep going").await.unwrap();

    assert!(!result.completed);
    assert_eq!(result.tool_calls_made, 2);
    assert_eq!(provider.requests().len(), 2);
}

fn looping_tool_call(id: &str) -> MockResponse {
    MockResponse::ToolCalls(vec![MockToolCall {
        id: id.to_string(),
        name: "bash".to_string(),
        arguments: serde_json::json!({
            "command": "printf 'loop'"
        }),
    }])
}

#[tokio::test]
async fn tool_results_accumulate_across_turns() {
    let workspace = TestWorkspace::new("runtime-tool-results-accumulate");
    workspace.write_agent("acc-agent", agent_markdown("acc-agent", "FullAccess"));

    let mut harness = Harness::init(workspace.root()).unwrap();

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let provider = MockProvider::with_response_fn(move |req| {
        let n = call_count_clone.fetch_add(1, Ordering::SeqCst);
        match n {
            0 => {
                // Turn 1: call glob
                MockResponse::ToolCalls(vec![MockToolCall {
                    id: "glob-1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "printf 'file-a.rs\nfile-b.rs'"}),
                }])
            }
            1 => {
                // Turn 2: verify previous tool_result is in the request
                let has_tool_result = req.messages.iter().any(|m| {
                    m.parts.iter().any(|p| matches!(p, ContentPart::ToolResult { id, content, .. } if id == "glob-1" && content.contains("file-a.rs")))
                });
                assert!(
                    has_tool_result,
                    "Turn 2 request must contain tool_result from turn 1 with glob output"
                );
                // Call another tool
                MockResponse::ToolCalls(vec![MockToolCall {
                    id: "read-1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "printf 'content-of-file'"}),
                }])
            }
            2 => {
                // Turn 3: verify BOTH previous tool_results are present
                let tool_results: Vec<_> = req
                    .messages
                    .iter()
                    .flat_map(|m| m.parts.iter())
                    .filter(|p| matches!(p, ContentPart::ToolResult { .. }))
                    .collect();
                assert!(
                    tool_results.len() >= 2,
                    "Turn 3 must have at least 2 tool_results, got {}",
                    tool_results.len()
                );
                MockResponse::Text("summary complete".into())
            }
            _ => MockResponse::Text("done".into()),
        }
    });

    harness
        .provider_registry
        .register("mock", Box::new(provider.clone()));

    let session = harness
        .session_manager
        .create("acc-agent", "mock-model", workspace.root())
        .unwrap();
    let mut runtime = AgentRuntime::new("acc-agent".into(), session.id, 10);

    let result = runtime
        .run_turn(&harness, "summarize project")
        .await
        .unwrap();

    assert_eq!(result.response, "summary complete");
    assert!(result.completed);
    assert_eq!(call_count.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn tool_telemetry_records_error_category() {
    let workspace = TestWorkspace::new("runtime-tool-telemetry-error-category");
    workspace.write_agent("err-agent", agent_markdown("err-agent", "FullAccess"));

    let mut harness = Harness::init(workspace.root()).unwrap();
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            id: "tool-1".to_string(),
            name: "nope".to_string(),
            arguments: serde_json::json!({}),
        }]),
        MockResponse::Text("handled tool failure".to_string()),
    ]);
    harness
        .provider_registry
        .register("mock", Box::new(provider));

    let session = harness
        .session_manager
        .create("err-agent", "mock-model", workspace.root())
        .unwrap();
    let mut runtime = AgentRuntime::new("err-agent".to_string(), session.id.clone(), 4);

    let result = runtime
        .run_turn(&harness, "run missing tool")
        .await
        .unwrap();
    assert_eq!(result.response, "handled tool failure");

    let path = harness.session_manager.tool_telemetry_path(&session.id);
    let records = read_tool_telemetry_jsonl(&path).unwrap();
    assert_eq!(records.len(), 1);
    assert!(!records[0].success);
    assert_eq!(records[0].error_category, Some(ErrorCategory::ToolNotFound));
}

fn agent_markdown(name: &str, permission_level: &str) -> String {
    format!(
        "# {name}\n\nIntegration test agent\n\n## Config\n- mode: primary\n- cost: cheap\n- model: mock-model\n- provider: mock\n- permission_level: {permission_level}\n\n## System Prompt\nReturn mock provider responses.\n"
    )
}

struct TestWorkspace {
    root: PathBuf,
}

impl TestWorkspace {
    fn new(prefix: &str) -> Self {
        let root = std::env::temp_dir().join(format!("{prefix}-{}", ulid::Ulid::new()));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn write_agent(&self, name: &str, content: String) {
        let agents_dir = self.root.join(".omh/agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join(format!("{name}.md")), content).unwrap();
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
