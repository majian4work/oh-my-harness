use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use async_trait::async_trait;
use futures::{StreamExt, stream};
use message::{ContentPart, Message, Role};

use crate::{
    CompletionRequest, CompletionResponse, ModelInfo, Provider, StreamEvent, StreamResult,
    UsageStats,
};

#[derive(Clone)]
pub struct MockProvider {
    responses: Arc<Mutex<VecDeque<MockResponse>>>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
    response_fn: Option<Arc<dyn Fn(&CompletionRequest) -> MockResponse + Send + Sync>>,
}

impl std::fmt::Debug for MockProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockProvider")
            .field("responses", &self.responses)
            .field("requests", &self.requests)
            .field("response_fn", &self.response_fn.as_ref().map(|_| "<fn>"))
            .finish()
    }
}

#[derive(Debug, Clone)]
pub enum MockResponse {
    Text(String),
    ToolCalls(Vec<MockToolCall>),
    TextThenTools {
        text: String,
        tool_calls: Vec<MockToolCall>,
    },
}

#[derive(Debug, Clone)]
pub struct MockToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

impl MockProvider {
    pub fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::from(responses))),
            requests: Arc::new(Mutex::new(Vec::new())),
            response_fn: None,
        }
    }

    pub fn text(response: &str) -> Self {
        Self::new(vec![MockResponse::Text(response.to_string())])
    }

    pub fn with_response_fn(
        f: impl Fn(&CompletionRequest) -> MockResponse + Send + Sync + 'static,
    ) -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::new())),
            requests: Arc::new(Mutex::new(Vec::new())),
            response_fn: Some(Arc::new(f)),
        }
    }

    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests
            .lock()
            .expect("mock provider requests mutex poisoned")
            .clone()
    }

    fn next_response(&self, request: &CompletionRequest) -> MockResponse {
        if let Some(f) = &self.response_fn {
            return f(request);
        }
        self.responses
            .lock()
            .expect("mock provider responses mutex poisoned")
            .pop_front()
            .unwrap_or_else(|| MockResponse::Text("done".to_string()))
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn default_model(&self) -> String {
        "mock-model".to_string()
    }

    async fn stream(&self, request: CompletionRequest) -> Result<StreamResult> {
        let response = self.next_response(&request);
        self.requests
            .lock()
            .expect("mock provider requests mutex poisoned")
            .push(request);
        let mut events = Vec::new();

        match response {
            MockResponse::Text(text) => {
                push_text_events(&mut events, &text);
            }
            MockResponse::ToolCalls(tool_calls) => {
                push_tool_call_events(&mut events, &tool_calls);
            }
            MockResponse::TextThenTools { text, tool_calls } => {
                push_text_events(&mut events, &text);
                push_tool_call_events(&mut events, &tool_calls);
            }
        }

        events.push(StreamEvent::Usage(UsageStats {
            input_tokens: 1,
            output_tokens: 1,
            cache_read_tokens: None,
            cache_creation_tokens: None,
        }));
        events.push(StreamEvent::Done);

        Ok(Box::pin(stream::iter(
            events.into_iter().map(Ok::<_, anyhow::Error>),
        )))
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let mut stream = self.stream(request).await?;
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        let mut usage = UsageStats::default();

        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::TextDelta(chunk) => text.push_str(&chunk),
                StreamEvent::ToolCallStart { id, name } => {
                    tool_calls.push((id, name, String::new()));
                }
                StreamEvent::ToolCallDelta { id, args_chunk } => {
                    if let Some((_, _, args)) =
                        tool_calls.iter_mut().find(|(tool_id, _, _)| tool_id == &id)
                    {
                        args.push_str(&args_chunk);
                    }
                }
                StreamEvent::ToolCallEnd { .. } => {}
                StreamEvent::Usage(stats) => usage = stats,
                StreamEvent::Done => break,
            }
        }

        let mut parts = Vec::new();
        if !text.is_empty() {
            parts.push(ContentPart::Text { text });
        }

        for (id, name, args) in tool_calls {
            let input = serde_json::from_str(&args).unwrap_or(serde_json::Value::Null);
            parts.push(ContentPart::ToolUse { id, name, input });
        }

        Ok(CompletionResponse {
            message: Message {
                id: format!("mock-{}", now_unix_millis()),
                role: Role::Assistant,
                parts,
                created_at: now_unix_millis(),
            },
            usage,
        })
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(vec![ModelInfo {
            id: "mock-model".to_string(),
            name: Some("Mock Model".to_string()),
            provider: "mock".to_string(),
        }])
    }
}

fn push_text_events(events: &mut Vec<StreamEvent>, text: &str) {
    for chunk in chunk_text(text, 10) {
        events.push(StreamEvent::TextDelta(chunk));
    }
}

fn push_tool_call_events(events: &mut Vec<StreamEvent>, tool_calls: &[MockToolCall]) {
    for tool_call in tool_calls {
        events.push(StreamEvent::ToolCallStart {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
        });

        for chunk in chunk_text(&tool_call.arguments.to_string(), 10) {
            events.push(StreamEvent::ToolCallDelta {
                id: tool_call.id.clone(),
                args_chunk: chunk,
            });
        }

        events.push(StreamEvent::ToolCallEnd {
            id: tool_call.id.clone(),
        });
    }
}

fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return Vec::new();
    }

    chars
        .chunks(max_chars)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn now_unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
