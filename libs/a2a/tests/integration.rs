//! Integration tests — A2A server ↔ client roundtrip + team management.
//!
//! Each test spins up an in-process axum server on port 0 and exercises
//! the full HTTP path: serialization → routing → handler → response.

use std::sync::Arc;

use a2a::*;
use async_trait::async_trait;
use tokio::net::TcpListener;

// ── Test handler ────────────────────────────────────────────────────

/// Minimal A2A handler that echoes prompts back as completed tasks.
struct EchoHandler;

#[async_trait]
impl A2aHandler for EchoHandler {
    fn agent_card(&self) -> AgentCard {
        AgentCard {
            name: "echo-agent".to_string(),
            description: Some("echoes prompts back".to_string()),
            url: String::new(),
            provider: None,
            version: "0.1.0".to_string(),
            capabilities: AgentCapabilities {
                streaming: false,
                push_notifications: false,
                state_transition_history: false,
            },
            skills: vec![AgentSkill {
                id: "echo".to_string(),
                name: "Echo".to_string(),
                description: Some("echoes input".to_string()),
                tags: vec!["echo".to_string(), "test".to_string()],
                examples: vec![],
            }],
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
        }
    }

    async fn on_task_send(&self, params: TaskSendParams) -> Result<Task, A2aError> {
        let prompt = params.message.text();
        Ok(Task {
            id: params.id,
            session_id: params.session_id,
            status: TaskStatus {
                state: A2aTaskState::Completed,
                message: Some(Message::agent_text(format!("echo: {prompt}"))),
                timestamp: Some("2026-01-01T00:00:00Z".to_string()),
            },
            artifacts: vec![Artifact {
                name: Some("result".to_string()),
                description: None,
                parts: vec![Part::Text {
                    text: format!("echoed: {prompt}"),
                }],
                index: 0,
                metadata: None,
            }],
            history: vec![params.message],
            metadata: None,
        })
    }

    async fn on_task_get(&self, params: TaskQueryParams) -> Result<Task, A2aError> {
        Err(A2aError::not_found(&params.id))
    }

    async fn on_task_cancel(&self, params: TaskIdParams) -> Result<Task, A2aError> {
        Err(A2aError::not_cancelable(&params.id))
    }
}

// ── Team-capable handler ────────────────────────────────────────────

/// Handler that supports team management via an embedded TeamManager-like store.
struct TeamHandler {
    members: tokio::sync::RwLock<Vec<TeamMember>>,
}

impl TeamHandler {
    fn new() -> Self {
        Self {
            members: tokio::sync::RwLock::new(Vec::new()),
        }
    }
}

#[async_trait]
impl A2aHandler for TeamHandler {
    fn agent_card(&self) -> AgentCard {
        AgentCard {
            name: "team-orchestrator".to_string(),
            description: Some("orchestrator with team support".to_string()),
            url: String::new(),
            provider: None,
            version: "0.1.0".to_string(),
            capabilities: AgentCapabilities::default(),
            skills: vec![],
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
        }
    }

    async fn on_task_send(&self, params: TaskSendParams) -> Result<Task, A2aError> {
        Ok(Task {
            id: params.id,
            session_id: None,
            status: TaskStatus {
                state: A2aTaskState::Completed,
                message: None,
                timestamp: None,
            },
            artifacts: vec![],
            history: vec![],
            metadata: None,
        })
    }

    async fn on_task_get(&self, params: TaskQueryParams) -> Result<Task, A2aError> {
        Err(A2aError::not_found(&params.id))
    }

    async fn on_task_cancel(&self, params: TaskIdParams) -> Result<Task, A2aError> {
        Err(A2aError::not_cancelable(&params.id))
    }

    async fn on_team_join(&self, req: TeamJoinRequest) -> Result<TeamJoinResponse, A2aError> {
        let instance_id = format!("{}-{}", req.card.name, self.members.read().await.len());
        let member = TeamMember {
            instance_id: instance_id.clone(),
            card: req.card,
            endpoint: req.endpoint,
            role: req.role,
            capacity: req.capacity,
            active_tasks: 0,
            status: MemberStatus::Active,
            joined_at: "2026-01-01T00:00:00Z".to_string(),
            last_heartbeat: None,
        };
        self.members.write().await.push(member);
        Ok(TeamJoinResponse {
            accepted: true,
            instance_id: Some(instance_id),
            heartbeat_interval_secs: 30,
            message: None,
        })
    }

    async fn on_team_leave(&self, req: TeamLeaveRequest) -> Result<(), A2aError> {
        self.members
            .write()
            .await
            .retain(|m| m.instance_id != req.instance_id);
        Ok(())
    }

    async fn on_team_heartbeat(&self, req: TeamHeartbeatRequest) -> Result<(), A2aError> {
        if let Some(m) = self
            .members
            .write()
            .await
            .iter_mut()
            .find(|m| m.instance_id == req.instance_id)
        {
            m.active_tasks = req.active_tasks;
            m.last_heartbeat = Some("2026-01-01T00:00:01Z".to_string());
        }
        Ok(())
    }

    async fn on_team_status(&self) -> Result<TeamStatusResponse, A2aError> {
        Ok(TeamStatusResponse {
            members: self.members.read().await.clone(),
        })
    }
}

// ── Helper ──────────────────────────────────────────────────────────

/// Spawn an A2A server on an OS-assigned port, return the base URL.
async fn spawn_test_server(handler: Arc<dyn A2aHandler>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = A2aServer::new(handler).router();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

fn make_card(name: &str) -> AgentCard {
    AgentCard {
        name: name.to_string(),
        description: Some(format!("{name} agent")),
        url: String::new(),
        provider: None,
        version: "0.1.0".to_string(),
        capabilities: AgentCapabilities::default(),
        skills: vec![AgentSkill {
            id: "code".to_string(),
            name: "Code".to_string(),
            description: None,
            tags: vec!["coding".to_string()],
            examples: vec![],
        }],
        default_input_modes: vec!["text/plain".to_string()],
        default_output_modes: vec!["text/plain".to_string()],
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[tokio::test]
async fn agent_card_discovery() {
    let handler: Arc<dyn A2aHandler> = Arc::new(EchoHandler);
    let base = spawn_test_server(handler).await;

    let client = A2aClient::new();
    let card = client.fetch_agent_card(&base).await.unwrap();

    assert_eq!(card.name, "echo-agent");
    assert_eq!(card.skills.len(), 1);
    assert_eq!(card.skills[0].id, "echo");
}

#[tokio::test]
async fn send_task_roundtrip() {
    let handler: Arc<dyn A2aHandler> = Arc::new(EchoHandler);
    let base = spawn_test_server(handler).await;

    let client = A2aClient::new();
    let task = client
        .send_task(
            &base,
            TaskSendParams {
                id: "task-001".to_string(),
                session_id: None,
                message: Message::user_text("hello world"),
                metadata: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(task.id, "task-001");
    assert_eq!(task.status.state, A2aTaskState::Completed);
    assert_eq!(
        task.status.message.as_ref().unwrap().text(),
        "echo: hello world"
    );
    assert_eq!(task.artifacts.len(), 1);
    assert!(task.artifacts[0]
        .parts
        .iter()
        .any(|p| matches!(p, Part::Text { text } if text == "echoed: hello world")));
}

#[tokio::test]
async fn get_unknown_task_returns_error() {
    let handler: Arc<dyn A2aHandler> = Arc::new(EchoHandler);
    let base = spawn_test_server(handler).await;

    let client = A2aClient::new();
    let err = client
        .get_task(
            &base,
            TaskQueryParams {
                id: "nonexistent".to_string(),
                history_length: None,
            },
        )
        .await;

    assert!(err.is_err());
    let msg = err.unwrap_err().to_string();
    assert!(msg.contains("not found") || msg.contains("error"), "unexpected: {msg}");
}

#[tokio::test]
async fn cancel_task_not_cancelable() {
    let handler: Arc<dyn A2aHandler> = Arc::new(EchoHandler);
    let base = spawn_test_server(handler).await;

    let client = A2aClient::new();
    let err = client
        .cancel_task(
            &base,
            TaskIdParams {
                id: "task-x".to_string(),
            },
        )
        .await;

    assert!(err.is_err());
}

#[tokio::test]
async fn agent_registration_roundtrip() {
    let handler: Arc<dyn A2aHandler> = Arc::new(EchoHandler);
    let base = spawn_test_server(handler).await;

    let client = A2aClient::new();
    let resp = client
        .register_with_peer(
            &base,
            &AgentRegistration {
                card: make_card("omh-remote"),
                endpoint: "http://localhost:4444".to_string(),
            },
        )
        .await
        .unwrap();

    assert!(resp.accepted);
}

#[tokio::test]
async fn probe_reachable() {
    let handler: Arc<dyn A2aHandler> = Arc::new(EchoHandler);
    let base = spawn_test_server(handler).await;

    let client = A2aClient::new();
    assert!(client.probe(&base).await);
    assert!(!client.probe("http://127.0.0.1:1").await);
}

// ── Team integration tests ─────────────────────────────────────────

#[tokio::test]
async fn team_join_and_status() {
    let handler: Arc<dyn A2aHandler> = Arc::new(TeamHandler::new());
    let base = spawn_test_server(handler).await;
    let client = A2aClient::new();

    // Initially empty
    let status = client.team_status(&base).await.unwrap();
    assert!(status.members.is_empty());

    // Join
    let resp = client
        .team_join(
            &base,
            &TeamJoinRequest {
                card: make_card("omh-1"),
                endpoint: "http://localhost:5001".to_string(),
                role: "coder".to_string(),
                capacity: 2,
            },
        )
        .await
        .unwrap();
    assert!(resp.accepted);
    let iid = resp.instance_id.unwrap();
    assert!(iid.starts_with("omh-1-"));

    // Status should show 1 member
    let status = client.team_status(&base).await.unwrap();
    assert_eq!(status.members.len(), 1);
    assert_eq!(status.members[0].role, "coder");
    assert_eq!(status.members[0].capacity, 2);
}

#[tokio::test]
async fn team_join_leave_lifecycle() {
    let handler: Arc<dyn A2aHandler> = Arc::new(TeamHandler::new());
    let base = spawn_test_server(handler).await;
    let client = A2aClient::new();

    // Join two members
    let r1 = client
        .team_join(
            &base,
            &TeamJoinRequest {
                card: make_card("omh-a"),
                endpoint: "http://localhost:5001".to_string(),
                role: "coder".to_string(),
                capacity: 1,
            },
        )
        .await
        .unwrap();
    let r2 = client
        .team_join(
            &base,
            &TeamJoinRequest {
                card: make_card("omh-b"),
                endpoint: "http://localhost:5002".to_string(),
                role: "reviewer".to_string(),
                capacity: 1,
            },
        )
        .await
        .unwrap();
    assert!(r1.accepted && r2.accepted);

    let status = client.team_status(&base).await.unwrap();
    assert_eq!(status.members.len(), 2);

    // Leave one
    client
        .team_leave(
            &base,
            &TeamLeaveRequest {
                instance_id: r1.instance_id.unwrap(),
            },
        )
        .await
        .unwrap();

    let status = client.team_status(&base).await.unwrap();
    assert_eq!(status.members.len(), 1);
    assert_eq!(status.members[0].role, "reviewer");
}

#[tokio::test]
async fn team_heartbeat_updates_load() {
    let handler: Arc<dyn A2aHandler> = Arc::new(TeamHandler::new());
    let base = spawn_test_server(handler).await;
    let client = A2aClient::new();

    let resp = client
        .team_join(
            &base,
            &TeamJoinRequest {
                card: make_card("omh-hb"),
                endpoint: "http://localhost:5003".to_string(),
                role: "coder".to_string(),
                capacity: 4,
            },
        )
        .await
        .unwrap();
    let iid = resp.instance_id.unwrap();

    // Send heartbeat with active_tasks=3
    client
        .team_heartbeat(
            &base,
            &TeamHeartbeatRequest {
                instance_id: iid.clone(),
                active_tasks: 3,
            },
        )
        .await
        .unwrap();

    let status = client.team_status(&base).await.unwrap();
    let member = &status.members[0];
    assert_eq!(member.active_tasks, 3);
    assert!(member.last_heartbeat.is_some());
}

#[tokio::test]
async fn team_multiple_roles() {
    let handler: Arc<dyn A2aHandler> = Arc::new(TeamHandler::new());
    let base = spawn_test_server(handler).await;
    let client = A2aClient::new();

    // Join 3 members: 2 coders + 1 reviewer
    for (name, role) in [("c1", "coder"), ("c2", "coder"), ("r1", "reviewer")] {
        let resp = client
            .team_join(
                &base,
                &TeamJoinRequest {
                    card: make_card(name),
                    endpoint: format!("http://localhost:600{}", name.len()),
                    role: role.to_string(),
                    capacity: 1,
                },
            )
            .await
            .unwrap();
        assert!(resp.accepted);
    }

    let status = client.team_status(&base).await.unwrap();
    assert_eq!(status.members.len(), 3);

    let coders = status
        .members
        .iter()
        .filter(|m| m.role == "coder")
        .count();
    let reviewers = status
        .members
        .iter()
        .filter(|m| m.role == "reviewer")
        .count();
    assert_eq!(coders, 2);
    assert_eq!(reviewers, 1);
}

#[tokio::test]
async fn full_workflow_discover_join_task_leave() {
    // Simulate the full omt ↔ omh lifecycle:
    // 1. omh discovers omt (agent card fetch)
    // 2. omh joins team
    // 3. omt sends task to omh via A2A
    // 4. omh leaves team

    // Start "omt" server (TeamHandler with task echo)
    let omt: Arc<dyn A2aHandler> = Arc::new(TeamHandler::new());
    let omt_url = spawn_test_server(omt).await;

    // Start "omh" server (EchoHandler, simulating a worker)
    let omh: Arc<dyn A2aHandler> = Arc::new(EchoHandler);
    let omh_url = spawn_test_server(omh).await;

    let client = A2aClient::new();

    // 1. omh discovers omt
    let omt_card = client.fetch_agent_card(&omt_url).await.unwrap();
    assert_eq!(omt_card.name, "team-orchestrator");

    // 2. omh joins omt's team
    let join_resp = client
        .team_join(
            &omt_url,
            &TeamJoinRequest {
                card: make_card("omh-worker"),
                endpoint: omh_url.clone(),
                role: "coder".to_string(),
                capacity: 2,
            },
        )
        .await
        .unwrap();
    assert!(join_resp.accepted);
    let instance_id = join_resp.instance_id.unwrap();

    // Verify team has the member
    let status = client.team_status(&omt_url).await.unwrap();
    assert_eq!(status.members.len(), 1);
    assert_eq!(status.members[0].endpoint, omh_url);

    // 3. omt sends a task to omh (via the registered endpoint)
    let omh_endpoint = &status.members[0].endpoint;
    let task = client
        .send_task(
            omh_endpoint,
            TaskSendParams {
                id: "distributed-task-001".to_string(),
                session_id: None,
                message: Message::user_text("implement auth module"),
                metadata: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(task.status.state, A2aTaskState::Completed);
    assert_eq!(
        task.status.message.as_ref().unwrap().text(),
        "echo: implement auth module"
    );

    // 4. omh leaves the team
    client
        .team_leave(
            &omt_url,
            &TeamLeaveRequest { instance_id },
        )
        .await
        .unwrap();

    let status = client.team_status(&omt_url).await.unwrap();
    assert!(status.members.is_empty());
}
