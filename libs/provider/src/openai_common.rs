//! Shared types, parsers, and helpers for OpenAI-compatible providers
//! (Chat Completions API + Responses API).

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use futures::{StreamExt, channel::mpsc};
use message::{ContentPart, Message, Role};
use serde::{Deserialize, Serialize};

use crate::{
    CompletionRequest, CompletionResponse, StreamEvent, StreamResult, ToolDefinition, UsageStats,
};

// ─── Model detection helpers ───

/// GPT-5+ models (except gpt-5-mini) require the Responses API.
pub fn needs_responses_api(model: &str) -> bool {
    let m = model.to_lowercase();
    if let Some(rest) = m.strip_prefix("gpt-") {
        if let Some(major) = rest.chars().next().and_then(|c| c.to_digit(10)) {
            return major >= 5 && !m.starts_with("gpt-5-mini");
        }
    }
    false
}

/// Newer OpenAI models (gpt-5.x, o3, o4, etc.) require `max_completion_tokens`
/// instead of `max_tokens`.
pub fn needs_max_completion_tokens(model: &str) -> bool {
    let m = model.to_lowercase();
    m.starts_with("gpt-5") || m.starts_with("o3") || m.starts_with("o4") || m.starts_with("o1")
}

// ─── Chat Completions API types ───

#[derive(Debug, Clone, Serialize)]
pub struct OpenAIChatCompletionRequest {
    pub model: String,
    pub messages: Vec<OpenAIChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<OpenAITool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<OpenAIStreamOptions>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenAIStreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIChatCompletionResponse {
    pub choices: Vec<OpenAIChoice>,
    #[serde(default)]
    pub usage: Option<OpenAIUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIChoice {
    pub message: OpenAIResponseMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIResponseMessage {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<OpenAIResponseToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIResponseToolCall {
    pub id: String,
    pub function: OpenAIResponseFunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIResponseFunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIStreamChunk {
    #[serde(default)]
    pub choices: Vec<OpenAIStreamChoice>,
    #[serde(default)]
    pub usage: Option<OpenAIUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIStreamChoice {
    #[serde(default)]
    pub delta: OpenAIStreamDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpenAIStreamDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<OpenAIStreamToolCallDelta>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIStreamToolCallDelta {
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<OpenAIStreamFunctionDelta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIStreamFunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenAIChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "OpenAIMessageContent::is_empty")]
    pub content: OpenAIMessageContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenAIToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum OpenAIMessageContent {
    Plain(String),
    Parts(Vec<OpenAIContentPart>),
}

impl OpenAIMessageContent {
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Plain(s) => s.is_empty(),
            Self::Parts(p) => p.is_empty(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OpenAIContentPart {
    Text { text: String },
    ImageUrl { image_url: OpenAIImageUrl },
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenAIImageUrl {
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenAITool {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: OpenAIFunctionDefinition,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenAIFunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenAIToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: OpenAIFunctionCall,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenAIFunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpenAIUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
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

// ─── Responses API types ───

#[derive(Debug, Clone, Serialize)]
pub struct ResponsesApiRequest {
    pub model: String,
    pub input: Vec<ResponsesInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponsesTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    pub stream: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesInputItem {
    Message {
        role: String,
        content: ResponsesContent,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput { call_id: String, output: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ResponsesContent {
    Text(String),
    Parts(Vec<ResponsesContentPart>),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesContentPart {
    InputText { text: String },
    InputImage { image_url: String },
    OutputText { text: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponsesTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesApiResponse {
    #[allow(dead_code)]
    pub id: String,
    pub output: Vec<ResponsesOutputItem>,
    pub usage: ResponsesUsage,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesOutputItem {
    Message {
        #[allow(dead_code)]
        id: String,
        content: Vec<ResponsesOutputText>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        #[allow(dead_code)]
        id: String,
        call_id: String,
        name: String,
        arguments: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesOutputText {
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesUsage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
}

// ─── Shared builders ───

pub fn map_tool(tool: &ToolDefinition) -> OpenAITool {
    OpenAITool {
        kind: "function".to_string(),
        function: OpenAIFunctionDefinition {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.input_schema.clone(),
        },
    }
}

pub fn map_message(message: &Message) -> Vec<OpenAIChatMessage> {
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

pub fn build_chat_payload(
    model: String,
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

    messages.extend(request.messages.iter().flat_map(map_message));

    let use_max_completion_tokens = needs_max_completion_tokens(&model);
    let (max_tokens, max_completion_tokens) = if use_max_completion_tokens {
        (None, request.max_tokens)
    } else {
        (request.max_tokens, None)
    };

    OpenAIChatCompletionRequest {
        model,
        messages,
        tools: (!request.tools.is_empty()).then(|| request.tools.iter().map(map_tool).collect()),
        temperature: request.temperature,
        max_tokens,
        max_completion_tokens,
        stream,
        stream_options: stream.then_some(OpenAIStreamOptions {
            include_usage: true,
        }),
    }
}

pub fn build_responses_payload(
    model: String,
    request: &CompletionRequest,
    stream: bool,
) -> ResponsesApiRequest {
    let mut input: Vec<ResponsesInputItem> = Vec::new();

    for system in &request.system {
        input.push(ResponsesInputItem::Message {
            role: "developer".to_string(),
            content: ResponsesContent::Text(system.content.clone()),
        });
    }

    for message in &request.messages {
        match &message.role {
            Role::User => {
                let mut parts = Vec::new();
                for part in &message.parts {
                    match part {
                        ContentPart::Text { text } | ContentPart::Thinking { text } => {
                            parts.push(ResponsesContentPart::InputText { text: text.clone() });
                        }
                        ContentPart::Image { media_type, data } => {
                            parts.push(ResponsesContentPart::InputImage {
                                image_url: format!("data:{media_type};base64,{data}"),
                            });
                        }
                        ContentPart::ToolResult {
                            id,
                            content: result_content,
                            ..
                        } => {
                            input.push(ResponsesInputItem::FunctionCallOutput {
                                call_id: id.clone(),
                                output: result_content.clone(),
                            });
                            continue;
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    input.push(ResponsesInputItem::Message {
                        role: "user".to_string(),
                        content: ResponsesContent::Parts(parts),
                    });
                }
            }
            Role::Assistant => {
                for part in &message.parts {
                    match part {
                        ContentPart::Text { text } | ContentPart::Thinking { text } => {
                            input.push(ResponsesInputItem::Message {
                                role: "assistant".to_string(),
                                content: ResponsesContent::Parts(vec![
                                    ResponsesContentPart::OutputText { text: text.clone() },
                                ]),
                            });
                        }
                        ContentPart::ToolUse {
                            id,
                            name,
                            input: args,
                        } => {
                            input.push(ResponsesInputItem::FunctionCall {
                                call_id: id.clone(),
                                name: name.clone(),
                                arguments: args.to_string(),
                            });
                        }
                        _ => {}
                    }
                }
            }
            Role::System => {
                for part in &message.parts {
                    if let ContentPart::Text { text } = part {
                        input.push(ResponsesInputItem::Message {
                            role: "developer".to_string(),
                            content: ResponsesContent::Text(text.clone()),
                        });
                    }
                }
            }
        }
    }

    let tools = if request.tools.is_empty() {
        None
    } else {
        Some(
            request
                .tools
                .iter()
                .map(|tool| ResponsesTool {
                    kind: "function".to_string(),
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    parameters: tool.input_schema.clone(),
                })
                .collect(),
        )
    };

    ResponsesApiRequest {
        model,
        input,
        tools,
        max_output_tokens: request.max_tokens,
        stream,
    }
}

pub fn parse_chat_completion_response(
    response: OpenAIChatCompletionResponse,
    id_prefix: &str,
) -> Result<CompletionResponse> {
    let usage = response.usage.unwrap_or_default();
    let choice =
        response.choices.into_iter().next().ok_or_else(|| {
            anyhow::anyhow!("{id_prefix} completion response contained no choices")
        })?;

    let mut parts = Vec::new();
    if let Some(content) = choice.message.content.filter(|c| !c.is_empty()) {
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
            id: format!("{id_prefix}-{created_at}"),
            role: Role::Assistant,
            parts,
            created_at,
        },
        usage: usage.into(),
    })
}

pub fn parse_responses_api_response(
    resp: ResponsesApiResponse,
    id_prefix: &str,
) -> CompletionResponse {
    let mut parts = Vec::new();
    for item in resp.output {
        match item {
            ResponsesOutputItem::Message { content, .. } => {
                for c in content {
                    if !c.text.is_empty() {
                        parts.push(ContentPart::Text { text: c.text });
                    }
                }
            }
            ResponsesOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                parts.push(ContentPart::ToolUse {
                    id: call_id,
                    name,
                    input: parse_tool_arguments(arguments),
                });
            }
            ResponsesOutputItem::Unknown => {}
        }
    }

    let usage = resp.usage;
    let created_at = now_unix_millis();
    CompletionResponse {
        message: Message {
            id: format!("{id_prefix}-{created_at}"),
            role: Role::Assistant,
            parts,
            created_at,
        },
        usage: UsageStats {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: None,
            cache_creation_tokens: None,
        },
    }
}

// ─── Chat Completions SSE parser ───

#[derive(Default)]
pub struct OpenAISseParser {
    buffer: String,
    active_tool_calls: HashMap<usize, String>,
    saw_done: bool,
}

impl OpenAISseParser {
    pub fn push_chunk(
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

    pub fn finish(
        &mut self,
        sender: &mut mpsc::UnboundedSender<Result<StreamEvent>>,
    ) -> Result<()> {
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
            if let Some(content) = choice.delta.content.filter(|c| !c.is_empty()) {
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

                    if let Some(arguments) = function.arguments.filter(|a| !a.is_empty()) {
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

// ─── Responses API SSE parser ───

#[derive(Default)]
pub struct ResponsesSseParser {
    buffer: String,
    active_tool_calls: HashMap<String, String>,
    saw_done: bool,
}

impl ResponsesSseParser {
    pub fn push_chunk(
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

    pub fn finish(
        &mut self,
        sender: &mut mpsc::UnboundedSender<Result<StreamEvent>>,
    ) -> Result<()> {
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

        let event: serde_json::Value = serde_json::from_str(data)?;
        self.emit_event(&event, sender);
        Ok(())
    }

    fn emit_event(
        &mut self,
        event: &serde_json::Value,
        sender: &mut mpsc::UnboundedSender<Result<StreamEvent>>,
    ) {
        let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match event_type {
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(|v| v.as_str()) {
                    if !delta.is_empty() {
                        let _ =
                            sender.unbounded_send(Ok(StreamEvent::TextDelta(delta.to_string())));
                    }
                }
            }
            "response.output_item.added" => {
                if let Some(item) = event.get("item") {
                    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if item_type == "function_call" {
                        let call_id = item
                            .get("call_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = item
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !call_id.is_empty() && !name.is_empty() {
                            self.active_tool_calls
                                .insert(call_id.clone(), call_id.clone());
                            let _ = sender.unbounded_send(Ok(StreamEvent::ToolCallStart {
                                id: call_id,
                                name,
                            }));
                        }
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = event.get("delta").and_then(|v| v.as_str()) {
                    let call_id = event.get("item_id").and_then(|v| v.as_str()).unwrap_or("");
                    let id = self
                        .active_tool_calls
                        .values()
                        .next()
                        .cloned()
                        .unwrap_or_else(|| call_id.to_string());
                    if !delta.is_empty() {
                        let _ = sender.unbounded_send(Ok(StreamEvent::ToolCallDelta {
                            id,
                            args_chunk: delta.to_string(),
                        }));
                    }
                }
            }
            "response.output_item.done" => {
                if let Some(item) = event.get("item") {
                    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if item_type == "function_call" {
                        let call_id = item
                            .get("call_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !call_id.is_empty() {
                            self.active_tool_calls.remove(&call_id);
                            let _ =
                                sender.unbounded_send(Ok(StreamEvent::ToolCallEnd { id: call_id }));
                        }
                    }
                }
            }
            "response.completed" => {
                if let Some(usage) = event.get("response").and_then(|r| r.get("usage")) {
                    let input_tokens = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    let output_tokens = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    let _ = sender.unbounded_send(Ok(StreamEvent::Usage(UsageStats {
                        input_tokens,
                        output_tokens,
                        cache_read_tokens: None,
                        cache_creation_tokens: None,
                    })));
                }
                self.emit_tool_call_ends(sender);
                let _ = sender.unbounded_send(Ok(StreamEvent::Done));
                self.saw_done = true;
            }
            _ => {}
        }
    }

    fn emit_tool_call_ends(&mut self, sender: &mut mpsc::UnboundedSender<Result<StreamEvent>>) {
        for id in self.active_tool_calls.drain().map(|(_, id)| id) {
            let _ = sender.unbounded_send(Ok(StreamEvent::ToolCallEnd { id }));
        }
    }
}

// ─── Streaming helpers ───

/// Spawn a streaming task using the Chat Completions SSE parser.
pub fn spawn_chat_stream(response: reqwest::Response) -> StreamResult {
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

    Box::pin(receiver)
}

/// Spawn a streaming task using the Responses API SSE parser.
pub fn spawn_responses_stream(response: reqwest::Response) -> StreamResult {
    let mut bytes_stream = response.bytes_stream();
    let (sender, receiver) = mpsc::unbounded::<Result<StreamEvent>>();

    tokio::spawn(async move {
        let mut sender = sender;
        let mut parser = ResponsesSseParser::default();

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

    Box::pin(receiver)
}

// ─── Utilities ───

pub fn parse_tool_arguments(arguments: String) -> serde_json::Value {
    match serde_json::from_str(&arguments) {
        Ok(value) => value,
        Err(_) => serde_json::Value::String(arguments),
    }
}

pub fn now_unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
