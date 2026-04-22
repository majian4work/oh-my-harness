use serde::{Deserialize, Serialize};

/// Unique message identifier.
pub type MessageId = String;

/// Unique session identifier.
pub type SessionId = String;

/// Message role in conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
}

/// A single part of a message's content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    Image {
        media_type: String,
        data: String,
    },
    Thinking {
        text: String,
    },
}

/// A conversation message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub role: Role,
    pub parts: Vec<ContentPart>,
    pub created_at: i64,
}

impl Message {
    /// Create a new user message with text content.
    pub fn user(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            role: Role::User,
            parts: vec![ContentPart::Text { text: text.into() }],
            created_at: now_millis(),
        }
    }

    /// Create a new assistant message with text content.
    pub fn assistant(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            role: Role::Assistant,
            parts: vec![ContentPart::Text { text: text.into() }],
            created_at: now_millis(),
        }
    }

    /// Extract all text content from this message.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// Extract tool use parts.
    pub fn tool_uses(&self) -> Vec<(&str, &str, &serde_json::Value)> {
        self.parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::ToolUse { id, name, input } => {
                    Some((id.as_str(), name.as_str(), input))
                }
                _ => None,
            })
            .collect()
    }
}

/// Rough token estimate: ~4 chars per token. Not billing-accurate.
pub fn estimate_tokens(text: &str) -> usize {
    (text.len() + 3) / 4
}

pub fn estimate_message_tokens(msg: &Message) -> usize {
    let mut total = 4;
    for part in &msg.parts {
        total += match part {
            ContentPart::Text { text } | ContentPart::Thinking { text } => estimate_tokens(text),
            ContentPart::Image { data, .. } => estimate_tokens(data) / 4,
            ContentPart::ToolUse { name, input, .. } => {
                estimate_tokens(name) + estimate_tokens(&input.to_string())
            }
            ContentPart::ToolResult { content, .. } => estimate_tokens(content),
        };
    }
    total
}

pub fn estimate_messages_tokens(msgs: &[Message]) -> usize {
    msgs.iter().map(estimate_message_tokens).sum()
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
    fn round_trip_text_message() {
        let message = Message {
            id: "m1".into(),
            role: Role::User,
            parts: vec![ContentPart::Text {
                text: "hello".into(),
            }],
            created_at: 123,
        };

        let json = serde_json::to_string(&message).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, message);
    }

    #[test]
    fn round_trip_tool_use_message() {
        let message = Message {
            id: "m2".into(),
            role: Role::Assistant,
            parts: vec![ContentPart::ToolUse {
                id: "t1".into(),
                name: "search".into(),
                input: serde_json::json!({"query": "rust"}),
            }],
            created_at: 456,
        };

        let json = serde_json::to_string(&message).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, message);
    }

    #[test]
    fn user_and_text_helpers_work() {
        let message = Message::user("m3", "hello world");

        assert_eq!(message.id, "m3");
        assert_eq!(message.role, Role::User);
        assert_eq!(message.text(), "hello world");
        assert!(
            matches!(message.parts.as_slice(), [ContentPart::Text { text }] if text == "hello world")
        );
    }
}
