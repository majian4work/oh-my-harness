use std::sync::atomic::{AtomicUsize, Ordering};

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use message::ContentPart;
use provider::mock::{MockProvider, MockResponse, MockToolCall};
use runtime::{
    AgentRuntime, ErrorCategory, Harness, TurnRouting, read_telemetry_jsonl,
    read_tool_telemetry_jsonl,
};

#[tokio::test]
async fn agent_turn_returns_text_response() {
    let workspace = TestWorkspace::new("runtime-agent-loop-text");
    workspace.write_agent("test-agent", agent_markdown("test-agent", "ReadOnly", true));

    let mut harness = workspace.init_harness();
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
    workspace.write_agent(
        "tool-agent",
        agent_markdown("tool-agent", "FullAccess", true),
    );

    let mut harness = workspace.init_harness();
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
    workspace.write_agent(
        "loop-agent",
        agent_markdown("loop-agent", "FullAccess", true),
    );

    let mut harness = workspace.init_harness();
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
    workspace.write_agent("acc-agent", agent_markdown("acc-agent", "FullAccess", true));

    let mut harness = workspace.init_harness();

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
    workspace.write_agent("err-agent", agent_markdown("err-agent", "FullAccess", true));

    let mut harness = workspace.init_harness();
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

#[tokio::test]
async fn explicit_routing_bypasses_current_primary_agent() {
    let workspace = TestWorkspace::new("runtime-explicit-routing-direct");
    workspace.write_agent(
        "primary-agent",
        agent_markdown("primary-agent", "ReadOnly", true),
    );
    workspace.write_agent(
        "direct-agent",
        agent_markdown("direct-agent", "ReadOnly", true),
    );

    let mut harness = workspace.init_harness();
    let provider = MockProvider::with_response_fn(|req| {
        let system = req
            .system
            .first()
            .map(|msg| msg.content.as_str())
            .unwrap_or_default();
        if system.contains("# primary-agent") {
            MockResponse::Text("primary handled turn".into())
        } else if system.contains("# direct-agent") {
            MockResponse::Text("direct handled turn".into())
        } else {
            panic!("unexpected system prompt: {system}");
        }
    });
    harness
        .provider_registry
        .register("mock", Box::new(provider.clone()));

    let session = harness
        .session_manager
        .create("primary-agent", "mock-model", workspace.root())
        .unwrap();
    let mut runtime = AgentRuntime::new("primary-agent".to_string(), session.id.clone(), 4)
        .with_turn_routing(TurnRouting::direct("direct-agent"));

    let result = runtime.run_turn(&harness, "route directly").await.unwrap();

    assert_eq!(result.response, "direct handled turn");
    assert_eq!(provider.requests().len(), 1);
    let session = harness.session_manager.get(&session.id).unwrap();
    assert_eq!(session.agent_name, "primary-agent");
}

#[tokio::test]
async fn explicit_routing_blocks_nested_delegation() {
    let workspace = TestWorkspace::new("runtime-explicit-routing-leaf");
    workspace.write_agent(
        "primary-agent",
        agent_markdown("primary-agent", "ReadOnly", true),
    );
    workspace.write_agent(
        "direct-agent",
        agent_markdown("direct-agent", "FullAccess", true),
    );
    workspace.write_agent("worker", agent_markdown("worker", "ReadOnly", true));

    let mut harness = workspace.init_harness();
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            id: "spawn-1".to_string(),
            name: "spawn_agent".to_string(),
            arguments: serde_json::json!({
                "agent_name": "worker",
                "prompt": "do work",
                "background": false
            }),
        }]),
        MockResponse::Text("leaf turn completed".to_string()),
    ]);
    harness
        .provider_registry
        .register("mock", Box::new(provider.clone()));

    let session = harness
        .session_manager
        .create("primary-agent", "mock-model", workspace.root())
        .unwrap();
    let mut runtime = AgentRuntime::new("primary-agent".to_string(), session.id.clone(), 4)
        .with_turn_routing(TurnRouting::direct("direct-agent"));

    let result = runtime.run_turn(&harness, "route directly").await.unwrap();

    assert_eq!(result.response, "leaf turn completed");
    assert_eq!(provider.requests().len(), 2);
    assert!(provider.requests()[1].messages.iter().any(|message| {
        message.parts.iter().any(|part| {
            matches!(
                part,
                ContentPart::ToolResult {
                    id,
                    content,
                    is_error
                } if id == "spawn-1"
                    && *is_error
                    && content.contains("leaf-only")
            )
        })
    }));
    assert!(
        !workspace
            .subagent_session_path(&session.id, "worker")
            .exists()
    );
}

#[tokio::test]
async fn explicit_routing_rejects_non_user_invocable_agent() {
    let workspace = TestWorkspace::new("runtime-explicit-routing-non-invocable");
    workspace.write_agent(
        "primary-agent",
        agent_markdown("primary-agent", "ReadOnly", true),
    );
    workspace.write_agent(
        "secret-agent",
        agent_markdown("secret-agent", "ReadOnly", false),
    );

    let mut harness = workspace.init_harness();
    let provider = MockProvider::text("should not run");
    harness
        .provider_registry
        .register("mock", Box::new(provider.clone()));

    let session = harness
        .session_manager
        .create("primary-agent", "mock-model", workspace.root())
        .unwrap();
    let mut runtime = AgentRuntime::new("primary-agent".to_string(), session.id, 4)
        .with_turn_routing(TurnRouting::direct("secret-agent"));

    let error = match runtime.run_turn(&harness, "route directly").await {
        Ok(_) => panic!("non-invocable explicit target should fail"),
        Err(error) => error,
    };

    assert_eq!(
        error.to_string(),
        "agent 'secret-agent' cannot be invoked explicitly"
    );
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn primary_agent_can_delegate_to_declared_subagent() {
    let workspace = TestWorkspace::new("runtime-delegation-allowed");
    workspace.write_agent(
        "primary-agent",
        agent_markdown_with_delegates("primary-agent", "FullAccess", true, &["worker"]),
    );
    workspace.write_agent(
        "worker",
        subagent_markdown_with_delegates("worker", "ReadOnly", true, &[]),
    );

    let mut harness = workspace.init_harness();
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            id: "spawn-1".to_string(),
            name: "spawn_agent".to_string(),
            arguments: serde_json::json!({
                "agent_name": "worker",
                "prompt": "do work",
                "background": false
            }),
        }]),
        MockResponse::Text("worker completed".to_string()),
        MockResponse::Text("primary turn completed".to_string()),
    ]);
    harness
        .provider_registry
        .register("mock", Box::new(provider.clone()));

    let session = harness
        .session_manager
        .create("primary-agent", "mock-model", workspace.root())
        .unwrap();
    let mut runtime = AgentRuntime::new("primary-agent".to_string(), session.id.clone(), 4);

    let result = runtime.run_turn(&harness, "delegate").await.unwrap();

    assert_eq!(result.response, "primary turn completed");
    assert_eq!(provider.requests().len(), 3);
    assert!(provider.requests()[2].messages.iter().any(|message| {
        message.parts.iter().any(|part| {
            matches!(
                part,
                ContentPart::ToolResult {
                    id,
                    content,
                    is_error
                } if id == "spawn-1"
                    && !*is_error
                    && content.contains("worker")
            )
        })
    }));
    assert!(
        workspace
            .subagent_session_path(&session.id, "worker")
            .exists()
    );
}

#[tokio::test]
async fn primary_agent_cannot_delegate_to_undeclared_subagent() {
    let workspace = TestWorkspace::new("runtime-delegation-blocked");
    workspace.write_agent(
        "primary-agent",
        agent_markdown_with_delegates("primary-agent", "FullAccess", true, &["oracle"]),
    );
    workspace.write_agent(
        "worker",
        subagent_markdown_with_delegates("worker", "ReadOnly", true, &[]),
    );

    let mut harness = workspace.init_harness();
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            id: "spawn-1".to_string(),
            name: "spawn_agent".to_string(),
            arguments: serde_json::json!({
                "agent_name": "worker",
                "prompt": "do work",
                "background": false
            }),
        }]),
        MockResponse::Text("primary turn completed".to_string()),
    ]);
    harness
        .provider_registry
        .register("mock", Box::new(provider.clone()));

    let session = harness
        .session_manager
        .create("primary-agent", "mock-model", workspace.root())
        .unwrap();
    let mut runtime = AgentRuntime::new("primary-agent".to_string(), session.id.clone(), 4);

    let result = runtime.run_turn(&harness, "delegate").await.unwrap();

    assert_eq!(result.response, "primary turn completed");
    assert_eq!(provider.requests().len(), 2);
    assert!(provider.requests()[1].messages.iter().any(|message| {
        message.parts.iter().any(|part| {
            matches!(
                part,
                ContentPart::ToolResult {
                    id,
                    content,
                    is_error
                } if id == "spawn-1"
                    && *is_error
                    && content.contains("Delegation policy denied")
                    && content.contains("primary-agent")
                    && content.contains("worker")
                    && content.contains("oracle")
            )
        })
    }));
    assert!(
        !workspace
            .subagent_session_path(&session.id, "worker")
            .exists()
    );
}

#[tokio::test]
async fn explicit_route_telemetry_records_session_primary_agent() {
    let workspace = TestWorkspace::new("runtime-task10-telemetry-primary");
    workspace.write_agent(
        "primary-agent",
        agent_markdown("primary-agent", "ReadOnly", true),
    );
    workspace.write_agent(
        "direct-agent",
        agent_markdown("direct-agent", "ReadOnly", true),
    );

    let mut harness = workspace.init_harness();
    let provider = MockProvider::with_response_fn(|req| {
        let system = req
            .system
            .first()
            .map(|msg| msg.content.as_str())
            .unwrap_or_default();
        if system.contains("# direct-agent") {
            MockResponse::Text("direct handled".into())
        } else {
            MockResponse::Text("primary handled".into())
        }
    });
    harness
        .provider_registry
        .register("mock", Box::new(provider));

    let session = harness
        .session_manager
        .create("primary-agent", "mock-model", workspace.root())
        .unwrap();

    let mut runtime = AgentRuntime::new("primary-agent".to_string(), session.id.clone(), 4)
        .with_turn_routing(TurnRouting::direct("direct-agent"))
        .with_logger(&harness);
    runtime.run_turn(&harness, "route me").await.unwrap();

    let telemetry_path = harness.session_manager.telemetry_path(&session.id);
    let records = read_telemetry_jsonl(&telemetry_path).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].agent_name, "direct-agent");
    assert_eq!(
        records[0].session_primary_agent.as_deref(),
        Some("primary-agent")
    );
}

#[tokio::test]
async fn normal_turn_telemetry_has_no_session_primary_agent_field() {
    let workspace = TestWorkspace::new("runtime-task10-telemetry-no-primary");
    workspace.write_agent(
        "primary-agent",
        agent_markdown("primary-agent", "ReadOnly", true),
    );

    let mut harness = workspace.init_harness();
    let provider = MockProvider::text("response");
    harness
        .provider_registry
        .register("mock", Box::new(provider));

    let session = harness
        .session_manager
        .create("primary-agent", "mock-model", workspace.root())
        .unwrap();

    let mut runtime =
        AgentRuntime::new("primary-agent".to_string(), session.id.clone(), 4).with_logger(&harness);
    runtime.run_turn(&harness, "hello").await.unwrap();

    let telemetry_path = harness.session_manager.telemetry_path(&session.id);
    let records = read_telemetry_jsonl(&telemetry_path).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].agent_name, "primary-agent");
    assert!(records[0].session_primary_agent.is_none());
}

#[tokio::test]
async fn primary_agent_persists_across_session_restore() {
    let workspace = TestWorkspace::new("runtime-task10-session-restore");

    let harness = workspace.init_harness();

    let session = harness
        .session_manager
        .create("orchestrator", "mock-model", workspace.root())
        .unwrap();

    harness
        .session_manager
        .update_agent_name(&session.id, "planner")
        .unwrap();

    let restored = harness.session_manager.get(&session.id).unwrap();
    assert_eq!(restored.agent_name, "planner");
}

fn agent_markdown(name: &str, permission_level: &str, user_invocable: bool) -> String {
    agent_markdown_with_delegates(name, permission_level, user_invocable, &[])
}

fn agent_markdown_with_delegates(
    name: &str,
    permission_level: &str,
    user_invocable: bool,
    can_delegate_to: &[&str],
) -> String {
    let delegation_list = format_front_matter_list(can_delegate_to);
    format!(
        "---\nname: {name}\nuser_invocable: {user_invocable}\ncan_delegate_to: {delegation_list}\nconfig:\n  mode: primary\n  cost: cheap\n  model: mock-model\n  provider: mock\n  permission_level: {permission_level}\n---\n# {name}\nReturn mock provider responses.\n"
    )
}

fn subagent_markdown_with_delegates(
    name: &str,
    permission_level: &str,
    user_invocable: bool,
    can_delegate_to: &[&str],
) -> String {
    let delegation_list = format_front_matter_list(can_delegate_to);
    format!(
        "---\nname: {name}\nuser_invocable: {user_invocable}\ncan_delegate_to: {delegation_list}\nconfig:\n  mode: subagent\n  cost: cheap\n  model: mock-model\n  provider: mock\n  permission_level: {permission_level}\n---\n# {name}\nReturn mock provider responses.\n"
    )
}

fn format_front_matter_list(values: &[&str]) -> String {
    if values.is_empty() {
        "[]".to_string()
    } else {
        format!("[{}]", values.join(", "))
    }
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

    fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    fn init_harness(&self) -> Harness {
        Harness::init_with_sessions_dir(self.root(), self.sessions_dir()).unwrap()
    }

    fn write_agent(&self, name: &str, content: String) {
        let agents_dir = self.root.join(".omh/agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join(format!("{name}.md")), content).unwrap();
    }

    fn subagent_session_path(&self, parent_session_id: &str, agent_name: &str) -> PathBuf {
        self.sessions_dir()
            .join(parent_session_id)
            .join("agents")
            .join(agent_name)
            .join("session.jsonl")
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
