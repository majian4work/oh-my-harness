//! A2A HTTP server — axum-based JSON-RPC endpoint + agent card.

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event as SseEvent, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures::Stream;
use tokio::net::TcpListener;

use crate::types::*;

/// Stream of task events for `tasks/sendSubscribe`.
pub type TaskEventStream = Pin<Box<dyn Stream<Item = TaskEvent> + Send>>;

/// Trait implemented by an A2A agent to handle incoming requests.
#[async_trait]
pub trait A2aHandler: Send + Sync + 'static {
    /// Return this agent's card.
    fn agent_card(&self) -> AgentCard;

    /// Handle `tasks/send` — execute a task synchronously and return the result.
    async fn on_task_send(&self, params: TaskSendParams) -> Result<Task, A2aError>;

    /// Handle `tasks/get` — return current task state.
    async fn on_task_get(&self, params: TaskQueryParams) -> Result<Task, A2aError>;

    /// Handle `tasks/cancel` — cancel a running task.
    async fn on_task_cancel(&self, params: TaskIdParams) -> Result<Task, A2aError>;

    /// Handle `tasks/sendSubscribe` — execute and stream progress via SSE.
    /// Default implementation falls back to `on_task_send` with a single final event.
    async fn on_task_subscribe(
        &self,
        params: TaskSendParams,
    ) -> Result<TaskEventStream, A2aError> {
        let task = self.on_task_send(params).await?;
        let event = TaskEvent::Status(TaskStatusEvent {
            id: task.id.clone(),
            status: task.status.clone(),
            final_: true,
        });
        Ok(Box::pin(futures::stream::once(async move { event })))
    }

    /// Called when a remote agent pushes its registration to us.
    /// Return our own card + endpoint if we want bidirectional registration.
    /// Default: accept and return None (no reciprocal push).
    async fn on_agent_register(
        &self,
        _registration: AgentRegistration,
    ) -> Result<AgentRegistrationResponse, A2aError> {
        Ok(AgentRegistrationResponse {
            accepted: true,
            peer_card: None,
            peer_endpoint: None,
        })
    }

    /// Handle team join request. Default: reject (not a team-capable server).
    async fn on_team_join(
        &self,
        _request: TeamJoinRequest,
    ) -> Result<TeamJoinResponse, A2aError> {
        Ok(TeamJoinResponse {
            accepted: false,
            instance_id: None,
            heartbeat_interval_secs: 0,
            message: Some("this agent does not support team management".to_string()),
        })
    }

    /// Handle team leave request. Default: no-op.
    async fn on_team_leave(&self, _request: TeamLeaveRequest) -> Result<(), A2aError> {
        Ok(())
    }

    /// Handle heartbeat from a team member. Default: no-op.
    async fn on_team_heartbeat(&self, _request: TeamHeartbeatRequest) -> Result<(), A2aError> {
        Ok(())
    }

    /// Return current team status. Default: empty.
    async fn on_team_status(&self) -> Result<TeamStatusResponse, A2aError> {
        Ok(TeamStatusResponse {
            members: vec![],
        })
    }
}

/// A2A server wrapping an axum router.
pub struct A2aServer {
    handler: Arc<dyn A2aHandler>,
}

impl A2aServer {
    pub fn new(handler: Arc<dyn A2aHandler>) -> Self {
        Self { handler }
    }

    /// Build the axum [`Router`] for this A2A server.
    pub fn router(&self) -> Router {
        let state = self.handler.clone();
        Router::new()
            .route("/.well-known/agent.json", get(handle_agent_card))
            .route("/agents/register", post(handle_agent_register))
            .route("/team/join", post(handle_team_join))
            .route("/team/leave", post(handle_team_leave))
            .route("/team/heartbeat", post(handle_team_heartbeat))
            .route("/team/status", get(handle_team_status))
            .route("/", post(handle_jsonrpc))
            .with_state(state)
    }

    /// Serve on the given address (e.g. `"0.0.0.0:8080"`).
    pub async fn serve(self, addr: &str) -> anyhow::Result<()> {
        let router = self.router();
        let listener = TcpListener::bind(addr).await?;
        tracing::info!("A2A server listening on {addr}");
        axum::serve(listener, router).await?;
        Ok(())
    }
}

// ── Handlers ────────────────────────────────────────────────────────

async fn handle_agent_card(
    State(handler): State<Arc<dyn A2aHandler>>,
) -> Json<AgentCard> {
    Json(handler.agent_card())
}

async fn handle_jsonrpc(
    State(handler): State<Arc<dyn A2aHandler>>,
    Json(request): Json<JsonRpcRequest>,
) -> Response {
    match request.method.as_str() {
        "tasks/send" => {
            handle_task_send(handler, request).await
        }
        "tasks/get" => {
            handle_task_get(handler, request).await
        }
        "tasks/cancel" => {
            handle_task_cancel(handler, request).await
        }
        "tasks/sendSubscribe" => {
            handle_task_subscribe(handler, request).await
        }
        _ => {
            let err = JsonRpcError {
                code: METHOD_NOT_FOUND,
                message: format!("unknown method: {}", request.method),
                data: None,
            };
            Json(JsonRpcResponse::error(request.id, err)).into_response()
        }
    }
}

async fn handle_task_send(
    handler: Arc<dyn A2aHandler>,
    request: JsonRpcRequest,
) -> Response {
    let params = match serde_json::from_value::<TaskSendParams>(request.params) {
        Ok(p) => p,
        Err(e) => return rpc_error(request.id, INVALID_PARAMS, &e.to_string()),
    };

    match handler.on_task_send(params).await {
        Ok(task) => rpc_success(request.id, &task),
        Err(e) => rpc_error(request.id, e.code, &e.message),
    }
}

async fn handle_task_get(
    handler: Arc<dyn A2aHandler>,
    request: JsonRpcRequest,
) -> Response {
    let params = match serde_json::from_value::<TaskQueryParams>(request.params) {
        Ok(p) => p,
        Err(e) => return rpc_error(request.id, INVALID_PARAMS, &e.to_string()),
    };

    match handler.on_task_get(params).await {
        Ok(task) => rpc_success(request.id, &task),
        Err(e) => rpc_error(request.id, e.code, &e.message),
    }
}

async fn handle_task_cancel(
    handler: Arc<dyn A2aHandler>,
    request: JsonRpcRequest,
) -> Response {
    let params = match serde_json::from_value::<TaskIdParams>(request.params) {
        Ok(p) => p,
        Err(e) => return rpc_error(request.id, INVALID_PARAMS, &e.to_string()),
    };

    match handler.on_task_cancel(params).await {
        Ok(task) => rpc_success(request.id, &task),
        Err(e) => rpc_error(request.id, e.code, &e.message),
    }
}

async fn handle_task_subscribe(
    handler: Arc<dyn A2aHandler>,
    request: JsonRpcRequest,
) -> Response {
    let params = match serde_json::from_value::<TaskSendParams>(request.params) {
        Ok(p) => p,
        Err(e) => return rpc_error(request.id, INVALID_PARAMS, &e.to_string()),
    };

    match handler.on_task_subscribe(params).await {
        Ok(stream) => {
            let sse_stream = futures::StreamExt::map(stream, |event| {
                let notification = event.to_notification();
                let data = serde_json::to_string(&notification).unwrap_or_default();
                Ok::<_, std::convert::Infallible>(SseEvent::default().data(data))
            });
            Sse::new(sse_stream)
                .keep_alive(KeepAlive::default())
                .into_response()
        }
        Err(e) => rpc_error(request.id, e.code, &e.message),
    }
}

async fn handle_agent_register(
    State(handler): State<Arc<dyn A2aHandler>>,
    Json(registration): Json<AgentRegistration>,
) -> Response {
    match handler.on_agent_register(registration).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.message })),
        )
            .into_response(),
    }
}

async fn handle_team_join(
    State(handler): State<Arc<dyn A2aHandler>>,
    Json(request): Json<TeamJoinRequest>,
) -> Response {
    match handler.on_team_join(request).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.message })),
        )
            .into_response(),
    }
}

async fn handle_team_leave(
    State(handler): State<Arc<dyn A2aHandler>>,
    Json(request): Json<TeamLeaveRequest>,
) -> Response {
    match handler.on_team_leave(request).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(_e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({}))).into_response(),
    }
}

async fn handle_team_heartbeat(
    State(handler): State<Arc<dyn A2aHandler>>,
    Json(request): Json<TeamHeartbeatRequest>,
) -> Response {
    match handler.on_team_heartbeat(request).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(_e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({}))).into_response(),
    }
}

async fn handle_team_status(
    State(handler): State<Arc<dyn A2aHandler>>,
) -> Response {
    match handler.on_team_status().await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.message })),
        )
            .into_response(),
    }
}

fn rpc_success(id: Option<serde_json::Value>, result: &impl serde::Serialize) -> Response {
    let value = serde_json::to_value(result).unwrap_or_default();
    Json(JsonRpcResponse::success(id, value)).into_response()
}

fn rpc_error(id: Option<serde_json::Value>, code: i32, message: &str) -> Response {
    let err = JsonRpcError {
        code,
        message: message.to_string(),
        data: None,
    };
    let status = if code == INVALID_PARAMS || code == INVALID_REQUEST {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::OK
    };
    (status, Json(JsonRpcResponse::error(id, err))).into_response()
}
