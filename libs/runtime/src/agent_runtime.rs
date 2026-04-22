use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use bus::AgentEvent;
use futures::StreamExt;
use memory::Scope;
use message::{ContentPart, Message, Role};
use provider::{
    CompletionRequest, ModelCostTier, ModelSpec, StreamEvent, SystemMessage, ToolDefinition,
};
use tokio_util::sync::CancellationToken;
use tool::ToolContext;

use crate::harness::Harness;
use crate::session_logger::SessionLogger;
use crate::telemetry::{ErrorCategory, ToolTelemetry, TurnTelemetry, classify_error};

const COMPACTION_THRESHOLD: f64 = 0.80;
const COMPACTION_KEEP_RECENT: usize = 4;
const MAX_RETRIES: u32 = 4;
const RETRY_BASE_MS: u64 = 1000;
const RETRY_MAX_MS: u64 = 60_000;

pub struct AgentRuntime {
    pub agent_name: String,
    pub session_id: String,
    pub max_turns: u32,
    pub current_turn: u32,
    pub model_override: Option<ModelSpec>,
    pub interactive: bool,
    pub shared_harness: Option<std::sync::Arc<Harness>>,
    logger: Option<SessionLogger>,
}

pub struct ToolCallRecord {
    pub name: String,
    pub duration_ms: u64,
}

pub struct TurnResult {
    pub response: String,
    pub tool_calls_made: usize,
    pub tool_calls: Vec<ToolCallRecord>,
    pub tokens_used: Option<u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub completed: bool,
}

struct ToolCallAccumulator {
    id: String,
    name: String,
    args: String,
}

impl AgentRuntime {
    pub fn new(agent_name: String, session_id: String, max_turns: u32) -> Self {
        Self {
            agent_name,
            session_id,
            max_turns,
            current_turn: 0,
            model_override: None,
            interactive: true,
            shared_harness: None,
            logger: None,
        }
    }

    pub fn with_logger(mut self, harness: &Harness) -> Self {
        let log_path = harness.session_manager.log_path(&self.session_id);
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        self.logger = Some(SessionLogger::new(log_path));
        self
    }

    fn log(&self, event: &str, detail: &str) {
        if let Some(logger) = &self.logger {
            logger.log(event, detail);
        }
    }

    async fn maybe_compact_messages(
        &self,
        provider: &dyn provider::Provider,
        model_id: &str,
        system: &[SystemMessage],
        messages: &mut Vec<Message>,
    ) {
        let context_window = provider.context_window(model_id);
        let system_tokens: usize = system
            .iter()
            .map(|s| message::estimate_tokens(&s.content))
            .sum();
        let msg_tokens = message::estimate_messages_tokens(messages);
        let total = system_tokens + msg_tokens;
        let threshold = (context_window as f64 * COMPACTION_THRESHOLD) as usize;

        if total <= threshold || messages.len() <= COMPACTION_KEEP_RECENT {
            return;
        }

        let split_at = messages.len() - COMPACTION_KEEP_RECENT;
        let old_messages = &messages[..split_at];

        let old_text: String = old_messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::User => "User",
                    Role::Assistant => "Assistant",
                    Role::System => "System",
                };
                format!("[{}] {}", role, m.text())
            })
            .filter(|s| s.len() > 7)
            .collect::<Vec<_>>()
            .join("\n");

        if old_text.is_empty() {
            return;
        }

        let prompt = format!(
            "Summarize this conversation history concisely, preserving key decisions, file paths, errors, and context needed to continue the task:\n\n{old_text}"
        );

        let request = CompletionRequest {
            model: model_id.to_string(),
            system: vec![],
            messages: vec![Message::user("compact", &prompt)],
            tools: vec![],
            max_tokens: Some(1024),
            temperature: Some(0.0),
        };

        match provider.complete(request).await {
            Ok(resp) => {
                let summary = resp.message.text();
                tracing::info!(
                    old_messages = split_at,
                    old_tokens = message::estimate_messages_tokens(old_messages),
                    summary_tokens = message::estimate_tokens(&summary),
                    "session compacted"
                );
                self.log("compact", &format!("compacted {} messages", split_at));
                let summary_msg = Message::user(
                    format!("msg-compact-{}", ulid::Ulid::new()),
                    format!("[Conversation summary]\n{summary}"),
                );
                let recent = messages[split_at..].to_vec();
                messages.clear();
                messages.push(summary_msg);
                messages.extend(recent);
            }
            Err(e) => {
                tracing::warn!(error = %e, "compaction failed, continuing with full context");
            }
        }
    }

    async fn rerank_memories(
        &self,
        provider: &dyn provider::Provider,
        model_id: &str,
        query: &str,
        candidates: &[memory::MemoryEntry],
    ) -> Vec<memory::MemoryEntry> {
        let numbered: String = candidates
            .iter()
            .enumerate()
            .map(|(i, e)| format!("{}. [{:?}/{:?}] {}", i, e.scope, e.kind, e.content))
            .collect::<Vec<_>>()
            .join("\n");

        let prompt = format!(
            "Given the user query: \"{query}\"\n\n\
             Rank these memories by relevance (most relevant first). \
             Return ONLY a comma-separated list of numbers (e.g. \"3,0,7,1\"). \
             Return at most 8 items.\n\n{numbered}"
        );

        let request = provider::CompletionRequest {
            model: model_id.to_string(),
            system: vec![],
            messages: vec![message::Message::user("rerank", &prompt)],
            tools: vec![],
            max_tokens: Some(50),
            temperature: Some(0.0),
        };

        let resp = match provider.complete(request).await {
            Ok(r) => r,
            Err(_) => return candidates.iter().take(8).cloned().collect(),
        };

        let text = resp.message.text();
        let indices: Vec<usize> = text
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter_map(|s| s.trim().parse::<usize>().ok())
            .filter(|&i| i < candidates.len())
            .collect();

        if indices.is_empty() {
            return candidates.iter().take(8).cloned().collect();
        }

        let mut seen = HashSet::new();
        indices
            .into_iter()
            .filter(|i| seen.insert(*i))
            .take(8)
            .map(|i| candidates[i].clone())
            .collect()
    }

    pub async fn run_turn(&mut self, harness: &Harness, input: &str) -> Result<TurnResult> {
        let run_started_at = now_millis();
        let run_started = std::time::Instant::now();
        let start_turn = self.current_turn;
        let agent = harness
            .agent_registry
            .get(&self.agent_name)
            .with_context(|| format!("unknown agent: {}", self.agent_name))?;

        let cost_hint = Some(agent_cost_to_tier(agent.cost));

        let override_spec = self.model_override.clone();
        let config_spec = harness
            .agent_overrides
            .get(&self.agent_name)
            .and_then(|ov| {
                ov.model.as_ref().map(|m| ModelSpec {
                    model_id: m.clone(),
                    provider_id: ov.provider.clone(),
                })
            });
        let agent_spec = agent.model.as_ref().map(|m| ModelSpec {
            model_id: m.model_id.clone(),
            provider_id: m.provider_id.clone(),
        });
        let requested_spec = override_spec.or(config_spec).or(agent_spec);

        tracing::trace!(
            agent = %self.agent_name,
            override_model = ?self.model_override.as_ref().map(|s| &s.model_id),
            config_model = ?harness.agent_overrides.get(&self.agent_name).and_then(|ov| ov.model.as_ref()),
            agent_model = ?agent.model.as_ref().map(|m| &m.model_id),
            final_requested = ?requested_spec.as_ref().map(|s| &s.model_id),
            cost_hint = ?cost_hint,
            "model resolution inputs"
        );

        let resolved = harness
            .provider_registry
            .resolve_model(requested_spec.as_ref(), cost_hint)
            .with_context(|| {
                format!(
                    "no provider can serve agent '{}' (requested model: {:?})",
                    agent.name,
                    requested_spec.as_ref().map(|s| &s.model_id),
                )
            })?;

        let provider_id = &resolved.provider_id;
        let provider = harness
            .provider_registry
            .get(provider_id)
            .with_context(|| format!("provider not registered: {provider_id}"))?;
        let model_id = resolved.model_id.clone();

        let session = harness.session_manager.get(&self.session_id)?;
        let dump_dir = harness.session_manager.dump_dir(&self.session_id);

        let memory_scopes = vec![
            Scope::Agent(self.agent_name.clone()),
            Scope::Project(session.workspace_root.to_string_lossy().into_owned()),
            Scope::Global,
        ];
        let recalled_memories = {
            let candidates =
                memory::recall_candidates(harness.memory.as_ref(), &memory_scopes, input, 20)
                    .await?;

            if candidates.len() > 5 {
                let ranked = self
                    .rerank_memories(provider, &model_id, input, &candidates)
                    .await;
                memory::format_memories(&ranked, 512)
            } else {
                memory::format_memories(&candidates, 512)
            }
        };
        let skill_injection = harness.skill_registry.inject_for_context(&[]);
        let available_skills = harness.skill_registry.format_available_skills();

        let mut system = vec![SystemMessage {
            content: agent.system_prompt.clone(),
            cache_control: true,
        }];
        if !skill_injection.is_empty() {
            system.push(SystemMessage {
                content: skill_injection,
                cache_control: false,
            });
        }
        if !available_skills.is_empty() {
            system.push(SystemMessage {
                content: available_skills,
                cache_control: false,
            });
        }
        system.push(SystemMessage {
            content: recalled_memories,
            cache_control: false,
        });
        if !self.interactive {
            system.push(SystemMessage {
                content: "You are running in non-interactive CLI mode. The user cannot reply or provide follow-up. \
                    You must complete the entire task in this single turn — gather all context, make all decisions, \
                    and execute all steps autonomously. Do not ask clarifying questions; make reasonable assumptions \
                    and state them."
                    .to_string(),
                cache_control: false,
            });
        }
        let tools: Vec<ToolDefinition> = harness
            .tool_registry
            .specs_for_permission(&agent.permission_rules.default_level)
            .into_iter()
            .map(|tool| ToolDefinition {
                name: tool.name,
                description: tool.description,
                input_schema: tool.input_schema,
            })
            .collect();

        harness.bus.publish(AgentEvent::TurnStarted {
            session_id: self.session_id.clone(),
        });
        self.log(
            "turn_start",
            &format!(
                "turn={} agent={} model={} provider={}",
                self.current_turn, self.agent_name, model_id, provider_id
            ),
        );

        let user_msg = Message::user(format!("msg-{}", ulid::Ulid::new()), input);
        harness
            .session_manager
            .append_message(&self.session_id, &user_msg)?;

        let mut messages = session.messages.clone();
        messages.push(user_msg.clone());

        let mut total_tool_calls = 0usize;
        let mut tool_call_records: Vec<ToolCallRecord> = Vec::new();
        let mut total_tokens = 0u64;
        let mut total_input_tokens = 0u64;
        let mut total_output_tokens = 0u64;
        let mut response_text = String::new();
        let mut completed = true;

        let run_result: Result<TurnResult> = async {
        loop {
            if self.current_turn >= self.max_turns {
                completed = false;
                break;
            }
            self.current_turn += 1;

            self.maybe_compact_messages(provider, &model_id, &system, &mut messages).await;
            sanitize_messages(&mut messages);

            let request = CompletionRequest {
                model: model_id.clone(),
                system: system.clone(),
                messages: messages.clone(),
                tools: tools.clone(),
                temperature: agent.temperature,
                max_tokens: None,
            };

            dump_turn(&dump_dir, &self.agent_name, self.current_turn, "request", &request);

            let stream_result = provider.stream(request.clone()).await;
            let mut stream = match stream_result {
                Ok(s) => s,
                Err(e) => {
                    let err_msg = e.to_string();
                    if is_retryable_error(&err_msg) {
                        let mut last_err = e;
                        let mut got_stream = None;
                        for attempt in 1..=MAX_RETRIES {
                            let delay = retry_delay(&err_msg, attempt);
                            tracing::warn!(
                                agent = %self.agent_name,
                                model = %model_id,
                                attempt,
                                delay_ms = delay.as_millis() as u64,
                                error = %last_err,
                                "retrying after error"
                            );
                            tokio::time::sleep(delay).await;
                            match provider.stream(request.clone()).await {
                                Ok(s) => { got_stream = Some(s); break; }
                                Err(retry_err) => { last_err = retry_err; }
                            }
                        }
                        match got_stream {
                            Some(s) => s,
                            None => return Err(last_err),
                        }
                    } else if err_msg.contains("not accessible") || err_msg.contains("not found") || err_msg.contains("not supported") {
                        let fallback_model = provider.model_for_tier(cost_hint.unwrap_or(ModelCostTier::Medium));
                        tracing::warn!(
                            agent = %self.agent_name,
                            model = %model_id,
                            fallback = %fallback_model,
                            error = %err_msg,
                            "Model not accessible, falling back"
                        );
                        let mut retry_request = request.clone();
                        retry_request.model = fallback_model;
                        provider.stream(retry_request).await?
                    } else {
                        tracing::warn!(
                            agent = %self.agent_name,
                            model = %model_id,
                            error = %err_msg,
                            "Non-retryable error"
                        );
                        return Err(e);
                    }
                }
            };
            let mut text_buffer = String::new();
            let mut tool_calls: Vec<ToolCallAccumulator> = Vec::new();

            while let Some(event) = stream.next().await {
                match event? {
                    StreamEvent::TextDelta(text) => {
                        text_buffer.push_str(&text);
                        harness.bus.publish(AgentEvent::StreamDelta {
                            session_id: self.session_id.clone(),
                            text,
                        });
                    }
                    StreamEvent::ToolCallStart { id, name } => {
                        tool_calls.push(ToolCallAccumulator {
                            id,
                            name,
                            args: String::new(),
                        });
                    }
                    StreamEvent::ToolCallDelta { id, args_chunk } => {
                        if let Some(tool_call) =
                            tool_calls.iter_mut().find(|tool_call| tool_call.id == id)
                        {
                            tool_call.args.push_str(&args_chunk);
                        }
                    }
                    StreamEvent::ToolCallEnd { .. } => {}
                    StreamEvent::Usage(usage) => {
                        total_tokens += (usage.input_tokens + usage.output_tokens) as u64;
                        total_input_tokens += usage.input_tokens as u64;
                        total_output_tokens += usage.output_tokens as u64;
                        harness.bus.publish(AgentEvent::TokenUsage {
                            session_id: self.session_id.clone(),
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                        });
                    }
                    StreamEvent::Done => break,
                }
            }

            let mut assistant_parts = Vec::new();
            if !text_buffer.is_empty() {
                assistant_parts.push(ContentPart::Text {
                    text: text_buffer.clone(),
                });
            }
            for tool_call in &tool_calls {
                let input =
                    serde_json::from_str(&tool_call.args).unwrap_or(serde_json::Value::Null);
                assistant_parts.push(ContentPart::ToolUse {
                    id: tool_call.id.clone(),
                    name: tool_call.name.clone(),
                    input,
                });
            }

            let assistant_msg = Message {
                id: format!("msg-{}", ulid::Ulid::new()),
                role: Role::Assistant,
                parts: assistant_parts,
                created_at: now_millis(),
            };
            messages.push(assistant_msg.clone());
            dump_turn(&dump_dir, &self.agent_name, self.current_turn, "response", &assistant_msg);
            tracing::info!(
                agent = %self.agent_name,
                turn = self.current_turn,
                parts = assistant_msg.parts.len(),
                text_len = text_buffer.len(),
                tools = tool_calls.len(),
                "persisting assistant message"
            );
            harness
                .session_manager
                .append_message(&self.session_id, &assistant_msg)?;

            if tool_calls.is_empty() {
                response_text = text_buffer;
                break;
            }

            total_tool_calls += tool_calls.len();
            let mut result_parts = Vec::new();
            for tool_call in &tool_calls {
                let input =
                    serde_json::from_str(&tool_call.args).unwrap_or(serde_json::Value::Null);
                let tool_started_at = now_millis();
                let started_at = std::time::Instant::now();
                let ctx = ToolContext {
                    session_id: self.session_id.clone(),
                    message_id: assistant_msg.id.clone(),
                    agent_name: self.agent_name.clone(),
                    workspace_root: session.workspace_root.clone(),
                    session_dir: Some(dump_dir.clone()),
                    abort: CancellationToken::new(),
                    depth: 0,
                };

                let output = if tool_call.name == "spawn_agent" {
                    execute_spawn_agent(
                        &self.session_id,
                        &self.model_override,
                        &self.shared_harness,
                        harness,
                        &input,
                        &session.workspace_root,
                    ).await
                } else {
                    harness
                        .tool_registry
                        .execute(&tool_call.name, input.clone(), &ctx)
                        .await
                };
                let duration_ms = started_at.elapsed().as_millis() as u64;
                tool_call_records.push(ToolCallRecord {
                    name: tool_call.name.clone(),
                    duration_ms,
                });
                tracing::info!(
                    tool = %tool_call.name,
                    args = %tool_call.args,
                    duration_ms,
                    "tool_call completed"
                );
                self.log("tool_call", &format!("{} ({}ms) args={}", tool_call.name, duration_ms, tool_call.args));

                let (content, is_error, error_category, error_text) = match output {
                    Ok(output) => {
                        let category = if output.is_error {
                            Some(classify_error(&output.content))
                        } else {
                            None
                        };
                        let error_text = if output.is_error {
                            Some(output.content.clone())
                        } else {
                            None
                        };
                        (output.content, output.is_error, category, error_text)
                    }
                    Err(error) => {
                        let message = error.to_string();
                        (
                            message.clone(),
                            true,
                            Some(classify_error(&message)),
                            Some(message),
                        )
                    }
                };

                harness.bus.publish(AgentEvent::ToolExecuted {
                    session_id: self.session_id.clone(),
                    tool: tool_call.name.clone(),
                    args: tool_call.args.clone(),
                    result: content.clone(),
                    is_error,
                    duration_ms,
                });

                let tool_telemetry = ToolTelemetry {
                    session_id: self.session_id.clone(),
                    agent_name: self.agent_name.clone(),
                    turn: self.current_turn,
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    started_at: tool_started_at,
                    completed_at: now_millis(),
                    duration_ms,
                    input_bytes: tool_call.args.len(),
                    output_chars: content.chars().count(),
                    success: !is_error,
                    error_category,
                    error: error_text,
                };
                let tool_telemetry_path = harness.session_manager.tool_telemetry_path(&self.session_id);
                if let Err(error) = tool_telemetry.append_jsonl(&tool_telemetry_path) {
                    tracing::warn!(session_id = %self.session_id, tool = %tool_call.name, error = %error, "failed to persist tool telemetry");
                }

                if !is_error && matches!(tool_call.name.as_str(), "write_file" | "edit_file") {
                    if let Some(path) = serde_json::from_str::<serde_json::Value>(&tool_call.args)
                        .ok()
                        .and_then(|v| v.get("filePath").or(v.get("path")).and_then(|p| p.as_str().map(String::from)))
                    {
                        harness.bus.publish(AgentEvent::FileModified {
                            session_id: self.session_id.clone(),
                            path,
                        });
                    }
                }

                if !is_error && tool_call.name == "todowrite" {
                    if let Ok(input_val) = serde_json::from_str::<serde_json::Value>(&tool_call.args) {
                        if let Some(todos) = input_val.get("todos").and_then(|v| v.as_array()) {
                            let items = todos.iter().filter_map(|t| {
                                Some(bus::TodoItem {
                                    content: t.get("content")?.as_str()?.to_string(),
                                    status: t.get("status")?.as_str()?.to_string(),
                                })
                            }).collect();
                            harness.bus.publish(AgentEvent::TodoUpdated { items });
                        }
                    }
                }
                result_parts.push(ContentPart::ToolResult {
                    id: tool_call.id.clone(),
                    content,
                    is_error,
                });
            }

            let tool_result_msg = Message {
                id: format!("msg-{}", ulid::Ulid::new()),
                role: Role::User,
                parts: result_parts,
                created_at: now_millis(),
            };
            messages.push(tool_result_msg.clone());
            dump_turn(&dump_dir, &self.agent_name, self.current_turn, "tool_results", &tool_result_msg);
            harness
                .session_manager
                .append_message(&self.session_id, &tool_result_msg)?;
        }

        harness.bus.publish(AgentEvent::TurnComplete {
            session_id: self.session_id.clone(),
        });
        self.log("turn_complete", &format!("tools={} input_tokens={} output_tokens={}", total_tool_calls, total_input_tokens, total_output_tokens));

        Ok(TurnResult {
            response: response_text,
            tool_calls_made: total_tool_calls,
            tool_calls: tool_call_records,
            tokens_used: Some(total_tokens),
            input_tokens: total_input_tokens,
            output_tokens: total_output_tokens,
            completed,
        })
        }
        .await;

        let run_completed_at = now_millis();
        let telemetry = match &run_result {
            Ok(result) => TurnTelemetry {
                session_id: self.session_id.clone(),
                agent_name: self.agent_name.clone(),
                provider_id: provider_id.to_string(),
                model_id: model_id.clone(),
                started_at: run_started_at,
                completed_at: run_completed_at,
                elapsed_ms: run_started.elapsed().as_millis() as u64,
                loop_turns: self.current_turn.saturating_sub(start_turn),
                tool_calls: result.tool_calls_made,
                input_tokens: result.input_tokens,
                output_tokens: result.output_tokens,
                completed: result.completed,
                response_chars: result.response.chars().count(),
                error_category: if result.completed {
                    None
                } else {
                    Some(ErrorCategory::MaxTurnsReached)
                },
                error: None,
            },
            Err(error) => TurnTelemetry {
                session_id: self.session_id.clone(),
                agent_name: self.agent_name.clone(),
                provider_id: provider_id.to_string(),
                model_id: model_id.clone(),
                started_at: run_started_at,
                completed_at: run_completed_at,
                elapsed_ms: run_started.elapsed().as_millis() as u64,
                loop_turns: self.current_turn.saturating_sub(start_turn),
                tool_calls: total_tool_calls,
                input_tokens: total_input_tokens,
                output_tokens: total_output_tokens,
                completed: false,
                response_chars: 0,
                error_category: Some(classify_error(&error.to_string())),
                error: Some(error.to_string()),
            },
        };

        let telemetry_path = harness.session_manager.telemetry_path(&self.session_id);
        if let Err(error) = telemetry.append_jsonl(&telemetry_path) {
            tracing::warn!(session_id = %self.session_id, error = %error, "failed to persist telemetry");
        }

        run_result
    }
}

fn execute_spawn_agent<'a>(
    session_id: &'a str,
    model_override: &'a Option<ModelSpec>,
    shared_harness: &'a Option<std::sync::Arc<Harness>>,
    harness: &'a Harness,
    input: &'a serde_json::Value,
    workspace_root: &'a std::path::Path,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = anyhow::Result<tool::ToolOutput>> + Send + 'a>,
> {
    Box::pin(async move {
        let agent_name = input
            .get("agent_name")
            .and_then(|v| v.as_str())
            .unwrap_or("worker")
            .to_string();
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let background = input
            .get("background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if harness.agent_registry.get(&agent_name).is_none() {
            return Ok(tool::ToolOutput::error(format!(
                "unknown agent: {agent_name}"
            )));
        }

        let parent_session = session_id.to_string();
        let model_ov = model_override.clone();

        let workspace_root = workspace_root.to_path_buf();
        let child_session = harness
            .session_manager
            .create_subagent_session(&parent_session, &agent_name, "", &workspace_root)
            .map(|s| s.id)
            .unwrap_or_else(|_| format!("{}:{}", parent_session, agent_name));

        if background {
            let harness_arc = match shared_harness {
                Some(arc) => arc.clone(),
                None => {
                    return Ok(tool::ToolOutput::error(
                        "Background agent spawning requires shared_harness to be set",
                    ));
                }
            };

            let agent_name_bg = agent_name.clone();
            let session_id_bg = child_session.clone();

            let task_id = harness.background_tasks.spawn(
                agent_name_bg.clone(),
                session_id_bg.clone(),
                run_background_agent(
                    harness_arc,
                    agent_name_bg,
                    session_id_bg,
                    prompt.clone(),
                    model_ov,
                ),
            )?;

            let mut output = tool::ToolOutput::text(format!(
                "Agent '{agent_name}' spawned in background.\nTask ID: {task_id}\nSession: {child_session}"
            ));
            output
                .metadata
                .insert("task_id".to_string(), serde_json::json!(task_id));
            output
                .metadata
                .insert("session_id".to_string(), serde_json::json!(child_session));
            output
                .metadata
                .insert("background".to_string(), serde_json::json!(true));
            Ok(output)
        } else {
            harness.bus.publish(bus::AgentEvent::SubagentSpawned {
                parent_id: parent_session.clone(),
                child_id: child_session.clone(),
                agent: agent_name.clone(),
            });

            let mut runtime = AgentRuntime::new(agent_name.clone(), child_session.clone(), 10)
                .with_logger(harness);
            runtime.model_override = model_ov;
            runtime.interactive = false;

            let result = Box::pin(runtime.run_turn(harness, &prompt)).await;

            match result {
                Ok(result) => {
                    harness.bus.publish(bus::AgentEvent::SubagentCompleted {
                        child_id: child_session.clone(),
                        result: result.response.clone(),
                    });
                    let mut output = tool::ToolOutput::text(&result.response);
                    output
                        .metadata
                        .insert("session_id".to_string(), serde_json::json!(child_session));
                    output.metadata.insert(
                        "tool_calls".to_string(),
                        serde_json::json!(result.tool_calls_made),
                    );
                    Ok(output)
                }
                Err(e) => {
                    harness.bus.publish(bus::AgentEvent::SubagentFailed {
                        child_id: child_session.clone(),
                        error: e.to_string(),
                    });
                    Ok(tool::ToolOutput::error(format!(
                        "Agent '{agent_name}' failed: {e}"
                    )))
                }
            }
        }
    })
}

fn agent_cost_to_tier(cost: agent::AgentCost) -> ModelCostTier {
    match cost {
        agent::AgentCost::Free | agent::AgentCost::Cheap => ModelCostTier::Low,
        agent::AgentCost::Expensive => ModelCostTier::High,
    }
}

fn is_retryable_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    let patterns = [
        "connection",
        "timeout",
        "timed out",
        "dns",
        "reset by peer",
        "broken pipe",
        "network",
        "econnrefused",
        "econnreset",
        "etimedout",
        "429",
        "rate limit",
        "too many requests",
        "503",
        "502",
        "504",
        "529",
        "service unavailable",
        "overloaded",
    ];
    patterns.iter().any(|p| lower.contains(p))
}

fn is_rate_limit_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("429")
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("529")
}

fn parse_retry_after(err: &str) -> Option<u64> {
    let lower = err.to_lowercase();
    if let Some(pos) = lower.find("retry-after") {
        let after = &err[pos + 12..];
        let num: String = after
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        num.parse::<u64>().ok()
    } else {
        None
    }
}

fn retry_delay(err: &str, attempt: u32) -> std::time::Duration {
    if let Some(retry_after_secs) = parse_retry_after(err) {
        return std::time::Duration::from_secs(retry_after_secs.min(RETRY_MAX_MS / 1000));
    }

    let base = if is_rate_limit_error(err) {
        RETRY_BASE_MS * 2
    } else {
        RETRY_BASE_MS
    };
    let exp_ms = base * 2u64.pow(attempt.saturating_sub(1));
    let capped = exp_ms.min(RETRY_MAX_MS);
    let jitter = capped / 4;
    let jittered = capped + (rand_u64() % (jitter + 1));
    std::time::Duration::from_millis(jittered.min(RETRY_MAX_MS))
}

fn rand_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

async fn run_background_agent(
    harness: std::sync::Arc<Harness>,
    agent_name: String,
    session_id: String,
    prompt: String,
    model_override: Option<ModelSpec>,
) -> anyhow::Result<String> {
    let mut runtime = AgentRuntime::new(agent_name.clone(), session_id, 10);
    runtime.model_override = model_override;
    runtime.interactive = false;
    runtime.logger = {
        let log_path = harness.session_manager.log_path(&runtime.session_id);
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Some(SessionLogger::new(log_path))
    };

    match Box::pin(runtime.run_turn(&harness, &prompt)).await {
        Ok(result) => Ok(result.response),
        Err(e) => Err(anyhow::anyhow!("Agent '{agent_name}' failed: {e}")),
    }
}

/// Remove orphaned tool_result messages whose IDs don't match any tool_use
/// in a preceding assistant message. This prevents 400 errors from the API.
fn sanitize_messages(messages: &mut Vec<Message>) {
    let mut known_tool_ids: HashSet<String> = HashSet::new();
    let mut to_remove = Vec::new();
    for (idx, msg) in messages.iter().enumerate() {
        match msg.role {
            Role::Assistant => {
                for part in &msg.parts {
                    if let ContentPart::ToolUse { id, .. } = part {
                        known_tool_ids.insert(id.clone());
                    }
                }
            }
            Role::User => {
                let all_tool_results = msg.parts.iter().all(|p| matches!(p, ContentPart::ToolResult { .. }));
                if all_tool_results && !msg.parts.is_empty() {
                    let any_known = msg.parts.iter().any(|p| {
                        if let ContentPart::ToolResult { id, .. } = p {
                            known_tool_ids.contains(id)
                        } else {
                            false
                        }
                    });
                    if !any_known {
                        to_remove.push(idx);
                    }
                }
            }
            _ => {}
        }
    }
    if !to_remove.is_empty() {
        tracing::warn!(count = to_remove.len(), "removing orphaned tool_result messages");
        for idx in to_remove.into_iter().rev() {
            messages.remove(idx);
        }
    }
}

fn dump_turn(
    dump_dir: &PathBuf,
    agent_name: &str,
    turn: u32,
    suffix: &str,
    content: &impl serde::Serialize,
) {
    if !tracing::enabled!(tracing::Level::TRACE) {
        return;
    }
    let agent_dir = dump_dir.join(agent_name);
    if let Err(e) = std::fs::create_dir_all(&agent_dir) {
        tracing::warn!(error = %e, "failed to create dump dir");
        return;
    }
    let filename = format!("turn_{turn:03}_{suffix}.json");
    let path = agent_dir.join(&filename);
    match serde_json::to_string_pretty(content) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!(path = %path.display(), error = %e, "failed to write dump");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize dump");
        }
    }
}
