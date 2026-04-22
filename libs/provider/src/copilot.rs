use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, bail};
use async_trait::async_trait;
use futures::{StreamExt, channel::mpsc};
use message::{ContentPart, Message, Role};
use reqwest::{
    Client,
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::ModelInfo;
use crate::{
    CompletionRequest, CompletionResponse, Provider, StreamEvent, StreamResult, ToolDefinition,
    UsageStats,
};

const GITHUB_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const TOKEN_EXCHANGE_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const DEFAULT_CHAT_URL: &str = "https://api.githubcopilot.com/chat/completions";

const USER_AGENT: &str = "GitHubCopilotChat/0.26.7";
const EDITOR_VERSION: &str = "vscode/1.99.3";
const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.26.7";
const COPILOT_INTEGRATION_ID: &str = "vscode-chat";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CopilotToken {
    token: String,
    expires_at: i64,
    endpoints: Option<CopilotEndpoints>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CopilotEndpoints {
    api: Option<String>,
}

pub struct CopilotProvider {
    oauth_token: String,
    client: Client,
    cached_token: Arc<RwLock<Option<CopilotToken>>>,
    model: String,
}

impl CopilotProvider {
    pub fn new(oauth_token: String, model: Option<String>) -> Self {
        Self {
            oauth_token,
            client: Client::new(),
            cached_token: Arc::new(RwLock::new(None)),
            model: model.unwrap_or_else(|| "gpt-4.1".to_string()),
        }
    }

    async fn get_copilot_token(&self) -> Result<CopilotToken> {
        let now = now_unix_seconds();

        if let Some(token) = self.cached_token.read().await.clone()
            && token.expires_at > now + 300
        {
            return Ok(token);
        }

        let response = self
            .client
            .get(TOKEN_EXCHANGE_URL)
            .headers(Self::copilot_headers())
            .header(AUTHORIZATION, format!("token {}", self.oauth_token))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!(
                "copilot token exchange failed with status {status}: {body}. Ensure the oauth token was issued for GitHub Copilot OAuth app {GITHUB_CLIENT_ID}"
            );
        }

        let token: CopilotToken = response.json().await?;
        if token.token.trim().is_empty() {
            bail!(
                "copilot token exchange returned an empty session token for GitHub Copilot OAuth app {GITHUB_CLIENT_ID}"
            );
        }

        *self.cached_token.write().await = Some(token.clone());
        Ok(token)
    }

    pub async fn get_session_token(&self) -> Result<String> {
        Ok(self.get_copilot_token().await?.token)
    }

    fn copilot_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", HeaderValue::from_static(USER_AGENT));
        headers.insert("editor-version", HeaderValue::from_static(EDITOR_VERSION));
        headers.insert(
            "editor-plugin-version",
            HeaderValue::from_static(EDITOR_PLUGIN_VERSION),
        );
        headers.insert(
            "copilot-integration-id",
            HeaderValue::from_static(COPILOT_INTEGRATION_ID),
        );
        headers
    }

    fn chat_url(&self, token: &CopilotToken) -> String {
        token
            .endpoints
            .as_ref()
            .and_then(|endpoints| endpoints.api.as_deref())
            .map(|api| format!("{}/chat/completions", api.trim_end_matches('/')))
            .unwrap_or_else(|| DEFAULT_CHAT_URL.to_string())
    }

    fn effective_model(&self, request: &CompletionRequest) -> String {
        if request.model.trim().is_empty() {
            self.model.clone()
        } else {
            request.model.clone()
        }
    }

    fn build_payload(
        &self,
        request: &CompletionRequest,
        stream: bool,
    ) -> OpenAIChatCompletionRequest {
        let mut messages = request
            .system
            .iter()
            .map(|system| OpenAIChatMessage {
                role: "system".to_string(),
                content: OpenAIMessageContent::Parts(vec![OpenAIContentPart::Text {
                    text: system.content.clone(),
                }]),
                tool_calls: None,
                tool_call_id: None,
            })
            .collect::<Vec<_>>();

        messages.extend(
            request
                .messages
                .iter()
                .flat_map(Self::map_message)
                .collect::<Vec<_>>(),
        );

        OpenAIChatCompletionRequest {
            model: self.effective_model(request),
            messages,
            tools: (!request.tools.is_empty())
                .then(|| request.tools.iter().map(Self::map_tool).collect::<Vec<_>>()),
            temperature: request.temperature,
            max_tokens: request.max_tokens,
            stream,
            stream_options: stream.then_some(OpenAIStreamOptions {
                include_usage: true,
            }),
        }
    }

    fn map_tool(tool: &ToolDefinition) -> OpenAITool {
        OpenAITool {
            kind: "function".to_string(),
            function: OpenAIFunctionDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.input_schema.clone(),
            },
        }
    }

    fn map_message(message: &Message) -> Vec<OpenAIChatMessage> {
        let role = match &message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        };

        let mut content = Vec::new();
        let mut tool_calls = Vec::new();
        let mut tool_messages = Vec::new();

        for part in &message.parts {
            match part {
                ContentPart::Text { text } | ContentPart::Thinking { text } => {
                    content.push(OpenAIContentPart::Text { text: text.clone() });
                }
                ContentPart::Image { media_type, data } => {
                    content.push(OpenAIContentPart::ImageUrl {
                        image_url: OpenAIImageUrl {
                            url: format!("data:{media_type};base64,{data}"),
                        },
                    });
                }
                ContentPart::ToolUse { id, name, input } => {
                    tool_calls.push(OpenAIToolCall {
                        id: id.clone(),
                        kind: "function".to_string(),
                        function: OpenAIFunctionCall {
                            name: name.clone(),
                            arguments: input.to_string(),
                        },
                    });
                }
                ContentPart::ToolResult {
                    id,
                    content: result_content,
                    ..
                } => {
                    tool_messages.push(OpenAIChatMessage {
                        role: "tool".to_string(),
                        content: OpenAIMessageContent::Plain(result_content.clone()),
                        tool_calls: None,
                        tool_call_id: Some(id.clone()),
                    });
                }
            }
        }

        let mut messages = Vec::new();

        if !content.is_empty() || !tool_calls.is_empty() {
            messages.push(OpenAIChatMessage {
                role: role.to_string(),
                content: OpenAIMessageContent::Parts(content),
                tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
                tool_call_id: None,
            });
        }

        messages.extend(tool_messages);
        messages
    }

    fn chat_request_builder(&self, token: &CopilotToken) -> reqwest::RequestBuilder {
        self.client
            .post(self.chat_url(token))
            .headers(Self::copilot_headers())
            .bearer_auth(&token.token)
            .header(CONTENT_TYPE, "application/json")
    }
}

#[async_trait]
impl Provider for CopilotProvider {
    fn name(&self) -> &str {
        "copilot"
    }

    fn default_model(&self) -> String {
        self.model.clone()
    }

    fn model_for_tier(&self, tier: crate::ModelCostTier) -> String {
        match tier {
            crate::ModelCostTier::Low => "gpt-4o-mini".to_string(),
            crate::ModelCostTier::Medium => "gpt-4.1".to_string(),
            crate::ModelCostTier::High => "gpt-5.4".to_string(),
        }
    }

    async fn stream(&self, request: CompletionRequest) -> Result<StreamResult> {
        let token = self.get_copilot_token().await?;
        let response = self
            .chat_request_builder(&token)
            .json(&self.build_payload(&request, true))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("copilot streaming request failed with status {status}: {body}");
        }

        let mut bytes_stream = response.bytes_stream();
        let (sender, receiver) = mpsc::unbounded::<Result<StreamEvent>>();

        tokio::spawn(async move {
            let mut sender = sender;
            let mut parser = OpenAISseParser::default();

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
        let token = self.get_copilot_token().await?;
        let response = self
            .chat_request_builder(&token)
            .json(&self.build_payload(&request, false))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("copilot completion request failed with status {status}: {body}");
        }

        let response: OpenAIChatCompletionResponse = response.json().await?;
        let usage = response.usage.unwrap_or_default();
        let choice =
            response.choices.into_iter().next().ok_or_else(|| {
                anyhow::anyhow!("copilot completion response contained no choices")
            })?;

        let mut parts = Vec::new();
        if let Some(content) = choice.message.content.filter(|content| !content.is_empty()) {
            parts.push(ContentPart::Text { text: content });
        }

        for tool_call in choice.message.tool_calls.unwrap_or_default() {
            parts.push(ContentPart::ToolUse {
                id: tool_call.id,
                name: tool_call.function.name,
                input: parse_tool_arguments(tool_call.function.arguments),
            });
        }

        let created_at = now_unix_millis();
        Ok(CompletionResponse {
            message: Message {
                id: format!("copilot-{created_at}"),
                role: Role::Assistant,
                parts,
                created_at,
            },
            usage: usage.into(),
        })
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let token = self.get_copilot_token().await?;
        let url = self
            .chat_url(&token)
            .trim_end_matches("/chat/completions")
            .to_string()
            + "/models";
        let response = self
            .client
            .get(&url)
            .headers(Self::copilot_headers())
            .bearer_auth(&token.token)
            .send()
            .await?;

        if !response.status().is_success() {
            return Ok(vec![]);
        }

        let body: serde_json::Value = response.json().await?;
        let entries = if let Some(data) = body.get("data").and_then(|value| value.as_array()) {
            data.clone()
        } else if let Some(array) = body.as_array() {
            array.clone()
        } else {
            return Ok(vec![]);
        };

        let mut models: Vec<ModelInfo> = entries
            .into_iter()
            .filter_map(|entry| {
                let id = entry.get("id")?.as_str()?.to_string();
                Some(ModelInfo {
                    name: entry
                        .get("name")
                        .and_then(|value| value.as_str())
                        .map(ToString::to_string)
                        .or_else(|| Some(id.clone())),
                    id,
                    provider: self.name().to_string(),
                })
            })
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }
}

#[derive(Default)]
struct OpenAISseParser {
    buffer: String,
    active_tool_calls: HashMap<usize, String>,
    saw_done: bool,
}

impl OpenAISseParser {
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

        if !self.saw_done {
            self.emit_tool_call_ends(sender);
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
        let line = line.trim();
        if line.is_empty() || !line.starts_with("data: ") {
            return Ok(());
        }

        let data = &line[6..];
        if data == "[DONE]" {
            self.emit_tool_call_ends(sender);
            let _ = sender.unbounded_send(Ok(StreamEvent::Done));
            self.saw_done = true;
            return Ok(());
        }

        let chunk: OpenAIStreamChunk = serde_json::from_str(data)?;
        self.emit_chunk(chunk, sender);
        Ok(())
    }

    fn emit_chunk(
        &mut self,
        chunk: OpenAIStreamChunk,
        sender: &mut mpsc::UnboundedSender<Result<StreamEvent>>,
    ) {
        if let Some(usage) = chunk.usage {
            let _ = sender.unbounded_send(Ok(StreamEvent::Usage(usage.into())));
        }

        for choice in chunk.choices {
            if let Some(content) = choice.delta.content.filter(|content| !content.is_empty()) {
                let _ = sender.unbounded_send(Ok(StreamEvent::TextDelta(content)));
            }

            for tool_call in choice.delta.tool_calls.unwrap_or_default() {
                let known_id = self.active_tool_calls.get(&tool_call.index).cloned();
                let tool_id = tool_call
                    .id
                    .or(known_id)
                    .unwrap_or_else(|| format!("tool-call-{}", tool_call.index));

                if let Some(function) = tool_call.function {
                    if let Some(name) = function.name {
                        self.active_tool_calls
                            .insert(tool_call.index, tool_id.clone());
                        let _ = sender.unbounded_send(Ok(StreamEvent::ToolCallStart {
                            id: tool_id.clone(),
                            name,
                        }));
                    }

                    if let Some(arguments) =
                        function.arguments.filter(|arguments| !arguments.is_empty())
                    {
                        self.active_tool_calls
                            .insert(tool_call.index, tool_id.clone());
                        let _ = sender.unbounded_send(Ok(StreamEvent::ToolCallDelta {
                            id: tool_id,
                            args_chunk: arguments,
                        }));
                    }
                }
            }

            if matches!(choice.finish_reason.as_deref(), Some("tool_calls")) {
                self.emit_tool_call_ends(sender);
            }
        }
    }

    fn emit_tool_call_ends(&mut self, sender: &mut mpsc::UnboundedSender<Result<StreamEvent>>) {
        for id in self.active_tool_calls.drain().map(|(_, id)| id) {
            let _ = sender.unbounded_send(Ok(StreamEvent::ToolCallEnd { id }));
        }
    }
}

fn parse_tool_arguments(arguments: String) -> serde_json::Value {
    match serde_json::from_str(&arguments) {
        Ok(value) => value,
        Err(_) => serde_json::Value::String(arguments),
    }
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn now_unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[derive(Debug, Clone, Serialize)]
struct OpenAIChatCompletionRequest {
    model: String,
    messages: Vec<OpenAIChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAITool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<OpenAIStreamOptions>,
}

#[derive(Debug, Clone, Serialize)]
struct OpenAIStreamOptions {
    include_usage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIChatCompletionResponse {
    choices: Vec<OpenAIChoice>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIChoice {
    message: OpenAIResponseMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAIResponseToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIResponseToolCall {
    id: String,
    function: OpenAIResponseFunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIResponseFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIStreamChunk {
    #[serde(default)]
    choices: Vec<OpenAIStreamChoice>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIStreamChoice {
    #[serde(default)]
    delta: OpenAIStreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OpenAIStreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAIStreamToolCallDelta>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIStreamToolCallDelta {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<OpenAIStreamFunctionDelta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIStreamFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct OpenAIChatMessage {
    role: String,
    #[serde(skip_serializing_if = "OpenAIMessageContent::is_empty")]
    content: OpenAIMessageContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAIToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum OpenAIMessageContent {
    Plain(String),
    Parts(Vec<OpenAIContentPart>),
}

impl OpenAIMessageContent {
    fn is_empty(&self) -> bool {
        match self {
            Self::Plain(s) => s.is_empty(),
            Self::Parts(p) => p.is_empty(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAIContentPart {
    Text { text: String },
    ImageUrl { image_url: OpenAIImageUrl },
}

#[derive(Debug, Clone, Serialize)]
struct OpenAIImageUrl {
    url: String,
}

#[derive(Debug, Clone, Serialize)]
struct OpenAITool {
    #[serde(rename = "type")]
    kind: String,
    function: OpenAIFunctionDefinition,
}

#[derive(Debug, Clone, Serialize)]
struct OpenAIFunctionDefinition {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
struct OpenAIToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: OpenAIFunctionCall,
}

#[derive(Debug, Clone, Serialize)]
struct OpenAIFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OpenAIUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
}

impl From<OpenAIUsage> for UsageStats {
    fn from(value: OpenAIUsage) -> Self {
        Self {
            input_tokens: value.prompt_tokens,
            output_tokens: value.completion_tokens,
            cache_read_tokens: None,
            cache_creation_tokens: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_uses_expected_defaults() {
        let provider = CopilotProvider::new("gho_test".to_string(), None);

        assert_eq!(provider.oauth_token, "gho_test");
        assert_eq!(provider.model, "gpt-4.1");
        assert!(provider.cached_token.blocking_read().is_none());
    }

    #[test]
    fn copilot_headers_include_required_values() {
        let headers = CopilotProvider::copilot_headers();

        assert_eq!(headers.get("user-agent").unwrap(), USER_AGENT);
        assert_eq!(headers.get("editor-version").unwrap(), EDITOR_VERSION);
        assert_eq!(
            headers.get("editor-plugin-version").unwrap(),
            EDITOR_PLUGIN_VERSION
        );
        assert_eq!(
            headers.get("copilot-integration-id").unwrap(),
            COPILOT_INTEGRATION_ID
        );
    }

    #[tokio::test]
    async fn get_session_token_reuses_valid_cached_token() {
        let provider = CopilotProvider::new("gho_test".to_string(), Some("gpt-5".to_string()));
        let cached = CopilotToken {
            token: "session-token".to_string(),
            expires_at: now_unix_seconds() + 600,
            endpoints: Some(CopilotEndpoints {
                api: Some("https://api.githubcopilot.com".to_string()),
            }),
        };

        *provider.cached_token.write().await = Some(cached);

        let token = provider.get_session_token().await.unwrap();

        assert_eq!(token, "session-token");
    }
}
