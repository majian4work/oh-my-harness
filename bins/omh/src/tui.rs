use std::{future::pending, io, sync::Arc, time::{Duration, Instant}};

use anyhow::Result;
use unicode_width::UnicodeWidthStr;
use chrono::Local;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
};

use provider::ModelInfo;
use runtime::AgentRuntime;
use tokio::sync::broadcast;

use crate::auth::{
    Credentials, OmhConfig, ProviderCredential, configured_provider_names, provider_type_for_name,
};
use crate::slash::{self, SlashResult};

type AppTerminal = Terminal<CrosstermBackend<io::Stdout>>;
const DEFAULT_AGENT: &str = "orchestrator";

struct Suggestion {
    label: String,
    description: String,
}

struct SuggestionState {
    items: Vec<Suggestion>,
    selected: usize,
    trigger: SuggestionTrigger,
}

enum SuggestionTrigger {
    Slash,
    Agent,
    Model,
}

enum AppAction {
    LoadModels { force_refresh: bool },
}

struct ChatMessage {
    role: String,
    content: String,
    started_at: Option<String>,
    duration_ms: Option<u64>,
}

impl ChatMessage {
    fn new(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_string(),
            content: content.into(),
            started_at: None,
            duration_ms: None,
        }
    }
}

use ratatui::style::Modifier;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;

pub struct SubAgentInfo {
    pub name: String,
    pub task_id: String,
    pub status: String,
}

struct App {
    messages: Vec<ChatMessage>,
    input: String,
    cursor_position: usize,
    scroll_offset: u16,
    should_quit: bool,
    auth_popup: Option<AuthPopup>,
    suggestions: Option<SuggestionState>,
    sub_agents: Vec<SubAgentInfo>,
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,
    provider_id: String,
    model_id: String,
    harness: Option<Arc<runtime::Harness>>,
    session_id: Option<String>,
    bus_rx: Option<broadcast::Receiver<bus::AgentEvent>>,
    streaming_text: String,
    is_streaming: bool,
    input_tokens: u32,
    output_tokens: u32,
    log_buffer: omh_trace::TuiLogBuffer,
    log_scroll: u16,
    todos: Vec<bus::TodoItem>,
    modified_files: Vec<String>,
    mcp_servers: Vec<bus::McpServerStatus>,
    turn_start: Option<Instant>,
    title_generated: bool,
    status_scroll: u16,
    last_messages_area: Rect,
    last_log_area: Rect,
    last_status_area: Rect,
}

#[derive(Default)]
struct AuthPopup {
    provider: String,
    key: String,
    focus: AuthField,
    message: String,
}

#[derive(Default)]
enum AuthField {
    #[default]
    Provider,
    Key,
}

impl App {
    fn new(log_buffer: omh_trace::TuiLogBuffer) -> Result<Self> {
        let mut harness = crate::init_harness()?;
        crate::cli::register_providers_from_env(&mut harness)?;
        let (provider_id, model_id) = resolve_active_model(&harness);
        let bus_rx = Some(harness.bus.subscribe());
        let mcp_servers = harness.mcp_statuses.lock().unwrap().clone();
        let harness = Arc::new(harness);

        {
            let harness = Arc::clone(&harness);
            let workspace_root = std::env::current_dir().unwrap_or_default();
            tokio::task::spawn_blocking(move || {
                harness.connect_mcp_servers(&workspace_root);
            });
        }

        let harness = Some(harness);

        let mut app = Self {
            messages: Vec::new(),
            input: String::new(),
            cursor_position: 0,
            scroll_offset: 0,
            should_quit: false,
            auth_popup: None,
            suggestions: None,
            sub_agents: Vec::new(),
            syntax_set: SyntaxSet::load_defaults_newlines(),
            theme_set: ThemeSet::load_defaults(),
            provider_id,
            model_id,
            harness,
            session_id: None,
            bus_rx,
            streaming_text: String::new(),
            is_streaming: false,
            input_tokens: 0,
            output_tokens: 0,
            log_buffer,
            log_scroll: 0,
            todos: Vec::new(),
            modified_files: Vec::new(),
            mcp_servers,
            turn_start: None,
            title_generated: false,
            status_scroll: 0,
            last_messages_area: Rect::default(),
            last_log_area: Rect::default(),
            last_status_area: Rect::default(),
        };
        app.refresh_status();
        Ok(app)
    }

    fn load_most_recent_session(&mut self) {
        let Some(harness) = &self.harness else { return };
        if let Ok(sessions) = harness.session_manager.list(1) {
            if let Some(s) = sessions.first() {
                self.load_session(&s.id);
            } else {
                self.create_new_session();
            }
        }
    }

    fn create_new_session(&mut self) {
        let Some(harness) = &self.harness else { return };
        let workspace_root = match std::env::current_dir() {
            Ok(path) => path,
            Err(error) => {
                self.messages.push(ChatMessage::new("system", format!("Error: {error}")));
                return;
            }
        };
        match harness.session_manager.create(DEFAULT_AGENT, &self.model_id, &workspace_root) {
            Ok(session) => {
                self.session_id = Some(session.id.clone());
                self.title_generated = false;
            }
            Err(error) => {
                self.messages.push(ChatMessage::new("system", format!("Error: {error}")));
            }
        }
    }

    fn load_session(&mut self, session_id: &str) {
        let Some(harness) = &self.harness else { return };
        self.title_generated = true;
        match harness.session_manager.get(session_id) {
            Ok(session) => {
                self.session_id = Some(session.id.clone());
                self.messages.clear();
                for msg in &session.messages {
                    let role = match msg.role {
                        message::Role::User => "user",
                        message::Role::Assistant => "assistant",
                        message::Role::System => "system",
                    };
                    if let Some(text) = msg.parts.iter().find_map(|p| match p {
                        message::ContentPart::Text { text } => Some(text.clone()),
                        _ => None,
                    }) {
                        self.messages.push(ChatMessage::new(role, text));
                    }
                }
            }
            Err(e) => {
                self.messages.push(ChatMessage::new("system", format!("Failed to load session: {e}")));
            }
        }
    }

    fn slash_suggestions() -> Vec<Suggestion> {
        let mut items = vec![
            Suggestion {
                label: "/help".into(),
                description: "Show available commands".into(),
            },
            Suggestion {
                label: "/auth login".into(),
                description: "Add provider credential".into(),
            },
            Suggestion {
                label: "/auth logout".into(),
                description: "Remove provider credential".into(),
            },
            Suggestion {
                label: "/auth list".into(),
                description: "List configured providers".into(),
            },
            Suggestion {
                label: "/auth status".into(),
                description: "Show provider status".into(),
            },
            Suggestion {
                label: "/models".into(),
                description: "List available models".into(),
            },
            Suggestion {
                label: "/model".into(),
                description: "Set active model".into(),
            },
            Suggestion {
                label: "/evolution log".into(),
                description: "Show evolution history".into(),
            },
            Suggestion {
                label: "/evolution consolidate".into(),
                description: "Consolidate learnings".into(),
            },
            Suggestion {
                label: "/skills".into(),
                description: "List available skills".into(),
            },
            Suggestion {
                label: "/skill".into(),
                description: "Show skill content".into(),
            },
        ];

        let workspace_root = std::env::current_dir().unwrap_or_default();
        if let Ok(registry) = skill::SkillRegistry::load(&workspace_root) {
            for s in registry.on_demand() {
                items.push(Suggestion {
                    label: format!("/{}", s.name),
                    description: s.description.clone(),
                });
            }
        }

        items
    }

    fn agent_suggestions() -> Vec<Suggestion> {
        let workspace_root = std::env::current_dir().unwrap_or_default();
        let mut items = Vec::new();
        if let Ok(registry) = agent::AgentRegistry::load(&workspace_root) {
            for agent_def in registry.all() {
                items.push(Suggestion {
                    label: format!("@{}", agent_def.name),
                    description: agent_def.description.clone(),
                });
            }
        }
        items
    }

    fn update_suggestions(&mut self) -> Option<AppAction> {
        let input = &self.input;

        if input.starts_with('/') {
            let trimmed = input.trim();
            if trimmed.eq_ignore_ascii_case("/models") {
                self.suggestions = None;
                return Some(AppAction::LoadModels {
                    force_refresh: false,
                });
            }
            if trimmed.eq_ignore_ascii_case("/models refresh") {
                self.suggestions = None;
                return Some(AppAction::LoadModels {
                    force_refresh: true,
                });
            }

            let filter = input.to_lowercase();
            let filtered: Vec<Suggestion> = Self::slash_suggestions()
                .into_iter()
                .filter(|s| {
                    s.label.to_lowercase().starts_with(&filter)
                        || s.label.to_lowercase().contains(&filter)
                })
                .collect();

            if !filtered.is_empty() {
                self.suggestions = Some(SuggestionState {
                    selected: 0,
                    items: filtered,
                    trigger: SuggestionTrigger::Slash,
                });
                return None;
            }
        }

        if let Some(at_pos) = input.rfind('@') {
            if at_pos == 0 || input.as_bytes().get(at_pos - 1) == Some(&b' ') {
                let partial = &input[at_pos..].to_lowercase();
                let filtered: Vec<Suggestion> = Self::agent_suggestions()
                    .into_iter()
                    .filter(|s| s.label.to_lowercase().starts_with(partial) || partial == "@")
                    .collect();

                if !filtered.is_empty() {
                    self.suggestions = Some(SuggestionState {
                        selected: 0,
                        items: filtered,
                        trigger: SuggestionTrigger::Agent,
                    });
                    return None;
                }
            }
        }

        self.suggestions = None;
        None
    }

    fn render_suggestions(&self, frame: &mut Frame, input_area: Rect) {
        let state = self.suggestions.as_ref().unwrap();
        let max_visible = 8.min(state.items.len());
        let popup_height = max_visible as u16 + 2;

        let popup_area = Rect {
            x: input_area.x,
            y: input_area.y.saturating_sub(popup_height),
            width: input_area.width.min(60),
            height: popup_height,
        };

        let scroll_start = if state.selected >= max_visible {
            state.selected - max_visible + 1
        } else {
            0
        };

        let items: Vec<Line> = state
            .items
            .iter()
            .enumerate()
            .skip(scroll_start)
            .take(max_visible)
            .map(|(i, s)| {
                let marker = if i == state.selected { "▸ " } else { "  " };
                let style = if i == state.selected {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Gray)
                };
                Line::from(vec![
                    Span::styled(marker, style),
                    Span::styled(&s.label, style),
                    Span::styled(
                        format!("  {}", s.description),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])
            })
            .collect();

        frame.render_widget(Clear, popup_area);
        frame.render_widget(
            Paragraph::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .style(Style::default().bg(Color::Black)),
            ),
            popup_area,
        );
    }

    fn refresh_status(&mut self) {
        let providers = configured_provider_names().unwrap_or_default();
        if providers.is_empty() && self.messages.is_empty() {
            self.messages.push(ChatMessage::new("system", "No providers configured. Use '/auth login' to set up a provider."
                .to_string()));
        }
    }

    fn save_popup_credentials(&mut self) {
        let Some(popup) = self.auth_popup.as_mut() else {
            return;
        };

        if popup.provider.trim().is_empty() || popup.key.trim().is_empty() {
            popup.message = "provider and key are required".to_string();
            return;
        }

        let provider = popup.provider.trim().to_string();
        if provider == "copilot" {
            self.messages.push(ChatMessage::new("system", "GitHub Copilot requires browser authentication. Run `omh auth login copilot` from your terminal.".to_string()));
            self.auth_popup = None;
            return;
        }
        let api_key = popup.key.trim().to_string();
        let provider_type = provider_type_for_name(&provider);
        let mut creds = match Credentials::load() {
            Ok(creds) => creds,
            Err(error) => {
                popup.message = error.to_string();
                return;
            }
        };

        creds.add(
            provider.clone(),
            ProviderCredential {
                provider_type,
                api_key,
                base_url: None,
                model: None,
            },
        );

        match creds.save() {
            Ok(()) => {
                self.messages.push(ChatMessage::new("system", format!(
                    "Saved provider '{provider}' to {}",
                    Credentials::global_path().display()
                )));
                self.auth_popup = None;
                self.refresh_status();
            }
            Err(error) => popup.message = error.to_string(),
        }
    }

    fn set_active_model(&mut self, provider_id: &str, model_id: &str) -> Result<()> {
        let mut config = OmhConfig::load().unwrap_or_default();
        config.active_model = Some(crate::auth::ActiveModel {
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
        });
        config.save()?;
        self.provider_id = provider_id.to_string();
        self.model_id = model_id.to_string();
        self.refresh_status();
        self.messages.push(ChatMessage::new("system", format!("✓ Active model set to {provider_id}/{model_id}")));
        Ok(())
    }

    fn start_agent_turn(&mut self, text: String) {
        if self.is_streaming {
            self.messages.push(ChatMessage::new("system", "Wait for the current response to finish streaming.".to_string()));
            return;
        }

        let Some(harness) = self.harness.clone() else {
            self.messages.push(ChatMessage::new("system", "Runtime harness is unavailable.".to_string()));
            return;
        };

        if harness.provider_registry.get(&self.provider_id).is_none() {
            self.messages.push(ChatMessage::new("system", format!(
                "Provider '{}' not configured. Run `omh auth login <provider> --key <key>` or set environment variables.",
                self.provider_id
            )));
            return;
        }

        let Some(session_id) = self.session_id.clone() else {
            self.messages.push(ChatMessage::new("system", "No active session.".to_string()));
            return;
        };

        self.input_tokens = 0;
        self.output_tokens = 0;
        self.streaming_text.clear();
        self.is_streaming = true;
        self.turn_start = Some(Instant::now());
        self.messages.push(ChatMessage::new("user", text.clone()));
        let mut assistant_msg = ChatMessage::new("assistant", String::new());
        assistant_msg.started_at = Some(Local::now().format("%H:%M:%S").to_string());
        self.messages.push(assistant_msg);
        self.refresh_status();

        let agent_name = DEFAULT_AGENT.to_string();
        let max_turns = harness
            .agent_registry
            .get(DEFAULT_AGENT)
            .and_then(|agent| agent.max_turns)
            .unwrap_or(30);
        tokio::spawn(async move {
            let runtime = AgentRuntime::new(agent_name, session_id.clone(), max_turns);
            let mut runtime = runtime.with_logger(&harness);
            runtime.shared_harness = Some(harness.clone());
            if let Err(error) = runtime.run_turn(&harness, &text).await {
                harness.bus.publish(bus::AgentEvent::Error {
                    session_id: Some(session_id),
                    message: error.to_string(),
                });
            }
        });
    }

    fn generate_session_title(&self) {
        let first_user_msg = self
            .messages
            .iter()
            .find(|m| m.role == "user")
            .map(|m| m.content.clone());
        let Some(user_text) = first_user_msg else {
            return;
        };
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        let Some(harness) = self.harness.clone() else {
            return;
        };

        tokio::spawn(async move {
            let prompt = format!(
                "Generate a concise title (max 50 chars, no quotes) for a conversation that starts with:\n\n{}",
                &user_text[..user_text.len().min(500)]
            );

            let request = provider::CompletionRequest {
                model: String::new(),
                system: vec![provider::SystemMessage {
                    content: "You are a title generator. Output ONLY the title, nothing else.".into(),
                    cache_control: false,
                }],
                messages: vec![message::Message::user("title-gen", &prompt)],
                tools: vec![],
                max_tokens: Some(30),
                temperature: Some(0.3),
            };

            let resolved = harness.provider_registry.resolve_model(
                Some(&provider::ModelSpec {
                    model_id: String::new(),
                    provider_id: None,
                }),
                Some(provider::ModelCostTier::Low),
            );

            if let Some(resolved) = resolved {
                if let Some(prov) = harness.provider_registry.get(&resolved.provider_id) {
                    let mut req = request;
                    req.model = resolved.model_id;
                    if let Ok(resp) = prov.complete(req).await {
                        let title = resp.message.text().trim().to_string();
                        if !title.is_empty() {
                            let _ = harness.session_manager.update_title(&session_id, &title);
                        }
                    }
                }
            }
        });
    }

    fn update_last_assistant_message(&mut self) {
        if let Some(message) = self
            .messages
            .iter_mut()
            .rev()
            .find(|message| message.role == "assistant")
        {
            message.content = self.streaming_text.clone();
        }
    }

    fn handle_agent_event(&mut self, event: bus::AgentEvent) {
        let matches_session = |event_session_id: &str, current_session_id: &Option<String>| {
            current_session_id
                .as_deref()
                .is_some_and(|session_id| session_id == event_session_id)
        };

        match event {
            bus::AgentEvent::TurnStarted { session_id } => {
                if matches_session(&session_id, &self.session_id) {
                    self.is_streaming = true;
                    self.refresh_status();
                }
            }
            bus::AgentEvent::StreamDelta { session_id, text } => {
                if matches_session(&session_id, &self.session_id) {
                    self.streaming_text.push_str(&text);
                    self.update_last_assistant_message();
                }
            }
            bus::AgentEvent::ToolExecuted {
                session_id, tool, ..
            } => {
                if matches_session(&session_id, &self.session_id) {
                    if !self.streaming_text.is_empty() && !self.streaming_text.ends_with('\n') {
                        self.streaming_text.push('\n');
                    }
                    self.streaming_text
                        .push_str(&format!("[tool: {tool}] ...\n"));
                    self.update_last_assistant_message();
                }
            }
            bus::AgentEvent::TokenUsage {
                session_id,
                input_tokens,
                output_tokens,
            } => {
                if matches_session(&session_id, &self.session_id) {
                    self.input_tokens = input_tokens;
                    self.output_tokens = output_tokens;
                    self.refresh_status();
                }
            }
            bus::AgentEvent::TurnComplete { session_id } => {
                if matches_session(&session_id, &self.session_id) {
                    self.is_streaming = false;
                    if let Some(start) = self.turn_start.take() {
                        let elapsed = start.elapsed().as_millis() as u64;
                        if let Some(msg) = self.messages.iter_mut().rev().find(|m| m.role == "assistant") {
                            msg.duration_ms = Some(elapsed);
                        }
                    }
                    self.update_last_assistant_message();
                    self.refresh_status();

                    if !self.title_generated {
                        self.title_generated = true;
                        self.generate_session_title();
                    }
                }
            }
            bus::AgentEvent::Error {
                session_id,
                message,
            } => {
                if session_id.as_deref().is_none_or(|event_session_id| {
                    matches_session(event_session_id, &self.session_id)
                }) {
                    self.is_streaming = false;
                    if self.streaming_text.is_empty() {
                        self.streaming_text = format!("Error: {message}");
                    } else {
                        if !self.streaming_text.ends_with('\n') {
                            self.streaming_text.push('\n');
                        }
                        self.streaming_text.push_str(&format!("Error: {message}"));
                    }
                    self.update_last_assistant_message();
                    self.refresh_status();
                }
            }
            bus::AgentEvent::SubagentSpawned {
                child_id, agent, ..
            } => {
                self.sub_agents.push(crate::tui::SubAgentInfo {
                    name: agent,
                    task_id: child_id,
                    status: "running".to_string(),
                });
            }
            bus::AgentEvent::SubagentCompleted { child_id, .. }
            | bus::AgentEvent::SubagentFailed { child_id, .. } => {
                self.sub_agents.retain(|s| s.task_id != child_id);
            }
            bus::AgentEvent::TodoUpdated { items } => {
                self.todos = items;
            }
            bus::AgentEvent::FileModified { session_id, path } => {
                if matches_session(&session_id, &self.session_id) {
                    if !self.modified_files.contains(&path) {
                        self.modified_files.push(path);
                    }
                }
            }
            bus::AgentEvent::McpServersChanged { servers } => {
                self.mcp_servers = servers;
            }
            _ => {}
        }
    }

    fn show_model_picker(&mut self, grouped_models: Vec<(String, Vec<ModelInfo>)>) {
        if grouped_models.is_empty() {
            self.messages.push(ChatMessage::new("system", "No models available from configured providers.".to_string()));
            self.suggestions = None;
            return;
        }

        let mut items = Vec::new();
        for (provider_id, models) in grouped_models {
            for model in models {
                let label = format!("{provider_id}/{}", model.id);
                if label.starts_with("copilot/account") {
                    continue;
                }
                let description = model.name.unwrap_or_else(|| model.id.clone());
                items.push(Suggestion { label, description });
            }
        }

        if items.is_empty() {
            self.messages.push(ChatMessage::new("system", "No models available from configured providers.".to_string()));
            self.suggestions = None;
            return;
        }

        self.suggestions = Some(SuggestionState {
            items,
            selected: 0,
            trigger: SuggestionTrigger::Model,
        });
    }

    fn handle_popup_key(&mut self, key: event::KeyEvent) -> bool {
        let Some(popup) = self.auth_popup.as_mut() else {
            return false;
        };

        match key.code {
            KeyCode::Esc => {
                self.auth_popup = None;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                popup.focus = match popup.focus {
                    AuthField::Provider => AuthField::Key,
                    AuthField::Key => AuthField::Provider,
                };
            }
            KeyCode::Enter => match popup.focus {
                AuthField::Provider => popup.focus = AuthField::Key,
                AuthField::Key => self.save_popup_credentials(),
            },
            KeyCode::Backspace => match popup.focus {
                AuthField::Provider => {
                    popup.provider.pop();
                }
                AuthField::Key => {
                    popup.key.pop();
                }
            },
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => match popup.focus
            {
                AuthField::Provider => popup.provider.push(c),
                AuthField::Key => popup.key.push(c),
            },
            _ => {}
        }

        true
    }

    fn cursor_position(&self, input_area: Rect, main_area: Rect) -> Option<(u16, u16)> {
        if let Some(popup) = &self.auth_popup {
            let area = centered_rect(60, 10, main_area);
            let x = area.x + 2;
            let y = match popup.focus {
                AuthField::Provider => area.y + 2,
                AuthField::Key => area.y + 4,
            };
            let offset = match popup.focus {
                AuthField::Provider => popup.provider.len() as u16,
                AuthField::Key => popup.key.len() as u16,
            };
            return Some((x + offset, y));
        }

        Some((
            input_area.x + 1 + 2 + self.input[..self.cursor_position].width() as u16,
            input_area.y + 1,
        ))
    }

    fn render_auth_popup(&self, frame: &mut Frame, area: Rect) {
        let popup_area = centered_rect(60, 10, area);
        let popup = self.auth_popup.as_ref().expect("popup state should exist");
        let key_mask = if popup.key.is_empty() {
            String::new()
        } else {
            "*".repeat(popup.key.len())
        };
        let provider_marker = if matches!(popup.focus, AuthField::Provider) {
            ">"
        } else {
            " "
        };
        let key_marker = if matches!(popup.focus, AuthField::Key) {
            ">"
        } else {
            " "
        };

        let content = vec![
            Line::from("Add provider credential"),
            Line::from(format!("{provider_marker} provider: {}", popup.provider)),
            Line::from(format!("{key_marker} api key: {key_mask}")),
            Line::from("Enter saves on API key field. Tab switches fields. Esc closes."),
            Line::from(popup.message.as_str()),
        ];

        frame.render_widget(Clear, popup_area);
        frame.render_widget(
            Paragraph::new(content)
                .block(Block::default().title(" auth ").borders(Borders::ALL))
                .alignment(Alignment::Left)
                .wrap(Wrap { trim: false }),
            popup_area,
        );
    }

    fn handle_mouse(&mut self, col: u16, row: u16, kind: MouseEventKind) {
        let scroll_amount: u16 = 3;
        let (up, down) = match kind {
            MouseEventKind::ScrollUp => (true, false),
            MouseEventKind::ScrollDown => (false, true),
            _ => return,
        };

        if self.last_messages_area.contains((col, row).into()) {
            if up {
                self.scroll_offset = self.scroll_offset.saturating_add(scroll_amount);
            } else if down {
                self.scroll_offset = self.scroll_offset.saturating_sub(scroll_amount);
            }
        } else if self.last_log_area.contains((col, row).into()) {
            if up {
                self.log_scroll = self.log_scroll.saturating_add(scroll_amount);
            } else if down {
                self.log_scroll = self.log_scroll.saturating_sub(scroll_amount);
            }
        } else if self.last_status_area.contains((col, row).into()) {
            if up {
                self.status_scroll = self.status_scroll.saturating_sub(scroll_amount);
            } else if down {
                self.status_scroll = self.status_scroll.saturating_add(scroll_amount);
            }
        }
    }

    fn handle_key(&mut self, key: event::KeyEvent) -> Option<AppAction> {
        if key.kind != KeyEventKind::Press {
            return None;
        }

        if self.handle_popup_key(key) {
            return None;
        }

        if let Some(ref mut state) = self.suggestions {
            match key.code {
                KeyCode::Up => {
                    state.selected = state.selected.saturating_sub(1);
                    return None;
                }
                KeyCode::Down => {
                    if state.selected + 1 < state.items.len() {
                        state.selected += 1;
                    }
                    return None;
                }
                KeyCode::Tab | KeyCode::Enter => {
                    let selected = state.items[state.selected].label.clone();
                    match state.trigger {
                        SuggestionTrigger::Slash => {
                            self.input = selected.clone();
                            self.cursor_position = self.input.len();
                            if !selected.ends_with(' ') {
                                self.input.push(' ');
                                self.cursor_position += 1;
                            }
                        }
                        SuggestionTrigger::Agent => {
                            if let Some(at_pos) = self.input.rfind('@') {
                                self.input.truncate(at_pos);
                                self.input.push_str(&selected);
                                self.input.push(' ');
                                self.cursor_position = self.input.len();
                            }
                        }
                        SuggestionTrigger::Model => {
                            if let Some((provider_id, model_id)) = selected.split_once('/') {
                                if let Err(error) = self.set_active_model(provider_id, model_id) {
                                    self.messages.push(ChatMessage::new("system", format!("Error: {error}")));
                                }
                            }
                            self.input.clear();
                            self.cursor_position = 0;
                        }
                    }
                    self.suggestions = None;
                    return None;
                }
                KeyCode::Esc => {
                    self.suggestions = None;
                    return None;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => {
                self.should_quit = true;
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.auth_popup = Some(AuthPopup::default());
            }
            KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Enter => {
                if !self.input.is_empty() {
                    let text = self.input.clone();
                    self.input.clear();
                    self.cursor_position = 0;

                    let workspace_root = std::env::current_dir().unwrap_or_default();
                    match slash::dispatch(&text, &workspace_root) {
                        Ok(SlashResult::Response(response)) => {
                            self.messages.push(ChatMessage::new("user", text));
                            self.messages.push(ChatMessage::new("system", response));
                            self.refresh_status();
                        }
                        Ok(SlashResult::AuthPopup) => {
                            self.auth_popup = Some(AuthPopup::default());
                        }
                        Ok(SlashResult::ListModels { force_refresh }) => {
                            return Some(AppAction::LoadModels { force_refresh });
                        }
                        Ok(SlashResult::NotACommand) => {
                            self.start_agent_turn(text);
                        }
                        Err(e) => {
                            self.messages.push(ChatMessage::new("system", format!("Error: {e}")));
                        }
                    }
                    self.scroll_offset = 0;
                }
            }
            KeyCode::Char(c) => {
                self.input.insert(self.cursor_position, c);
                self.cursor_position += c.len_utf8();
                if let Some(action) = self.update_suggestions() {
                    return Some(action);
                }
            }
            KeyCode::Backspace => {
                if self.cursor_position > 0 {
                    let prev = self.input[..self.cursor_position]
                        .char_indices()
                        .next_back()
                        .map(|(idx, _)| idx)
                        .unwrap_or(0);
                    self.input.remove(prev);
                    self.cursor_position = prev;
                    if let Some(action) = self.update_suggestions() {
                        return Some(action);
                    }
                }
            }
            KeyCode::Delete => {
                if self.cursor_position < self.input.len() {
                    self.input.remove(self.cursor_position);
                    if let Some(action) = self.update_suggestions() {
                        return Some(action);
                    }
                }
            }
            KeyCode::Left => {
                if self.cursor_position > 0 {
                    self.cursor_position = self.input[..self.cursor_position]
                        .char_indices()
                        .next_back()
                        .map(|(idx, _)| idx)
                        .unwrap_or(0);
                }
            }
            KeyCode::Right => {
                if self.cursor_position < self.input.len() {
                    self.cursor_position = self.input[self.cursor_position..]
                        .char_indices()
                        .nth(1)
                        .map(|(idx, _)| self.cursor_position + idx)
                        .unwrap_or(self.input.len());
                }
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.log_scroll = self.log_scroll.saturating_add(1);
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.log_scroll = self.log_scroll.saturating_sub(1);
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => {
                self.status_scroll = self.status_scroll.saturating_add(1);
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => {
                self.status_scroll = self.status_scroll.saturating_sub(1);
            }
            KeyCode::Up => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
            }
            KeyCode::Down => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
            }
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
            }
            _ => {}
        }

        None
    }

    fn render_message_content(&self, content: &str) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let mut in_code_block = false;
        let mut highlighter: Option<HighlightLines> = None;
        let theme = &self.theme_set.themes["base16-eighties.dark"];

        for line in content.lines() {
            if line.starts_with("```") {
                in_code_block = !in_code_block;
                if in_code_block {
                    let lang = line.trim_start_matches("```").trim();
                    if let Some(syntax) = self.syntax_set.find_syntax_by_token(lang) {
                        highlighter = Some(HighlightLines::new(syntax, theme));
                    } else {
                        highlighter = None;
                    }
                } else {
                    highlighter = None;
                }
                continue;
            }

            if in_code_block {
                if let Some(ref mut hl) = highlighter {
                    let line_with_nl = format!("{}\n", line);
                    let ranges = hl
                        .highlight_line(&line_with_nl, &self.syntax_set)
                        .unwrap_or_default();
                    let mut spans = Vec::new();
                    spans.push(Span::styled("│ ", Style::default().fg(Color::DarkGray)));
                    for (style, text) in ranges {
                        let text = text.trim_end_matches('\n');
                        if !text.is_empty() {
                            let color = Color::Rgb(
                                style.foreground.r,
                                style.foreground.g,
                                style.foreground.b,
                            );
                            spans.push(Span::styled(text.to_string(), Style::default().fg(color)));
                        }
                    }
                    lines.push(Line::from(spans));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                        Span::styled(line.to_string(), Style::default().fg(Color::Yellow)),
                    ]));
                }
                continue;
            }

            if line.starts_with("# ") {
                lines.push(Line::from(vec![Span::styled(
                    line.to_string(),
                    Style::default()
                        .fg(Color::Indexed(15))
                        .add_modifier(Modifier::BOLD),
                )]));
            } else if line.starts_with("## ") || line.starts_with("### ") {
                lines.push(Line::from(vec![Span::styled(
                    line.to_string(),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )]));
            } else {
                let mut text = line;
                let mut spans = Vec::new();

                if text.starts_with("- ") || text.starts_with("* ") {
                    spans.push(Span::styled("  • ", Style::default().fg(Color::Cyan)));
                    text = &text[2..];
                }

                let chars: Vec<char> = text.chars().collect();
                let mut i = 0;
                let mut plain_text = String::new();

                let push_plain = |spans: &mut Vec<Span<'static>>, plain: &mut String| {
                    if !plain.is_empty() {
                        spans.push(Span::raw(plain.clone()));
                        plain.clear();
                    }
                };

                while i < chars.len() {
                    if chars[i] == '`' {
                        let mut j = i + 1;
                        let mut found = false;
                        while j < chars.len() {
                            if chars[j] == '`' {
                                found = true;
                                break;
                            }
                            j += 1;
                        }
                        if found {
                            push_plain(&mut spans, &mut plain_text);
                            let code: String = chars[i + 1..j].iter().collect();
                            spans.push(Span::styled(
                                code,
                                Style::default().fg(Color::Yellow).bg(Color::DarkGray),
                            ));
                            i = j + 1;
                            continue;
                        }
                    } else if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
                        let mut j = i + 2;
                        let mut found = false;
                        while j + 1 < chars.len() {
                            if chars[j] == '*' && chars[j + 1] == '*' {
                                found = true;
                                break;
                            }
                            j += 1;
                        }
                        if found {
                            push_plain(&mut spans, &mut plain_text);
                            let bold_text: String = chars[i + 2..j].iter().collect();
                            spans.push(Span::styled(
                                bold_text,
                                Style::default().add_modifier(Modifier::BOLD),
                            ));
                            i = j + 2;
                            continue;
                        }
                    }
                    plain_text.push(chars[i]);
                    i += 1;
                }
                push_plain(&mut spans, &mut plain_text);

                if spans.is_empty() {
                    spans.push(Span::raw(""));
                }
                lines.push(Line::from(spans));
            }
        }
        lines
    }

    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();

        let main_chunks =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(30)]).split(area);
        let left_area = main_chunks[0];
        let right_area = main_chunks[1];
        self.last_status_area = right_area;

        let has_logs = !self.log_buffer.is_empty();

        let mut constraints = vec![Constraint::Min(1), Constraint::Length(3)];
        if has_logs {
            constraints.push(Constraint::Length(8));
        }

        let chunks = Layout::vertical(constraints).split(left_area);
        let messages_area = chunks[0];
        let input_area = chunks[1];
        self.last_messages_area = messages_area;

        // Render Messages
        let mut text_lines = Vec::new();
        let msg_width = messages_area.width.saturating_sub(2) as usize;
        let mut is_first = true;
        for msg in &self.messages {
            if !is_first {
                text_lines.push(Line::from(vec![Span::raw("")]));
            }
            is_first = false;

            let is_tool_output = msg.role == "assistant"
                && msg.content.starts_with("[tool:");

            let card_bg = if msg.role == "user" {
                Some(Color::Rgb(25, 50, 25))
            } else if msg.role == "system" {
                Some(Color::Rgb(50, 45, 20))
            } else if is_tool_output {
                Some(Color::Rgb(30, 30, 30))
            } else if msg.role == "assistant" {
                Some(Color::Rgb(20, 25, 45))
            } else {
                None
            };

            let pad_line = |line: Line<'static>, bg: Color, width: usize| -> Line<'static> {
                let visible: usize = line.spans.iter().map(|s| UnicodeWidthStr::width(s.content.as_ref())).sum();
                let mut spans: Vec<Span<'static>> = line.spans.into_iter().map(|s| {
                    let mut style = s.style;
                    style.bg = Some(bg);
                    Span::styled(s.content, style)
                }).collect();
                let remaining = width.saturating_sub(visible);
                if remaining > 0 {
                    spans.push(Span::styled(" ".repeat(remaining), Style::default().bg(bg)));
                }
                Line::from(spans)
            };

            if msg.role == "user" {
                let bg = card_bg.unwrap();
                let header = pad_line(
                    Line::from(vec![Span::styled(
                        " You",
                        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                    )]),
                    bg,
                    msg_width,
                );
                text_lines.push(header);
                let content_lines = self.render_message_content(&msg.content);
                for line in content_lines {
                    let mut spans = vec![Span::raw("  ")];
                    spans.extend(line.spans);
                    text_lines.push(pad_line(Line::from(spans), bg, msg_width));
                }
            } else if msg.role == "assistant" {
                let bg = card_bg.unwrap();
                if !is_tool_output {
                    let mut header_spans: Vec<Span<'static>> = vec![Span::styled(
                        " Assistant",
                        Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
                    )];
                    if let Some(started) = &msg.started_at {
                        let timing = match msg.duration_ms {
                            Some(ms) => format!("  [{started} • {:.1}s]", ms as f64 / 1000.0),
                            None => format!("  [{started} • ...]"),
                        };
                        header_spans.push(Span::styled(
                            timing,
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    let header = pad_line(Line::from(header_spans), bg, msg_width);
                    text_lines.push(header);
                }
                let content_lines = self.render_message_content(&msg.content);
                for line in content_lines {
                    text_lines.push(pad_line(line, bg, msg_width));
                }
            } else if msg.role == "system" {
                let bg = card_bg.unwrap();
                let mut content = msg.content.clone();
                if let Some(stripped) = content.strip_prefix("system: ") {
                    content = stripped.to_string();
                }
                let line = Line::from(vec![
                    Span::raw(" "),
                    Span::styled(
                        content,
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::DIM | Modifier::ITALIC),
                    ),
                ]);
                text_lines.push(pad_line(line, bg, msg_width));
            } else {
                text_lines.push(Line::from(vec![
                    Span::styled(format!("{}: ", msg.role), Style::default().fg(Color::White)),
                    Span::raw(&msg.content),
                ]));
            }

            text_lines.push(Line::from(vec![Span::raw("")]));
        }

        let inner_width = messages_area.width.saturating_sub(2);
        let messages_widget = Paragraph::new(text_lines)
            .block(Block::default().title(" omh ").borders(Borders::ALL))
            .wrap(Wrap { trim: false });
        let wrapped_count = messages_widget.line_count(inner_width);
        let visible_height = messages_area.height.saturating_sub(2) as usize;
        let max_scroll = wrapped_count.saturating_sub(visible_height).min(u16::MAX as usize) as u16;
        if self.scroll_offset > max_scroll {
            self.scroll_offset = max_scroll;
        }

        let current_line = max_scroll.saturating_sub(self.scroll_offset) as usize;
        let title = if wrapped_count > visible_height {
            format!(" omh [{}/{wrapped_count}] ", current_line + visible_height, )
        } else {
            " omh ".to_string()
        };

        let messages_widget = messages_widget
            .block(Block::default().title(title).borders(Borders::ALL))
            .scroll((max_scroll.saturating_sub(self.scroll_offset), 0));

        frame.render_widget(messages_widget, messages_area);

        if wrapped_count > visible_height {
            let mut scrollbar_state = ScrollbarState::new(wrapped_count)
                .position(current_line)
                .viewport_content_length(visible_height);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                messages_area,
                &mut scrollbar_state,
            );
        }

        // Render Input
        let input_display = format!("> {}", self.input);
        let input_widget =
            Paragraph::new(input_display.as_str()).block(Block::default().borders(Borders::ALL));

        frame.render_widget(input_widget, input_area);

        if has_logs {
            let log_area = chunks[2];
            self.last_log_area = log_area;
            let log_entries = self.log_buffer.drain();
            let log_lines: Vec<Line> = log_entries
                .iter()
                .map(|line| {
                    let color = if line.starts_with("ERROR") {
                        Color::Red
                    } else if line.starts_with("WARN") {
                        Color::Yellow
                    } else if line.starts_with("DEBUG") || line.starts_with("TRACE") {
                        Color::DarkGray
                    } else {
                        Color::White
                    };
                    Line::from(Span::styled(line.as_str(), Style::default().fg(color)))
                })
                .collect();

            let total = log_lines.len() as u16;
            let panel_height = log_area.height.saturating_sub(2);
            let max_scroll = total.saturating_sub(panel_height);
            if self.log_scroll > max_scroll {
                self.log_scroll = max_scroll;
            }
            let scroll_pos = max_scroll.saturating_sub(self.log_scroll);

            let log_title = if total > panel_height {
                format!(" logs [{}/{}] ", scroll_pos + panel_height, total)
            } else {
                " logs ".to_string()
            };

            let log_widget = Paragraph::new(log_lines)
                .block(Block::default().title(log_title).borders(Borders::ALL))
                .wrap(Wrap { trim: false })
                .scroll((scroll_pos, 0));

            frame.render_widget(log_widget, log_area);

            if total > panel_height {
                let mut log_scrollbar_state = ScrollbarState::new(total as usize)
                    .position(scroll_pos as usize)
                    .viewport_content_length(panel_height as usize);
                frame.render_stateful_widget(
                    Scrollbar::new(ScrollbarOrientation::VerticalRight),
                    log_area,
                    &mut log_scrollbar_state,
                );
            }
        }

        if self.suggestions.is_some() {
            self.render_suggestions(frame, input_area);
        }

        if self.auth_popup.is_some() {
            self.render_auth_popup(frame, area);
        }

        if let Some((cursor_x, cursor_y)) = self.cursor_position(input_area, messages_area) {
            frame.set_cursor_position((cursor_x, cursor_y));
        }

        let mut right_lines = Vec::new();

        right_lines.push(Line::from(vec![Span::raw(format!(
            " Model: {}/{}",
            self.provider_id, self.model_id
        ))]));
        right_lines.push(Line::from(vec![Span::raw(format!(
            " Session: {}",
            self.session_id.as_deref().unwrap_or("new")
        ))]));
        right_lines.push(Line::from(vec![Span::raw(format!(
            " Tokens: {}/{}",
            self.input_tokens, self.output_tokens
        ))]));

        let state_span = if self.is_streaming {
            Span::styled("streaming", Style::default().fg(Color::Magenta))
        } else {
            Span::raw("idle")
        };
        right_lines.push(Line::from(vec![Span::raw(" State: "), state_span]));
        right_lines.push(Line::raw(""));

        right_lines.push(Line::from(vec![Span::styled(
            "── MCP Servers ──",
            Style::default().fg(Color::DarkGray),
        )]));
        if self.mcp_servers.is_empty() {
            right_lines.push(Line::raw(" (none)"));
        } else {
            for srv in &self.mcp_servers {
                let color = if srv.status == "connected" {
                    Color::Green
                } else {
                    Color::Red
                };
                right_lines.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(&*srv.name, Style::default().fg(color)),
                    Span::raw(format!(" ({}T)", srv.tools_count)),
                ]));
            }
        }
        right_lines.push(Line::raw(""));

        right_lines.push(Line::from(vec![Span::styled(
            "── Sub-agents ──",
            Style::default().fg(Color::DarkGray),
        )]));
        if self.sub_agents.is_empty() {
            right_lines.push(Line::raw(" (none)"));
        } else {
            for agent in &self.sub_agents {
                right_lines.push(Line::raw(format!(" {} ({})", agent.name, agent.status)));
            }
        }
        right_lines.push(Line::raw(""));

        right_lines.push(Line::from(vec![Span::styled(
            "── Todos ──",
            Style::default().fg(Color::DarkGray),
        )]));
        if self.todos.is_empty() {
            right_lines.push(Line::raw(" (none)"));
        } else {
            for todo in &self.todos {
                let (marker, color) = match todo.status.as_str() {
                    "completed" => ("[x]", Color::Green),
                    "in_progress" => ("[>]", Color::Yellow),
                    _ => ("[ ]", Color::White),
                };
                right_lines.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(marker, Style::default().fg(color)),
                    Span::raw(format!(" {}", todo.content)),
                ]));
            }
        }
        right_lines.push(Line::raw(""));

        right_lines.push(Line::from(vec![Span::styled(
            "── Modified Files ──",
            Style::default().fg(Color::DarkGray),
        )]));
        if self.modified_files.is_empty() {
            right_lines.push(Line::raw(" (none)"));
        } else {
            for file in &self.modified_files {
                right_lines.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(file.clone(), Style::default().fg(Color::Cyan)),
                ]));
            }
        }

        let total_right_lines = right_lines.len() as u16;
        let right_height = right_area.height.saturating_sub(2);
        let max_right_scroll = total_right_lines.saturating_sub(right_height);
        let right_scroll = self.status_scroll.min(max_right_scroll);

        let right_widget = Paragraph::new(right_lines)
            .block(Block::default().title(" status ").borders(Borders::ALL))
            .wrap(Wrap { trim: false })
            .scroll((right_scroll, 0));

        frame.render_widget(right_widget, right_area);
    }
}

async fn fetch_models(force_refresh: bool) -> Result<Vec<(String, Vec<ModelInfo>)>> {
    let mut harness = crate::init_harness()?;
    crate::cli::register_providers_from_env(&mut harness)?;

    let provider_ids: Vec<String> = harness
        .provider_registry
        .list()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let id_refs: Vec<&str> = provider_ids.iter().map(|s| s.as_str()).collect();

    if !force_refresh {
        let cache = crate::auth::ModelsCache::load();
        if let Some(cached) = cache.get_all(&id_refs) {
            return Ok(cached);
        }
    }

    let models = harness.provider_registry.list_all_models_validated().await;
    let mut cache = crate::auth::ModelsCache::load();
    cache.update(&models);
    let _ = cache.save();
    Ok(models)
}

fn resolve_active_model(harness: &runtime::Harness) -> (String, String) {
    let agent_spec = harness
        .agent_registry
        .get(DEFAULT_AGENT)
        .and_then(|a| a.model.as_ref())
        .map(|m| provider::ModelSpec {
            model_id: m.model_id.clone(),
            provider_id: m.provider_id.clone(),
        });

    if let Some(resolved) = harness
        .provider_registry
        .resolve_model(agent_spec.as_ref(), None)
    {
        return (resolved.provider_id, resolved.model_id);
    }

    ("openai".to_string(), "gpt-4.1".to_string())
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let [vertical] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [horizontal] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(vertical);
    horizontal
}

pub async fn run_tui(
    log_buffer: omh_trace::TuiLogBuffer,
    continue_last: bool,
    resume_pick: bool,
) -> Result<()> {
    let mut terminal = init_terminal()?;
    let mut app = App::new(log_buffer)?;

    if continue_last {
        app.load_most_recent_session();
    } else if resume_pick {
        let picked = pick_session_interactive(&mut terminal, &app)?;
        if let Some(session_id) = picked {
            app.load_session(&session_id);
        } else {
            app.create_new_session();
        }
    } else {
        app.create_new_session();
    }

    loop {
        terminal.draw(|frame| app.render(frame))?;

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(16)) => {
                while event::poll(Duration::ZERO)? {
                    match event::read()? {
                        Event::Key(key) => {
                            if let Some(action) = app.handle_key(key) {
                                match action {
                                    AppAction::LoadModels { force_refresh } => match fetch_models(force_refresh).await {
                                        Ok(grouped_models) => app.show_model_picker(grouped_models),
                                        Err(error) => {
                                            app.messages.push(ChatMessage::new("system", format!("Failed to list models: {error}")));
                                        }
                                    },
                                }
                            }
                        }
                        Event::Mouse(mouse) => {
                            app.handle_mouse(mouse.column, mouse.row, mouse.kind);
                        }
                        _ => {}
                    }
                }
            }
            event = async {
                if let Some(ref mut rx) = app.bus_rx {
                    match rx.recv().await {
                        Ok(ev) => Some(ev),
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            rx.recv().await.ok()
                        }
                        Err(_) => None,
                    }
                } else {
                    pending::<Option<bus::AgentEvent>>().await
                }
            } => {
                if let Some(agent_event) = event {
                    app.handle_agent_event(agent_event);
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    restore_terminal(&mut terminal)?;
    Ok(())
}

fn pick_session_interactive(
    terminal: &mut AppTerminal,
    app: &App,
) -> Result<Option<String>> {
    let Some(harness) = &app.harness else {
        return Ok(None);
    };
    let sessions = harness.session_manager.list(50)?;
    if sessions.is_empty() {
        return Ok(None);
    }

    let mut selected: usize = 0;
    let mut scroll: usize = 0;

    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            let block = Block::default()
                .title(" Resume Session (↑/↓ select, Enter confirm, Esc cancel) ")
                .borders(Borders::ALL)
                .style(Style::default().fg(Color::Cyan));
            let inner = block.inner(area);
            frame.render_widget(block, area);

            let visible_height = inner.height as usize;
            if selected < scroll {
                scroll = selected;
            } else if selected >= scroll + visible_height {
                scroll = selected - visible_height + 1;
            }

            let lines: Vec<Line> = sessions
                .iter()
                .enumerate()
                .skip(scroll)
                .take(visible_height)
                .map(|(i, s)| {
                    let title = if s.title.is_empty() { "(untitled)" } else { &s.title };
                    let text = format!(
                        " {} │ {} │ {} msgs │ {}",
                        &s.id[..12.min(s.id.len())],
                        s.agent_name,
                        s.message_count,
                        title
                    );
                    if i == selected {
                        Line::from(Span::styled(text, Style::default().fg(Color::Black).bg(Color::Cyan)))
                    } else {
                        Line::from(text)
                    }
                })
                .collect();

            let paragraph = Paragraph::new(lines);
            frame.render_widget(paragraph, inner);
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        if selected > 0 {
                            selected -= 1;
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if selected + 1 < sessions.len() {
                            selected += 1;
                        }
                    }
                    KeyCode::Enter => {
                        return Ok(Some(sessions[selected].id.clone()));
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        return Ok(None);
                    }
                    _ => {}
                }
            }
        }
    }
}

fn init_terminal() -> io::Result<AppTerminal> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, crossterm::event::EnableMouseCapture)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut AppTerminal) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, crossterm::event::DisableMouseCapture)?;
    terminal.show_cursor()
}
