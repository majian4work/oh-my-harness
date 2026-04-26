//! Google A2A (Agent-to-Agent) protocol types.
//!
//! Follows the A2A specification for agent interoperability.
//! JSON-RPC 2.0 transport over HTTP, SSE for streaming.

use serde::{Deserialize, Serialize};

// ── Agent Card ──────────────────────────────────────────────────────

/// Agent Card — served at `/.well-known/agent.json`.
/// Describes an agent's identity, capabilities, and skills.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The agent's A2A endpoint URL.
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<AgentProvider>,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub capabilities: AgentCapabilities,
    #[serde(default)]
    pub skills: Vec<AgentSkill>,
    #[serde(default = "default_modes")]
    pub default_input_modes: Vec<String>,
    #[serde(default = "default_modes")]
    pub default_output_modes: Vec<String>,
}

fn default_version() -> String {
    "1.0".to_string()
}

fn default_modes() -> Vec<String> {
    vec!["text/plain".to_string()]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProvider {
    pub organization: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub push_notifications: bool,
    #[serde(default)]
    pub state_transition_history: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub examples: Vec<String>,
}

// ── Task ────────────────────────────────────────────────────────────

/// Task states per A2A lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum A2aTaskState {
    Submitted,
    Working,
    InputRequired,
    Completed,
    Failed,
    Canceled,
}

impl A2aTaskState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Canceled)
    }
}

/// Task status with optional status message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatus {
    pub state: A2aTaskState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

/// A task — the unit of work in A2A.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

// ── Message ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageRole {
    User,
    Agent,
}

/// A message in the A2A conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub role: MessageRole,
    pub parts: Vec<Part>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            parts: vec![Part::Text {
                text: text.into(),
            }],
            metadata: None,
        }
    }

    pub fn agent_text(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Agent,
            parts: vec![Part::Text {
                text: text.into(),
            }],
            metadata: None,
        }
    }

    /// Extract all text content concatenated.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Part::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

/// A message part — text, file, or structured data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Part {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "file")]
    File { file: FileContent },
    #[serde(rename = "data")]
    Data { data: serde_json::Value },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Base64-encoded bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<String>,
    /// URI to fetch the file from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

/// An output artifact produced by an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parts: Vec<Part>,
    #[serde(default)]
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

// ── JSON-RPC 2.0 ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn success(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Option<serde_json::Value>, err: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(err),
        }
    }
}

/// JSON-RPC notification (no id, used in SSE streams).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

// Standard JSON-RPC error codes.
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

// A2A-specific error codes.
pub const TASK_NOT_FOUND: i32 = -32001;
pub const TASK_NOT_CANCELABLE: i32 = -32002;

// ── Agent Registration (push-based) ────────────────────────────────

/// Request body for `POST /agents/register`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRegistration {
    /// The registering agent's card.
    pub card: AgentCard,
    /// The endpoint where the agent can be reached.
    pub endpoint: String,
}

/// Response for `POST /agents/register`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRegistrationResponse {
    pub accepted: bool,
    /// If the receiver also wants the caller to register it, it sends its own card back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_card: Option<AgentCard>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_endpoint: Option<String>,
}

// ── Team Management ─────────────────────────────────────────────────

/// Member status within a team.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MemberStatus {
    Active,
    Draining,
    Offline,
}

/// A team member — an agent instance participating in a team.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamMember {
    /// Unique instance id (e.g. hostname-pid).
    pub instance_id: String,
    /// The agent's card.
    pub card: AgentCard,
    /// A2A endpoint.
    pub endpoint: String,
    /// Role within the team (e.g. "coder", "reviewer", "tester").
    pub role: String,
    /// Max concurrent tasks this member can handle.
    #[serde(default = "default_capacity")]
    pub capacity: u32,
    /// Currently assigned task count.
    #[serde(default)]
    pub active_tasks: u32,
    pub status: MemberStatus,
    pub joined_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_heartbeat: Option<String>,
}

fn default_capacity() -> u32 {
    1
}

/// Request body for `POST /team/join`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamJoinRequest {
    /// The joining agent's card.
    pub card: AgentCard,
    /// Where to reach this agent.
    pub endpoint: String,
    /// Role the agent wants to fill.
    pub role: String,
    /// Max concurrent tasks.
    #[serde(default = "default_capacity")]
    pub capacity: u32,
}

/// Response for `POST /team/join`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamJoinResponse {
    pub accepted: bool,
    /// The assigned instance id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// Heartbeat interval in seconds (0 = no heartbeat required).
    #[serde(default)]
    pub heartbeat_interval_secs: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Request body for `POST /team/leave`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamLeaveRequest {
    pub instance_id: String,
}

/// Request body for `POST /team/heartbeat`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamHeartbeatRequest {
    pub instance_id: String,
    /// Current active task count.
    #[serde(default)]
    pub active_tasks: u32,
}

/// Response for `GET /team/status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamStatusResponse {
    pub members: Vec<TeamMember>,
}

// ── Method Params ───────────────────────────────────────────────────

/// Params for `tasks/send` and `tasks/sendSubscribe`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSendParams {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub message: Message,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Params for `tasks/get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskQueryParams {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_length: Option<u32>,
}

/// Params for `tasks/cancel`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskIdParams {
    pub id: String,
}

// ── Streaming Events ────────────────────────────────────────────────

/// Events emitted during streaming (`tasks/sendSubscribe`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusEvent {
    pub id: String,
    pub status: TaskStatus,
    #[serde(rename = "final")]
    pub final_: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskArtifactEvent {
    pub id: String,
    pub artifact: Artifact,
}

/// Unified streaming event.
pub enum TaskEvent {
    Status(TaskStatusEvent),
    Artifact(TaskArtifactEvent),
}

impl TaskEvent {
    /// Convert to a JSON-RPC notification for SSE.
    pub fn to_notification(&self) -> JsonRpcNotification {
        match self {
            TaskEvent::Status(e) => JsonRpcNotification {
                jsonrpc: "2.0".to_string(),
                method: "tasks/status".to_string(),
                params: serde_json::to_value(e).unwrap_or_default(),
            },
            TaskEvent::Artifact(e) => JsonRpcNotification {
                jsonrpc: "2.0".to_string(),
                method: "tasks/artifact".to_string(),
                params: serde_json::to_value(e).unwrap_or_default(),
            },
        }
    }
}

// ── A2A Error ───────────────────────────────────────────────────────

/// Application-level error returned by A2A handlers.
#[derive(Debug, Clone)]
pub struct A2aError {
    pub code: i32,
    pub message: String,
}

impl A2aError {
    pub fn not_found(id: &str) -> Self {
        Self {
            code: TASK_NOT_FOUND,
            message: format!("task not found: {id}"),
        }
    }

    pub fn not_cancelable(id: &str) -> Self {
        Self {
            code: TASK_NOT_CANCELABLE,
            message: format!("task is not cancelable: {id}"),
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: INTERNAL_ERROR,
            message: msg.into(),
        }
    }

    pub fn to_jsonrpc(&self) -> JsonRpcError {
        JsonRpcError {
            code: self.code,
            message: self.message.clone(),
            data: None,
        }
    }
}

impl std::fmt::Display for A2aError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "A2A error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for A2aError {}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_card_roundtrip() {
        let card = AgentCard {
            name: "test-agent".to_string(),
            description: Some("A test agent".to_string()),
            url: "http://localhost:8080".to_string(),
            provider: None,
            version: "1.0".to_string(),
            capabilities: AgentCapabilities {
                streaming: true,
                push_notifications: false,
                state_transition_history: true,
            },
            skills: vec![AgentSkill {
                id: "code".to_string(),
                name: "Code Generation".to_string(),
                description: Some("Generates code".to_string()),
                tags: vec!["coding".to_string()],
                examples: vec!["Write a function".to_string()],
            }],
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
        };

        let json = serde_json::to_string(&card).unwrap();
        let parsed: AgentCard = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "test-agent");
        assert!(parsed.capabilities.streaming);
        assert_eq!(parsed.skills.len(), 1);
        assert_eq!(parsed.skills[0].tags[0], "coding");
    }

    #[test]
    fn task_roundtrip() {
        let task = Task {
            id: "task-1".to_string(),
            session_id: None,
            status: TaskStatus {
                state: A2aTaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: vec![],
            history: vec![Message::user_text("do something")],
            metadata: None,
        };

        let json = serde_json::to_string(&task).unwrap();
        let parsed: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "task-1");
        assert_eq!(parsed.status.state, A2aTaskState::Working);
        assert_eq!(parsed.history.len(), 1);
    }

    #[test]
    fn message_parts_serde() {
        let msg = Message {
            role: MessageRole::User,
            parts: vec![
                Part::Text {
                    text: "hello".to_string(),
                },
                Part::Data {
                    data: serde_json::json!({"key": "value"}),
                },
            ],
            metadata: None,
        };

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"text"#));
        assert!(json.contains(r#""type":"data"#));

        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.parts.len(), 2);
        assert_eq!(parsed.text(), "hello");
    }

    #[test]
    fn jsonrpc_request_serde() {
        let json = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tasks/send",
            "params": {
                "id": "task-1",
                "message": {
                    "role": "user",
                    "parts": [{"type": "text", "text": "hello"}]
                }
            }
        }"#;

        let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.method, "tasks/send");
        let params: TaskSendParams = serde_json::from_value(req.params).unwrap();
        assert_eq!(params.id, "task-1");
        assert_eq!(params.message.text(), "hello");
    }

    #[test]
    fn task_state_terminal() {
        assert!(A2aTaskState::Completed.is_terminal());
        assert!(A2aTaskState::Failed.is_terminal());
        assert!(A2aTaskState::Canceled.is_terminal());
        assert!(!A2aTaskState::Working.is_terminal());
        assert!(!A2aTaskState::Submitted.is_terminal());
    }
}
