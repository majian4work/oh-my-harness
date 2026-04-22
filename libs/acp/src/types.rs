use serde::{Deserialize, Serialize};

/// Agent manifest — describes an agent's capabilities
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentManifest {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub input_content_types: Vec<String>,
    #[serde(default)]
    pub output_content_types: Vec<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// A message part with MIME type
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessagePart {
    pub content_type: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// ACP message (multimodal, MIME-based)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcpMessage {
    pub role: String,
    pub parts: Vec<MessagePart>,
}

/// Run execution modes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    Sync,
    Async,
    Stream,
}

/// Run status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunStatus {
    Created,
    InProgress,
    Awaiting,
    Cancelling,
    Cancelled,
    Completed,
    Failed,
}

/// Request to create a run
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunCreateRequest {
    pub agent_name: String,
    pub input: Vec<AcpMessage>,
    #[serde(default = "default_mode")]
    pub mode: RunMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

fn default_mode() -> RunMode {
    RunMode::Sync
}

/// A run (execution unit)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub run_id: String,
    pub agent_name: String,
    pub status: RunStatus,
    #[serde(default)]
    pub input: Vec<AcpMessage>,
    #[serde(default)]
    pub output: Vec<AcpMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RunError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunError {
    pub code: String,
    pub message: String,
}

/// SSE event types for streaming
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AcpEvent {
    #[serde(rename = "run.created")]
    RunCreated { run: Run },
    #[serde(rename = "run.in-progress")]
    RunInProgress { run: Run },
    #[serde(rename = "message.created")]
    MessageCreated { message: AcpMessage },
    #[serde(rename = "message.part")]
    MessagePart { part: MessagePart },
    #[serde(rename = "message.completed")]
    MessageCompleted { message: AcpMessage },
    #[serde(rename = "run.completed")]
    RunCompleted { run: Run },
    #[serde(rename = "run.failed")]
    RunFailed { run: Run },
    #[serde(rename = "run.awaiting")]
    RunAwaiting { run: Run },
    #[serde(rename = "run.cancelled")]
    RunCancelled { run: Run },
    #[serde(rename = "error")]
    Error { error: RunError },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_message() -> AcpMessage {
        AcpMessage {
            role: "user".into(),
            parts: vec![MessagePart {
                content_type: "text/plain".into(),
                content: "hello".into(),
                url: None,
            }],
        }
    }

    fn sample_run() -> Run {
        Run {
            run_id: "run_1".into(),
            agent_name: "helper".into(),
            status: RunStatus::Completed,
            input: vec![sample_message()],
            output: vec![AcpMessage {
                role: "agent/helper".into(),
                parts: vec![MessagePart {
                    content_type: "text/plain".into(),
                    content: "done".into(),
                    url: None,
                }],
            }],
            session_id: Some("session-1".into()),
            error: None,
        }
    }

    #[test]
    fn agent_manifest_round_trip() {
        let manifest = AgentManifest {
            name: "helper".into(),
            description: "Answers questions".into(),
            input_content_types: vec!["text/plain".into()],
            output_content_types: vec!["text/plain".into()],
            metadata: json!({"version": 1}),
        };

        let encoded = serde_json::to_string(&manifest).unwrap();
        let decoded: AgentManifest = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, manifest);
    }

    #[test]
    fn run_create_request_serialization() {
        let request = RunCreateRequest {
            agent_name: "helper".into(),
            input: vec![sample_message()],
            mode: RunMode::Async,
            session_id: Some("session-1".into()),
        };

        let value = serde_json::to_value(&request).unwrap();

        assert_eq!(value["agent_name"], json!("helper"));
        assert_eq!(value["mode"], json!("async"));
        assert_eq!(value["session_id"], json!("session-1"));
        assert_eq!(value["input"][0]["role"], json!("user"));
    }

    #[test]
    fn acp_event_serialization_variants() {
        let run_event = AcpEvent::RunCreated { run: sample_run() };
        let run_value = serde_json::to_value(&run_event).unwrap();
        assert_eq!(run_value["type"], json!("run.created"));

        let error_event = AcpEvent::Error {
            error: RunError {
                code: "bad_request".into(),
                message: "missing input".into(),
            },
        };
        let error_value = serde_json::to_value(&error_event).unwrap();
        assert_eq!(error_value["type"], json!("error"));

        let decoded: AcpEvent = serde_json::from_value(error_value).unwrap();
        assert!(matches!(decoded, AcpEvent::Error { error } if error.code == "bad_request"));
    }

    #[test]
    fn run_status_serde_round_trip() {
        let statuses = [
            RunStatus::Created,
            RunStatus::InProgress,
            RunStatus::Awaiting,
            RunStatus::Cancelling,
            RunStatus::Cancelled,
            RunStatus::Completed,
            RunStatus::Failed,
        ];

        for status in statuses {
            let encoded = serde_json::to_string(&status).unwrap();
            let decoded: RunStatus = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, status);
        }

        assert_eq!(
            serde_json::to_string(&RunStatus::InProgress).unwrap(),
            "\"in-progress\""
        );
    }
}
