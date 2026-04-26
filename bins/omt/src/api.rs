//! omt A2A server — exposes omt as a discoverable A2A agent.

use std::collections::HashMap;
use std::sync::Arc;

use a2a::{
    A2aError, A2aHandler, A2aTaskState, AgentCapabilities, AgentCard, AgentRegistration,
    AgentRegistrationResponse, AgentSkill, Artifact, Message, Part, Task, TaskIdParams,
    TaskQueryParams, TaskSendParams, TaskStatus, TeamHeartbeatRequest, TeamJoinRequest,
    TeamJoinResponse, TeamLeaveRequest, TeamStatusResponse,
};
use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::events::OmtBus;
use crate::planner;
use crate::scheduler;
use crate::state;
use crate::team::TeamManager;

/// In-memory task store for the A2A server.
struct TaskStore {
    tasks: HashMap<String, Task>,
}

/// omt's A2A handler — receives tasks from remote agents.
pub struct OmtA2aHandler {
    store: RwLock<TaskStore>,
    concurrency: usize,
    /// Our own endpoint URL — set after bind, used for bidirectional registration.
    self_endpoint: RwLock<Option<String>>,
    /// Team manager for tracking joined omh instances.
    pub team: TeamManager,
}

impl OmtA2aHandler {
    pub fn new(concurrency: usize) -> Self {
        Self {
            store: RwLock::new(TaskStore {
                tasks: HashMap::new(),
            }),
            concurrency,
            self_endpoint: RwLock::new(None),
            team: TeamManager::new(),
        }
    }

    /// Set the endpoint URL after the server binds.
    pub async fn set_self_endpoint(&self, url: String) {
        *self.self_endpoint.write().await = Some(url);
    }
}

#[async_trait]
impl A2aHandler for OmtA2aHandler {
    fn agent_card(&self) -> AgentCard {
        AgentCard {
            name: "omt".to_string(),
            description: Some(
                "oh-my-team: multi-agent task orchestrator. \
                 Decomposes prompts into parallel coding tasks."
                    .to_string(),
            ),
            url: String::new(), // Filled by caller based on bind address.
            provider: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            capabilities: AgentCapabilities {
                streaming: false,
                push_notifications: false,
                state_transition_history: true,
            },
            skills: vec![
                AgentSkill {
                    id: "parallel-coding".to_string(),
                    name: "Parallel Coding".to_string(),
                    description: Some(
                        "Decomposes a coding task into parallel sub-tasks \
                         executed in isolated git worktrees."
                            .to_string(),
                    ),
                    tags: vec![
                        "coding".to_string(),
                        "parallel".to_string(),
                        "orchestration".to_string(),
                    ],
                    examples: vec![
                        "Add authentication to the API and write tests for it".to_string(),
                        "Refactor the database layer and update the CLI".to_string(),
                    ],
                },
            ],
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
        }
    }

    async fn on_task_send(&self, params: TaskSendParams) -> Result<Task, A2aError> {
        let prompt = params.message.text();
        if prompt.is_empty() {
            return Err(A2aError::internal("empty prompt"));
        }

        // Store task as working
        let task_id = params.id.clone();
        let task = Task {
            id: task_id.clone(),
            session_id: params.session_id.clone(),
            status: TaskStatus {
                state: A2aTaskState::Working,
                message: None,
                timestamp: Some(chrono::Utc::now().to_rfc3339()),
            },
            artifacts: vec![],
            history: vec![params.message.clone()],
            metadata: params.metadata.clone(),
        };
        self.store.write().await.tasks.insert(task_id.clone(), task);

        // Plan and execute
        let result = self.execute_prompt(&task_id, &prompt).await;

        // Update task with result
        let mut store = self.store.write().await;
        let task = store.tasks.get_mut(&task_id).unwrap();

        match result {
            Ok(summary) => {
                task.status = TaskStatus {
                    state: A2aTaskState::Completed,
                    message: Some(Message::agent_text(&summary)),
                    timestamp: Some(chrono::Utc::now().to_rfc3339()),
                };
                task.artifacts.push(Artifact {
                    name: Some("result".to_string()),
                    description: Some("Task execution summary".to_string()),
                    parts: vec![Part::Text { text: summary }],
                    index: 0,
                    metadata: None,
                });
            }
            Err(e) => {
                task.status = TaskStatus {
                    state: A2aTaskState::Failed,
                    message: Some(Message::agent_text(format!("error: {e:#}"))),
                    timestamp: Some(chrono::Utc::now().to_rfc3339()),
                };
            }
        }

        Ok(task.clone())
    }

    async fn on_task_get(&self, params: TaskQueryParams) -> Result<Task, A2aError> {
        let store = self.store.read().await;
        store
            .tasks
            .get(&params.id)
            .cloned()
            .ok_or_else(|| A2aError::not_found(&params.id))
    }

    async fn on_task_cancel(&self, params: TaskIdParams) -> Result<Task, A2aError> {
        let mut store = self.store.write().await;
        let task = store
            .tasks
            .get_mut(&params.id)
            .ok_or_else(|| A2aError::not_found(&params.id))?;

        if task.status.state.is_terminal() {
            return Err(A2aError::not_cancelable(&params.id));
        }

        task.status = TaskStatus {
            state: A2aTaskState::Canceled,
            message: Some(Message::agent_text("canceled by client")),
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
        };

        Ok(task.clone())
    }

    async fn on_agent_register(
        &self,
        registration: AgentRegistration,
    ) -> Result<AgentRegistrationResponse, A2aError> {
        let name = registration.card.name.clone();
        let endpoint = registration.endpoint.clone();

        // Register the remote agent locally
        if let Ok(mut reg) = crate::registry::load() {
            let _ = reg.register_local(registration.card, &endpoint);
            tracing::info!("registered remote agent '{name}' from {endpoint}");
        }

        // Respond with our own card for bidirectional registration
        let self_ep = self.self_endpoint.read().await;
        let (peer_card, peer_endpoint) = if let Some(ref ep) = *self_ep {
            let mut card = self.agent_card();
            card.url = ep.clone();
            (Some(card), Some(ep.clone()))
        } else {
            (None, None)
        };

        Ok(AgentRegistrationResponse {
            accepted: true,
            peer_card,
            peer_endpoint,
        })
    }

    async fn on_team_join(
        &self,
        request: TeamJoinRequest,
    ) -> Result<TeamJoinResponse, A2aError> {
        Ok(self.team.handle_join(request).await)
    }

    async fn on_team_leave(&self, request: TeamLeaveRequest) -> Result<(), A2aError> {
        self.team.handle_leave(request).await;
        Ok(())
    }

    async fn on_team_heartbeat(&self, request: TeamHeartbeatRequest) -> Result<(), A2aError> {
        self.team.handle_heartbeat(request).await;
        Ok(())
    }

    async fn on_team_status(&self) -> Result<TeamStatusResponse, A2aError> {
        Ok(self.team.status().await)
    }
}

impl OmtA2aHandler {
    async fn execute_prompt(&self, _task_id: &str, prompt: &str) -> anyhow::Result<String> {
        let plan = planner::plan(prompt).await?;
        let run_id = state::create_run(&plan)?;

        let config = scheduler::SchedulerConfig {
            max_concurrent: self.concurrency,
            team: None,
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let bus = OmtBus::new();

        scheduler::run(run_id.clone(), config, cancel, bus).await?;

        // Load final state and build summary
        let run_state = state::load_state(&run_id)?;
        let counts = run_state.graph.summary();
        let summary = counts
            .iter()
            .map(|(state, count)| format!("{state:?}: {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        Ok(format!("run {run_id} finished — {summary}"))
    }
}

/// Start the omt A2A server on the given address (blocking).
pub async fn serve(bind: &str, concurrency: usize) -> anyhow::Result<()> {
    let handler = Arc::new(OmtA2aHandler::new(concurrency));
    let server = a2a::A2aServer::new(handler);
    server.serve(bind).await
}

/// Default A2A bind address.
pub const DEFAULT_BIND: &str = "127.0.0.1:9120";

/// Spawn the A2A server in the background. Returns the actual bound address
/// and a handle to the team manager for scheduler integration.
///
/// Tries `DEFAULT_BIND` first; if the port is taken, picks an OS-assigned port.
/// Registers omt itself in the local agent registry so other agents can discover it.
pub async fn spawn_background(concurrency: usize) -> anyhow::Result<(String, TeamManager)> {
    use tokio::net::TcpListener;

    let listener = match TcpListener::bind(DEFAULT_BIND).await {
        Ok(l) => l,
        Err(_) => TcpListener::bind("127.0.0.1:0").await?,
    };
    let addr = listener.local_addr()?;
    let bind = addr.to_string();
    let our_url = format!("http://{bind}");

    let handler = Arc::new(OmtA2aHandler::new(concurrency));
    handler.set_self_endpoint(our_url.clone()).await;
    let team = handler.team.clone();

    let router = a2a::A2aServer::new(handler.clone()).router();

    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            tracing::warn!("A2A background server exited: {e}");
        }
    });

    tracing::info!("A2A server listening on {bind}");
    eprintln!("omt: A2A server on {bind}");

    // Self-register in local registry
    if let Ok(mut reg) = crate::registry::load() {
        let mut card = handler.agent_card();
        card.url = our_url.clone();
        let _ = reg.register_local(card.clone(), &our_url);

        // Announce to all known peers — they register us and send their cards back
        if let Err(e) = reg.announce_to_peers(&card, &our_url).await {
            tracing::warn!("peer announcement failed: {e:#}");
        }
    }

    Ok((bind, team))
}
