use std::{
    collections::HashMap,
    env,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures::{StreamExt, channel::mpsc};
use message::{ContentPart, Message, Role};
use reqwest::{Client, Request, RequestBuilder};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::{
    CompletionRequest, CompletionResponse, ModelInfo, Provider, StreamEvent, StreamResult,
    ToolDefinition, UsageStats,
};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    pub client: Client,
    pub api_key: String,
    pub default_model: String,
}

impl AnthropicProvider {
    pub fn new(
        client: Client,
        api_key: impl Into<String>,
        default_model: impl Into<String>,
    ) -> Self {
        let provider = Self {
            client,
            api_key: api_key.into(),
            default_model: default_model.into(),
        };

        info!(
            default_model = %provider.default_model,
            "initialized anthropic provider"
        );

        provider
    }

    pub fn from_env(default_model: impl Into<String>) -> Result<Self> {
        let api_key = env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY is not set")?;
        Ok(Self::new(Client::new(), api_key, default_model))
    }

    fn effective_model(&self, request: &CompletionRequest) -> String {
        if request.model.trim().is_empty() {
            self.default_model.clone()
        } else {
            request.model.clone()
        }
    }

    fn build_payload(&self, request: &CompletionRequest, stream: bool) -> AnthropicMessagesRequest {
        AnthropicMessagesRequest {
            model: self.effective_model(request),
            system: (!request.system.is_empty()).then(|| {
                request
                    .system
                    .iter()
                    .map(|system| AnthropicSystemBlock {
                        kind: "text".to_string(),
                        text: system.content.clone(),
                        cache_control: system.cache_control.then(|| AnthropicCacheControl {
                            kind: "ephemeral".to_string(),
                        }),
                    })
                    .collect::<Vec<_>>()
            }),
            messages: request
                .messages
                .iter()
                .map(Self::map_message)
                .collect::<Vec<_>>(),
            tools: (!request.tools.is_empty())
                .then(|| request.tools.iter().map(Self::map_tool).collect::<Vec<_>>()),
            temperature: request.temperature,
            max_tokens: request.max_tokens.unwrap_or(4096),
            stream,
        }
    }

    fn build_http_request(&self, request: &CompletionRequest, stream: bool) -> Result<Request> {
        let payload = self.build_payload(request, stream);

        self.request_builder()
            .json(&payload)
            .build()
            .map_err(Into::into)
    }

    fn request_builder(&self) -> RequestBuilder {
        self.client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
    }

    fn map_tool(tool: &ToolDefinition) -> AnthropicTool {
        AnthropicTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: tool.input_schema.clone(),
        }
    }

    fn map_message(message: &Message) -> AnthropicMessage {
        let role = match &message.role {
            Role::Assistant => "assistant",
            Role::User | Role::System => "user",
        };

        AnthropicMessage {
            role: role.to_string(),
            content: message.parts.iter().map(Self::map_content_part).collect(),
        }
    }

    fn map_content_part(part: &ContentPart) -> AnthropicContentBlock {
        match part {
            ContentPart::Text { text } | ContentPart::Thinking { text } => {
                AnthropicContentBlock::Text {
                    text: text.clone(),
                    cache_control: None,
                }
            }
            ContentPart::Image { media_type, data } => AnthropicContentBlock::Image {
                source: AnthropicImageSource {
                    kind: "base64".to_string(),
                    media_type: media_type.clone(),
                    data: data.clone(),
                },
            },
            ContentPart::ToolUse { id, name, input } => AnthropicContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            },
            ContentPart::ToolResult {
                id,
                content,
                is_error,
            } => AnthropicContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: content.clone(),
                is_error: *is_error,
            },
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn default_model(&self) -> String {
        self.default_model.clone()
    }

    fn model_for_tier(&self, tier: crate::ModelCostTier) -> String {
        match tier {
            crate::ModelCostTier::Low => "claude-haiku-4-0".to_string(),
            crate::ModelCostTier::Medium => "claude-sonnet-4-0".to_string(),
            crate::ModelCostTier::High => "claude-opus-4-0".to_string(),
        }
    }

    async fn stream(&self, request: CompletionRequest) -> Result<StreamResult> {
        let request = self.build_http_request(&request, true)?;
        let response = self.client.execute(request).await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("anthropic streaming request failed with status {status}: {body}");
        }

        let mut bytes_stream = response.bytes_stream();
        let (sender, receiver) = mpsc::unbounded::<Result<StreamEvent>>();

        tokio::spawn(async move {
            let mut sender = sender;
            let mut parser = AnthropicSseParser::default();

            while let Some(chunk) = bytes_stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        if let Err(err) = parser.push_chunk(&bytes, &mut sender) {
                            let _ = sender.unbounded_send(Err(err));
                            return;
                        }
                    }
                    Err(err) => {
                        let _ = sender.unbounded_send(Err(err.into()));
                        return;
                    }
                }
            }

            if let Err(err) = parser.finish(&mut sender) {
                let _ = sender.unbounded_send(Err(err));
            }
        });

        Ok(Box::pin(receiver))
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let request = self.build_http_request(&request, false)?;
        let response = self.client.execute(request).await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("anthropic completion request failed with status {status}: {body}");
        }

        let response: AnthropicMessagesResponse = response.json().await?;
        let created_at = now_unix_millis();
        let parts = response
            .content
            .into_iter()
            .map(|block| match block {
                AnthropicResponseContentBlock::Text { text } => ContentPart::Text { text },
                AnthropicResponseContentBlock::ToolUse { id, name, input } => {
                    ContentPart::ToolUse { id, name, input }
                }
            })
            .collect();

        Ok(CompletionResponse {
            message: Message {
                id: response.id,
                role: Role::Assistant,
                parts,
                created_at,
            },
            usage: response.usage.into(),
        })
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let response = self
            .client
            .get("https://api.anthropic.com/v1/models")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .query(&[("limit", "100")])
            .send()
            .await?;

        if !response.status().is_success() {
            return Ok(vec![]);
        }

        #[derive(Deserialize)]
        struct ModelsResponse {
            data: Vec<ModelEntry>,
        }

        #[derive(Deserialize)]
        struct ModelEntry {
            id: String,
            display_name: Option<String>,
        }

        let body: ModelsResponse = response.json().await?;
        let mut models: Vec<ModelInfo> = body
            .data
            .into_iter()
            .map(|model| ModelInfo {
                id: model.id,
                name: model.display_name,
                provider: self.name().to_string(),
            })
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }
}

#[derive(Default)]
struct AnthropicSseParser {
    buffer: String,
    current_event: Option<String>,
    current_data: String,
    active_tool_calls: HashMap<usize, String>,
    saw_done: bool,
}

impl AnthropicSseParser {
    fn push_chunk(
        &mut self,
        bytes: &[u8],
        sender: &mut mpsc::UnboundedSender<Result<StreamEvent>>,
    ) -> Result<()> {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));

        while let Some(newline_index) = self.buffer.find('\n') {
            let line = self.buffer.drain(..=newline_index).collect::<String>();
            self.process_line(line.trim_end_matches(['\r', '\n']), sender)?;
        }

        Ok(())
    }

    fn finish(&mut self, sender: &mut mpsc::UnboundedSender<Result<StreamEvent>>) -> Result<()> {
        if !self.buffer.trim().is_empty() {
            let remaining = std::mem::take(&mut self.buffer);
            self.process_line(remaining.trim_end_matches(['\r', '\n']), sender)?;
        }

        self.flush_event(sender)?;

        if !self.saw_done {
            self.emit_all_tool_call_ends(sender);
            let _ = sender.unbounded_send(Ok(StreamEvent::Done));
            self.saw_done = true;
        }

        Ok(())
    }

    fn process_line(
        &mut self,
        line: &str,
        sender: &mut mpsc::UnboundedSender<Result<StreamEvent>>,
    ) -> Result<()> {
        if line.is_empty() {
            return self.flush_event(sender);
        }

        if let Some(event) = line.strip_prefix("event:") {
            self.current_event = Some(event.trim().to_string());
            return Ok(());
        }

        if let Some(data) = line.strip_prefix("data:") {
            if !self.current_data.is_empty() {
                self.current_data.push('\n');
            }
            self.current_data.push_str(data.trim());
        }

        Ok(())
    }

    fn flush_event(
        &mut self,
        sender: &mut mpsc::UnboundedSender<Result<StreamEvent>>,
    ) -> Result<()> {
        if self.current_data.trim().is_empty() {
            self.current_event = None;
            self.current_data.clear();
            return Ok(());
        }

        let data = std::mem::take(&mut self.current_data);
        self.current_event = None;

        let event: AnthropicSseEvent = serde_json::from_str(&data)?;
        self.emit_event(event, sender);
        Ok(())
    }

    fn emit_event(
        &mut self,
        event: AnthropicSseEvent,
        sender: &mut mpsc::UnboundedSender<Result<StreamEvent>>,
    ) {
        match event.event_type.as_str() {
            "content_block_start" => {
                if let (Some(index), Some(content_block)) = (event.index, event.content_block)
                    && content_block.kind == "tool_use"
                {
                    let id = content_block
                        .id
                        .unwrap_or_else(|| format!("tool-call-{index}"));
                    let name = content_block.name.unwrap_or_else(|| "tool".to_string());
                    self.active_tool_calls.insert(index, id.clone());
                    let _ = sender.unbounded_send(Ok(StreamEvent::ToolCallStart { id, name }));
                }
            }
            "content_block_delta" => {
                if let (Some(index), Some(delta)) = (event.index, event.delta) {
                    match delta.kind.as_str() {
                        "text_delta" => {
                            if let Some(text) = delta.text.filter(|text| !text.is_empty()) {
                                let _ = sender.unbounded_send(Ok(StreamEvent::TextDelta(text)));
                            }
                        }
                        "input_json_delta" => {
                            if let (Some(id), Some(partial_json)) = (
                                self.active_tool_calls.get(&index).cloned(),
                                delta.partial_json.filter(|chunk| !chunk.is_empty()),
                            ) {
                                let _ = sender.unbounded_send(Ok(StreamEvent::ToolCallDelta {
                                    id,
                                    args_chunk: partial_json,
                                }));
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                if let Some(index) = event.index {
                    self.emit_tool_call_end(index, sender);
                }
            }
            "message_delta" => {
                if let Some(usage) = event.usage {
                    let _ = sender.unbounded_send(Ok(StreamEvent::Usage(usage.into())));
                }
            }
            "message_stop" => {
                self.emit_all_tool_call_ends(sender);
                let _ = sender.unbounded_send(Ok(StreamEvent::Done));
                self.saw_done = true;
            }
            _ => {}
        }
    }

    fn emit_tool_call_end(
        &mut self,
        index: usize,
        sender: &mut mpsc::UnboundedSender<Result<StreamEvent>>,
    ) {
        if let Some(id) = self.active_tool_calls.remove(&index) {
            let _ = sender.unbounded_send(Ok(StreamEvent::ToolCallEnd { id }));
        }
    }

    fn emit_all_tool_call_ends(&mut self, sender: &mut mpsc::UnboundedSender<Result<StreamEvent>>) {
        for id in self.active_tool_calls.drain().map(|(_, id)| id) {
            let _ = sender.unbounded_send(Ok(StreamEvent::ToolCallEnd { id }));
        }
    }
}

fn now_unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[derive(Debug, Clone, Serialize)]
struct AnthropicMessagesRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<AnthropicSystemBlock>>,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    max_tokens: u32,
    stream: bool,
}

#[derive(Debug, Clone, Serialize)]
struct AnthropicSystemBlock {
    #[serde(rename = "type")]
    kind: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<AnthropicCacheControl>,
}

#[derive(Debug, Clone, Serialize)]
struct AnthropicCacheControl {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Clone, Serialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<AnthropicContentBlock>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
    Image {
        source: AnthropicImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize)]
struct AnthropicImageSource {
    #[serde(rename = "type")]
    kind: String,
    media_type: String,
    data: String,
}

#[derive(Debug, Clone, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicMessagesResponse {
    id: String,
    model: String,
    role: String,
    content: Vec<AnthropicResponseContentBlock>,
    usage: AnthropicUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicResponseContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicSseEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    content_block: Option<AnthropicContentBlockInfo>,
    #[serde(default)]
    delta: Option<AnthropicDelta>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicContentBlockInfo {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicDelta {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
}

impl From<AnthropicUsage> for UsageStats {
    fn from(value: AnthropicUsage) -> Self {
        Self {
            input_tokens: value.input_tokens,
            output_tokens: value.output_tokens,
            cache_read_tokens: value.cache_read_input_tokens,
            cache_creation_tokens: value.cache_creation_input_tokens,
        }
    }
}
