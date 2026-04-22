//! Event bus for cross-cutting agent communication.
//!
//! Uses `tokio::sync::broadcast` for pub/sub event distribution.
//! All frontends (TUI, CLI, Web) subscribe to the same bus.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Events emitted during agent operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub name: String,
    pub status: String,
    pub tools_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    TodoUpdated {
        items: Vec<TodoItem>,
    },
    FileModified {
        session_id: String,
        path: String,
    },
    TurnStarted {
        session_id: String,
    },
    TurnComplete {
        session_id: String,
    },
    StreamDelta {
        session_id: String,
        text: String,
    },
    ToolExecuted {
        session_id: String,
        tool: String,
        args: String,
        result: String,
        is_error: bool,
        duration_ms: u64,
    },
    SubagentSpawned {
        parent_id: String,
        child_id: String,
        agent: String,
    },
    SubagentCompleted {
        child_id: String,
        result: String,
    },
    SubagentFailed {
        child_id: String,
        error: String,
    },
    PermissionRequired {
        session_id: String,
        tool: String,
        input: serde_json::Value,
    },
    PermissionGranted {
        session_id: String,
        tool: String,
    },
    TokenUsage {
        session_id: String,
        input_tokens: u32,
        output_tokens: u32,
    },
    MemoryUpdated {
        scope: String,
        entry_id: String,
    },
    SnapshotCreated {
        session_id: String,
        snapshot_id: String,
    },
    SessionRecovered {
        session_id: String,
    },
    McpServersChanged {
        servers: Vec<McpServerStatus>,
    },
    Error {
        session_id: Option<String>,
        message: String,
    },
}

/// Broadcast-based event bus.
///
/// Clone-friendly — all clones share the same underlying channel.
#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<AgentEvent>,
}

impl EventBus {
    /// Create a new event bus with the given channel capacity.
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Publish an event to all subscribers.
    ///
    /// Returns the number of receivers that received the event.
    /// Returns 0 if there are no active subscribers (this is not an error).
    pub fn publish(&self, event: AgentEvent) -> usize {
        self.sender.send(event).unwrap_or(0)
    }

    /// Subscribe to events.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.sender.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(256)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_and_receive() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        bus.publish(AgentEvent::TurnStarted {
            session_id: "ses_1".into(),
        });

        let event = rx.recv().await.unwrap();
        assert!(matches!(event, AgentEvent::TurnStarted { session_id } if session_id == "ses_1"));
    }

    #[tokio::test]
    async fn multiple_subscribers() {
        let bus = EventBus::new(16);
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        bus.publish(AgentEvent::TurnComplete {
            session_id: "ses_2".into(),
        });

        let e1 = rx1.recv().await.unwrap();
        let e2 = rx2.recv().await.unwrap();
        assert!(matches!(e1, AgentEvent::TurnComplete { .. }));
        assert!(matches!(e2, AgentEvent::TurnComplete { .. }));
    }

    #[tokio::test]
    async fn no_subscribers_is_ok() {
        let bus = EventBus::new(16);
        let count = bus.publish(AgentEvent::Error {
            session_id: None,
            message: "test".into(),
        });
        assert_eq!(count, 0);
    }
}
