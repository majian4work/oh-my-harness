use std::{
    collections::HashMap,
    future::pending,
    io,
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use base64::Engine as _;
use chrono::{Local, TimeZone};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
};
use unicode_width::UnicodeWidthStr;

use provider::{ModelInfo, ModelSpec};
use runtime::AgentRuntime;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::auth::{OmhConfig, configured_provider_names};
use crate::slash::{self, SlashResult};

type AppTerminal = Terminal<CrosstermBackend<io::Stdout>>;
const DEFAULT_AGENT: &str = "orchestrator";
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Compact summary of tool call arguments for display.
fn compact_tool_args(args_json: &str) -> String {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(args_json) else {
        return String::new();
    };
    let obj = match val.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    let parts: Vec<String> = obj
        .iter()
        .map(|(k, v)| {
            let s = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let s = if s.len() > 60 {
                format!("{}…", &s[..60])
            } else {
                s
            };
            format!("{k}={s}")
        })
        .collect();
    parts.join(" ")
}

pub(crate) mod input_ast;
mod input_intent;
mod mention_parser;
mod slash_input;

mod palette {
    use ratatui::style::Color;

    // VS Code Dark 2026 inspired
    pub const BG: Color = Color::Rgb(31, 31, 31); // #1f1f1f  editor bg
    pub const SURFACE: Color = Color::Rgb(37, 37, 38); // #252526  sidebar/panel
    pub const SURFACE_BRIGHT: Color = Color::Rgb(45, 45, 45); // #2d2d2d elevated
    pub const BORDER: Color = Color::Rgb(60, 60, 60); // #3c3c3c
    pub const FG: Color = Color::Rgb(204, 204, 204); // #cccccc
    pub const MUTED: Color = Color::Rgb(110, 118, 129); // #6e7681
    pub const ACCENT: Color = Color::Rgb(79, 193, 255); // #4fc1ff  bright blue
    pub const GREEN: Color = Color::Rgb(137, 209, 133); // #89d185
    pub const YELLOW: Color = Color::Rgb(204, 167, 0); // #cca700
    pub const RED: Color = Color::Rgb(244, 135, 113); // #f48771
    pub const CYAN: Color = Color::Rgb(156, 220, 254); // #9cdcfe
    pub const MAGENTA: Color = Color::Rgb(197, 134, 192); // #c586c0
    pub const ORANGE: Color = Color::Rgb(206, 145, 120); // #ce9178
}

struct Suggestion {
    label: String,
    description: String,
    needs_arg: bool,
}

struct GitFileEntry {
    status: String, // "M", "A", "D", "?", "R", etc.
    path: String,
}

struct GitRepoStatus {
    root: String, // relative path or "." for the workspace root
    files: Vec<GitFileEntry>,
}

struct SuggestionState {
    items: Vec<Suggestion>,
    selected: usize,
    trigger: SuggestionTrigger,
}

#[derive(Clone, Copy)]
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
    todos: Vec<bus::TodoItem>,
    modified_files: Vec<GitRepoStatus>,
    last_git_poll: Option<Instant>,
    mcp_servers: Vec<bus::McpServerStatus>,
    turn_start: Option<Instant>,
    title_generated: bool,
    status_scroll: u16,
    last_messages_area: Rect,
    last_status_area: Rect,
    tick: usize,
    // Mouse text selection (character-level)
    selection_anchor: Option<(usize, usize)>, // (wrapped_row, display_col)
    selection_end: Option<(usize, usize)>,
    msg_plain_lines: Vec<String>, // plain text per wrapped screen row
    msg_scroll: u16,              // cached scroll value used during last render
    msg_max_scroll: u16,
    loading_models: bool,
    pending_approval: Option<bus::ApprovalRequest>,
}

impl App {
    fn new() -> Result<Self> {
        let mut harness = crate::init_harness()?;
        crate::run::register_providers_from_env(&mut harness)?;
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
            todos: Vec::new(),
            modified_files: Vec::new(),
            last_git_poll: None,
            mcp_servers,
            turn_start: None,
            title_generated: false,
            status_scroll: 0,
            last_messages_area: Rect::default(),
            last_status_area: Rect::default(),
            tick: 0,
            selection_anchor: None,
            selection_end: None,
            msg_plain_lines: Vec::new(),
            msg_scroll: 0,
            msg_max_scroll: 0,
            loading_models: false,
            pending_approval: None,
        };
        app.refresh_status();
        Ok(app)
    }

    fn load_most_recent_session(&mut self) {
        let Some(harness) = &self.harness else { return };
        let cwd = std::env::current_dir().ok();
        let sessions = cwd
            .as_deref()
            .map(|ws| harness.session_manager.list_for_workspace(1, ws))
            .unwrap_or_else(|| harness.session_manager.list(1));
        if let Ok(sessions) = sessions {
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
                self.messages
                    .push(ChatMessage::new("system", format!("Error: {error}")));
                return;
            }
        };
        match harness
            .session_manager
            .create(DEFAULT_AGENT, &self.model_id, &workspace_root)
        {
            Ok(session) => {
                self.session_id = Some(session.id.clone());
                self.title_generated = false;
                self.input_tokens = session.input_tokens.min(u32::MAX as u64) as u32;
                self.output_tokens = session.output_tokens.min(u32::MAX as u64) as u32;
            }
            Err(error) => {
                self.messages
                    .push(ChatMessage::new("system", format!("Error: {error}")));
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
                self.input_tokens = session.input_tokens.min(u32::MAX as u64) as u32;
                self.output_tokens = session.output_tokens.min(u32::MAX as u64) as u32;

                // Read tool telemetry for per-tool durations
                let tool_telemetry_path = harness.session_manager.tool_telemetry_path(session_id);
                let tool_telemetry: Vec<runtime::ToolTelemetry> =
                    runtime::read_tool_telemetry_jsonl(&tool_telemetry_path).unwrap_or_default();
                let tool_durations: HashMap<String, u64> = tool_telemetry
                    .iter()
                    .map(|t| (t.tool_call_id.clone(), t.duration_ms))
                    .collect();

                // Merge assistant turns + tool results into one ChatMessage,
                // matching real-time streaming display.
                let mut assistant_buf: Option<String> = None;
                let mut assistant_first_ts: Option<i64> = None;
                let mut assistant_last_ts: Option<i64> = None;
                let mut tool_info: HashMap<String, (String, String)> = HashMap::new(); // id -> (name, args_summary)

                let flush_assistant =
                    |buf: &mut Option<String>,
                     first_ts: &mut Option<i64>,
                     last_ts: &mut Option<i64>,
                     messages: &mut Vec<ChatMessage>| {
                        if let Some(content) = buf.take() {
                            if !content.is_empty() {
                                let mut chat_msg = ChatMessage::new("assistant", content);
                                if let Some(ts) = first_ts.take() {
                                    if let Some(dt) = Local.timestamp_millis_opt(ts).single() {
                                        chat_msg.started_at =
                                            Some(dt.format("%H:%M:%S").to_string());
                                    }
                                    if let Some(end) = last_ts.take() {
                                        chat_msg.duration_ms = Some((end - ts).max(0) as u64);
                                    }
                                }
                                messages.push(chat_msg);
                            }
                        }
                        *first_ts = None;
                        *last_ts = None;
                    };

                let tool_result_preview = |content: &str| -> String {
                    let preview: String = content.lines().take(3).collect::<Vec<_>>().join("\n");
                    if preview.len() > 200 {
                        format!("{}…", &preview[..200])
                    } else if content.lines().count() > 3 {
                        format!("{preview}\n…")
                    } else {
                        preview
                    }
                };

                for msg in &session.messages {
                    match msg.role {
                        message::Role::User => {
                            let has_text = msg
                                .parts
                                .iter()
                                .any(|p| matches!(p, message::ContentPart::Text { .. }));
                            if has_text {
                                // Real user message — flush pending assistant
                                flush_assistant(
                                    &mut assistant_buf,
                                    &mut assistant_first_ts,
                                    &mut assistant_last_ts,
                                    &mut self.messages,
                                );
                                let text: String = msg
                                    .parts
                                    .iter()
                                    .filter_map(|p| match p {
                                        message::ContentPart::Text { text } => Some(text.as_str()),
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                self.messages.push(ChatMessage::new("user", text));
                            } else {
                                // Tool results — append to current assistant message
                                assistant_last_ts = Some(msg.created_at);
                                let buf = assistant_buf.get_or_insert_with(String::new);
                                for part in &msg.parts {
                                    if let message::ContentPart::ToolResult {
                                        id,
                                        content,
                                        is_error,
                                    } = part
                                    {
                                        let (name, args_summary) = tool_info
                                            .remove(id.as_str())
                                            .unwrap_or_else(|| ("tool".into(), String::new()));
                                        let dur = tool_durations.get(id.as_str()).copied();
                                        if !buf.is_empty() && !buf.ends_with('\n') {
                                            buf.push('\n');
                                        }
                                        let status = if *is_error { " ✗" } else { "" };
                                        match dur {
                                            Some(ms) => buf.push_str(&format!(
                                                "⚙ {name}{status} ({ms}ms) {args_summary}\n"
                                            )),
                                            None => buf.push_str(&format!(
                                                "⚙ {name}{status} {args_summary}\n"
                                            )),
                                        }
                                        let preview = tool_result_preview(content);
                                        if !preview.is_empty() {
                                            for line in preview.lines() {
                                                buf.push_str(&format!("  │ {line}\n"));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        message::Role::Assistant => {
                            if assistant_first_ts.is_none() {
                                assistant_first_ts = Some(msg.created_at);
                            }
                            assistant_last_ts = Some(msg.created_at);
                            let buf = assistant_buf.get_or_insert_with(String::new);
                            for part in &msg.parts {
                                match part {
                                    message::ContentPart::Text { text } => {
                                        if !buf.is_empty() && !buf.ends_with('\n') {
                                            buf.push('\n');
                                        }
                                        buf.push_str(text);
                                    }
                                    message::ContentPart::ToolUse { id, name, input } => {
                                        tool_info.insert(
                                            id.clone(),
                                            (name.clone(), compact_tool_args(&input.to_string())),
                                        );
                                    }
                                    _ => {}
                                }
                            }
                        }
                        message::Role::System => {
                            flush_assistant(
                                &mut assistant_buf,
                                &mut assistant_first_ts,
                                &mut assistant_last_ts,
                                &mut self.messages,
                            );
                            let text: String = msg
                                .parts
                                .iter()
                                .filter_map(|p| match p {
                                    message::ContentPart::Text { text } => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            if !text.is_empty() {
                                self.messages.push(ChatMessage::new("system", text));
                            }
                        }
                    }
                }
                // Flush remaining assistant content
                flush_assistant(
                    &mut assistant_buf,
                    &mut assistant_first_ts,
                    &mut assistant_last_ts,
                    &mut self.messages,
                );
            }
            Err(e) => {
                self.messages.push(ChatMessage::new(
                    "system",
                    format!("Failed to load session: {e}"),
                ));
            }
        }
    }

    fn slash_suggestions() -> Vec<Suggestion> {
        let mut items = vec![
            Suggestion {
                label: "/models".into(),
                description: "List available models".into(),
                needs_arg: false,
            },
            Suggestion {
                label: "/models refresh".into(),
                description: "Refresh model cache".into(),
                needs_arg: false,
            },
            Suggestion {
                label: "/evolution log".into(),
                description: "Show evolution history".into(),
                needs_arg: false,
            },
            Suggestion {
                label: "/evolution consolidate".into(),
                description: "Consolidate learnings".into(),
                needs_arg: false,
            },
            Suggestion {
                label: "/skills".into(),
                description: "List available skills".into(),
                needs_arg: false,
            },
            Suggestion {
                label: "/skill".into(),
                description: "Show skill content".into(),
                needs_arg: true,
            },
        ];

        let workspace_root = std::env::current_dir().unwrap_or_default();
        if let Ok(registry) = skill::SkillRegistry::load(&workspace_root) {
            for s in registry.on_demand() {
                items.push(Suggestion {
                    label: format!("/{}", s.name),
                    description: s.description.clone(),
                    needs_arg: false,
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
                    needs_arg: false,
                });
            }
        }
        items
    }

    fn update_suggestions(&mut self) -> Option<AppAction> {
        let input = &self.input;

        if input.starts_with('/') {
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

        let content_width = state
            .items
            .iter()
            .map(|s| {
                // "▸ " (2) + label + "  " (2) + description
                2 + s.label.len() + 2 + s.description.len()
            })
            .max()
            .unwrap_or(20) as u16
            + 2; // border padding
        let popup_width = content_width.max(40).min(input_area.width);

        let popup_area = Rect {
            x: input_area.x,
            y: input_area.y.saturating_sub(popup_height),
            width: popup_width,
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
                    Style::default().fg(palette::ACCENT)
                } else {
                    Style::default().fg(palette::MUTED)
                };
                Line::from(vec![
                    Span::styled(marker, style),
                    Span::styled(&s.label, style),
                    Span::styled(
                        format!("  {}", s.description),
                        Style::default().fg(palette::MUTED),
                    ),
                ])
            })
            .collect();

        frame.render_widget(Clear, popup_area);
        frame.render_widget(
            Paragraph::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(palette::BORDER))
                        .style(Style::default().bg(palette::SURFACE)),
                )
                .style(Style::default().bg(palette::SURFACE).fg(palette::FG)),
            popup_area,
        );
    }

    fn render_approval_prompt(
        &self,
        frame: &mut Frame,
        area: Rect,
        approval: &bus::ApprovalRequest,
    ) {
        let tool = &approval.tool;
        let reason = &approval.reason;

        // Format input args compactly
        let args_display = compact_tool_args(&approval.input.to_string());
        let args_line = if args_display.is_empty() {
            String::new()
        } else {
            format!("  args: {args_display}")
        };

        let mut lines = vec![
            Line::from(Span::styled(
                format!(" ⚠ Tool requires approval: {tool} "),
                Style::default().fg(palette::YELLOW),
            )),
            Line::from(Span::styled(
                format!("  reason: {reason}"),
                Style::default().fg(palette::FG),
            )),
        ];
        if !args_line.is_empty() {
            lines.push(Line::from(Span::styled(
                args_line,
                Style::default().fg(palette::MUTED),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  Press ", Style::default().fg(palette::MUTED)),
            Span::styled("y", Style::default().fg(palette::GREEN)),
            Span::styled(" to approve, ", Style::default().fg(palette::MUTED)),
            Span::styled("any other key", Style::default().fg(palette::RED)),
            Span::styled(" to deny", Style::default().fg(palette::MUTED)),
        ]));

        let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(4));
        let width = 60.min(area.width.saturating_sub(4));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;

        let popup = Rect {
            x,
            y,
            width,
            height,
        };

        let block = Block::default()
            .title(" Approval Required ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(palette::YELLOW))
            .style(Style::default().bg(palette::BG));
        let inner = block.inner(popup);
        frame.render_widget(Clear, popup);
        frame.render_widget(block, popup);
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn refresh_status(&mut self) {
        let providers = configured_provider_names().unwrap_or_default();
        if providers.is_empty() && self.messages.is_empty() {
            self.messages.push(ChatMessage::new("system", "No providers configured. Set OPENAI_API_KEY or ANTHROPIC_API_KEY, or run `omh auth login`.".to_string()));
        }
    }

    fn set_active_model(&mut self, provider_id: &str, model_id: &str) -> Result<()> {
        let active = crate::auth::ActiveModel {
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
        };
        let config = OmhConfig {
            active_model: Some(active),
        };
        config.save()?;
        if let Ok(cwd) = std::env::current_dir() {
            let _ = config.save_to(&OmhConfig::project_path(&cwd));
        }
        self.provider_id = provider_id.to_string();
        self.model_id = model_id.to_string();
        self.refresh_status();
        self.messages.push(ChatMessage::new(
            "system",
            format!("✓ Active model set to {provider_id}/{model_id}"),
        ));
        Ok(())
    }

    fn start_agent_turn(&mut self, text: String) {
        if self.is_streaming {
            self.messages.push(ChatMessage::new(
                "system",
                "Wait for the current response to finish streaming.".to_string(),
            ));
            return;
        }

        let Some(harness) = self.harness.clone() else {
            self.messages.push(ChatMessage::new(
                "system",
                "Runtime harness is unavailable.".to_string(),
            ));
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
            self.messages
                .push(ChatMessage::new("system", "No active session.".to_string()));
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
        let model_override = ModelSpec {
            model_id: self.model_id.clone(),
            provider_id: Some(self.provider_id.clone()),
        };
        let max_turns = harness
            .agent_registry
            .get(DEFAULT_AGENT)
            .and_then(|agent| agent.max_turns)
            .unwrap_or(30);
        tokio::spawn(async move {
            let runtime = AgentRuntime::new(agent_name, session_id.clone(), max_turns);
            let mut runtime = runtime.with_logger(&harness);
            runtime.model_override = Some(model_override);
            runtime.shared_harness = Some(harness.clone());
            if let Err(error) = runtime.run_turn(&harness, &text).await {
                harness.bus.publish(bus::AgentEvent::Error {
                    session_id: Some(session_id.clone()),
                    message: error.to_string(),
                });
                harness
                    .bus
                    .publish(bus::AgentEvent::TurnComplete { session_id });
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
                    content: "You are a title generator. Output ONLY the title, nothing else."
                        .into(),
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
                session_id,
                tool,
                args,
                result,
                is_error,
                duration_ms,
            } => {
                if matches_session(&session_id, &self.session_id) {
                    if !self.streaming_text.is_empty() && !self.streaming_text.ends_with('\n') {
                        self.streaming_text.push('\n');
                    }
                    let status = if is_error { " ✗" } else { "" };
                    let args_summary = compact_tool_args(&args);
                    self.streaming_text.push_str(&format!(
                        "⚙ {tool}{status} ({duration_ms}ms) {args_summary}\n"
                    ));
                    // Show truncated result (first 3 lines, max 200 chars)
                    let preview: String = result.lines().take(3).collect::<Vec<_>>().join("\n");
                    let preview = if preview.len() > 200 {
                        format!("{}…", &preview[..200])
                    } else if result.lines().count() > 3 {
                        format!("{preview}\n…")
                    } else {
                        preview
                    };
                    if !preview.is_empty() {
                        for line in preview.lines() {
                            self.streaming_text.push_str(&format!("  │ {line}\n"));
                        }
                    }
                    self.update_last_assistant_message();
                }
            }
            bus::AgentEvent::TokenUsage {
                session_id,
                input_tokens,
                output_tokens,
            } => {
                if matches_session(&session_id, &self.session_id) {
                    self.input_tokens = self.input_tokens.saturating_add(input_tokens);
                    self.output_tokens = self.output_tokens.saturating_add(output_tokens);
                    if let Some(harness) = &self.harness {
                        let _ = harness.session_manager.update_tokens(
                            &session_id,
                            self.input_tokens as u64,
                            self.output_tokens as u64,
                        );
                    }
                    self.refresh_status();
                }
            }
            bus::AgentEvent::TurnComplete { session_id } => {
                if matches_session(&session_id, &self.session_id) {
                    self.is_streaming = false;
                    if let Some(start) = self.turn_start.take() {
                        let elapsed = start.elapsed().as_millis() as u64;
                        if let Some(msg) = self
                            .messages
                            .iter_mut()
                            .rev()
                            .find(|m| m.role == "assistant")
                        {
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
            bus::AgentEvent::FileModified { .. } => {
                // git status is polled periodically instead
            }
            bus::AgentEvent::McpServersChanged { servers } => {
                self.mcp_servers = servers;
            }
            _ => {}
        }
    }

    fn show_model_picker(&mut self, grouped_models: Vec<(String, Vec<ModelInfo>)>) {
        if grouped_models.is_empty() {
            self.messages.push(ChatMessage::new(
                "system",
                "No models available from configured providers.".to_string(),
            ));
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
                items.push(Suggestion {
                    label,
                    description,
                    needs_arg: false,
                });
            }
        }

        if items.is_empty() {
            self.messages.push(ChatMessage::new(
                "system",
                "No models available from configured providers.".to_string(),
            ));
            self.suggestions = None;
            return;
        }

        self.suggestions = Some(SuggestionState {
            items,
            selected: 0,
            trigger: SuggestionTrigger::Model,
        });
    }

    fn cursor_position(&self, input_area: Rect) -> Option<(u16, u16)> {
        let prompt_width = if self.is_streaming { 3u16 } else { 2u16 };
        Some((
            input_area.x + 1 + prompt_width + self.input[..self.cursor_position].width() as u16,
            input_area.y + 1,
        ))
    }

    fn handle_mouse(&mut self, col: u16, row: u16, kind: MouseEventKind) {
        let scroll_amount: u16 = 3;

        // Text selection in messages area
        if self.last_messages_area.contains((col, row).into()) {
            match kind {
                MouseEventKind::Down(event::MouseButton::Left) => {
                    let pos = self.screen_to_content(col, row);
                    self.selection_anchor = Some(pos);
                    self.selection_end = Some(pos);
                    return;
                }
                MouseEventKind::Drag(event::MouseButton::Left) => {
                    if self.selection_anchor.is_some() {
                        let pos = self.screen_to_content(col, row);
                        self.selection_end = Some(pos);
                    }
                    return;
                }
                MouseEventKind::Up(event::MouseButton::Left) => {
                    if let (Some(anchor), Some(_)) = (self.selection_anchor, self.selection_end) {
                        let end_pos = self.screen_to_content(col, row);
                        self.selection_end = Some(end_pos);
                        let text = Self::extract_selection(&self.msg_plain_lines, anchor, end_pos);
                        if !text.trim().is_empty() {
                            Self::copy_to_clipboard(text.trim());
                        }
                    }
                    self.selection_anchor = None;
                    self.selection_end = None;
                    return;
                }
                _ => {}
            }
        } else {
            // Click outside messages clears selection
            if matches!(kind, MouseEventKind::Down(_)) {
                self.selection_anchor = None;
                self.selection_end = None;
            }
        }

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
        } else if self.last_status_area.contains((col, row).into()) {
            if up {
                self.status_scroll = self.status_scroll.saturating_sub(scroll_amount);
            } else if down {
                self.status_scroll = self.status_scroll.saturating_add(scroll_amount);
            }
        }
    }

    /// Map screen (col, row) to content (wrapped_row, display_col).
    fn screen_to_content(&self, col: u16, row: u16) -> (usize, usize) {
        let area = self.last_messages_area;
        let relative_row = row.saturating_sub(area.y + 1) as usize;
        let scroll = self.msg_max_scroll.saturating_sub(self.msg_scroll) as usize;
        let content_row = (scroll + relative_row).min(self.msg_plain_lines.len().saturating_sub(1));
        let content_col = col.saturating_sub(area.x + 1) as usize;
        (content_row, content_col)
    }

    /// Manually wrap logical Lines into screen-width rows.
    fn wrap_lines(
        lines: Vec<Line<'static>>,
        max_width: usize,
    ) -> (Vec<Line<'static>>, Vec<String>) {
        let mut wrapped = Vec::new();
        let mut plain = Vec::new();

        for line in lines {
            let mut row_spans: Vec<Span<'static>> = Vec::new();
            let mut row_plain = String::new();
            let mut row_width: usize = 0;

            for span in line.spans {
                let style = span.style;
                let text = span.content.to_string();
                let mut buf = String::new();

                for ch in text.chars() {
                    let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    if row_width + cw > max_width && row_width > 0 {
                        if !buf.is_empty() {
                            row_spans.push(Span::styled(buf.clone(), style));
                            buf.clear();
                        }
                        wrapped.push(Line::from(std::mem::take(&mut row_spans)));
                        plain.push(std::mem::take(&mut row_plain));
                        row_width = 0;
                    }
                    buf.push(ch);
                    row_plain.push(ch);
                    row_width += cw;
                }
                if !buf.is_empty() {
                    row_spans.push(Span::styled(buf, style));
                }
            }
            wrapped.push(Line::from(row_spans));
            plain.push(row_plain);
        }

        (wrapped, plain)
    }

    /// Apply selection highlight to wrapped lines between two (row, col) points.
    fn apply_selection_highlight(
        lines: &mut [Line<'static>],
        start: (usize, usize),
        end: (usize, usize),
        bg: Color,
    ) {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let last = lines.len().saturating_sub(1);

        for row_idx in start.0..=end.0.min(last) {
            let left = if row_idx == start.0 { start.1 } else { 0 };
            let right = if row_idx == end.0 {
                end.1 + 1
            } else {
                usize::MAX
            };

            let line = &lines[row_idx];
            let mut new_spans: Vec<Span<'static>> = Vec::new();
            let mut col: usize = 0;

            for span in &line.spans {
                let text = span.content.to_string();
                let style = span.style;
                let mut before = String::new();
                let mut sel = String::new();
                let mut after = String::new();

                for ch in text.chars() {
                    let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    if col < left {
                        before.push(ch);
                    } else if col < right {
                        sel.push(ch);
                    } else {
                        after.push(ch);
                    }
                    col += cw;
                }

                if !before.is_empty() {
                    new_spans.push(Span::styled(before, style));
                }
                if !sel.is_empty() {
                    new_spans.push(Span::styled(sel, style.bg(bg)));
                }
                if !after.is_empty() {
                    new_spans.push(Span::styled(after, style));
                }
            }

            lines[row_idx] = Line::from(new_spans);
        }
    }

    /// Extract plain text from a selection range.
    fn extract_selection(plain: &[String], start: (usize, usize), end: (usize, usize)) -> String {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let mut result = String::new();
        if plain.is_empty() {
            return result;
        }

        for row_idx in start.0..=end.0.min(plain.len() - 1) {
            if row_idx > start.0 {
                result.push('\n');
            }
            let text = &plain[row_idx];
            let left = if row_idx == start.0 { start.1 } else { 0 };
            let right = if row_idx == end.0 {
                end.1 + 1
            } else {
                usize::MAX
            };

            let mut col = 0;
            for ch in text.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if col >= left && col < right {
                    result.push(ch);
                }
                col += cw;
            }
        }

        // Strip decorative border chars from copied text
        let cleaned: Vec<&str> = result
            .lines()
            .filter(|l| !l.chars().all(|c| c == '─' || c == ' '))
            .map(|l| l.strip_prefix("│ ").unwrap_or(l))
            .collect();
        cleaned.join("\n")
    }

    /// Copy text to system clipboard via OSC 52 escape sequence.
    fn copy_to_clipboard(text: &str) {
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        // OSC 52: \x1b]52;c;<base64>\x07
        print!("\x1b]52;c;{encoded}\x07");
    }

    fn last_assistant_content(&self) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant")
            .map(|m| m.content.as_str())
    }

    fn handle_key(&mut self, key: event::KeyEvent) -> Option<AppAction> {
        if key.kind != KeyEventKind::Press {
            return None;
        }

        // Handle pending approval prompt — y/n/Esc
        if let Some(approval) = self.pending_approval.take() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let tool_name = approval.tool.clone();
                    let _ = approval.respond.send(bus::ApprovalResponse::Allow);
                    self.messages.push(ChatMessage::new(
                        "system",
                        format!("✓ Approved: {tool_name}"),
                    ));
                }
                _ => {
                    let tool_name = approval.tool.clone();
                    let _ = approval.respond.send(bus::ApprovalResponse::Deny);
                    self.messages
                        .push(ChatMessage::new("system", format!("✗ Denied: {tool_name}")));
                }
            }
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
                    let needs_arg = state.items[state.selected].needs_arg;
                    let trigger = state.trigger;
                    match trigger {
                        SuggestionTrigger::Slash if !needs_arg => {
                            // No-arg: fill input and fall through to main Enter handler
                            self.input = selected;
                            self.cursor_position = self.input.len();
                            self.suggestions = None;
                            // Don't return — let the main key handler execute it
                        }
                        SuggestionTrigger::Slash => {
                            self.input = selected;
                            self.cursor_position = self.input.len();
                            self.input.push(' ');
                            self.cursor_position += 1;
                            self.suggestions = None;
                            return None;
                        }
                        SuggestionTrigger::Agent => {
                            if let Some(at_pos) = self.input.rfind('@') {
                                self.input.truncate(at_pos);
                                self.input.push_str(&selected);
                                self.input.push(' ');
                                self.cursor_position = self.input.len();
                            }
                            self.suggestions = None;
                            return None;
                        }
                        SuggestionTrigger::Model => {
                            if let Some((provider_id, model_id)) = selected.split_once('/') {
                                if let Err(error) = self.set_active_model(provider_id, model_id) {
                                    self.messages.push(ChatMessage::new(
                                        "system",
                                        format!("Error: {error}"),
                                    ));
                                }
                            }
                            self.input.clear();
                            self.cursor_position = 0;
                            self.suggestions = None;
                            return None;
                        }
                    }
                }
                KeyCode::Esc => {
                    self.suggestions = None;
                    return None;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Char('d') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(content) = self.last_assistant_content() {
                    Self::copy_to_clipboard(content);
                }
            }
            KeyCode::Enter => {
                if !self.input.is_empty() {
                    let text = self.input.clone();
                    self.input.clear();
                    self.cursor_position = 0;

                    let workspace_root = std::env::current_dir().unwrap_or_default();
                    if let Some(invocation) = slash_input::parse_slash_invocation(&text) {
                        match slash::dispatch(&invocation, &workspace_root) {
                            Ok(SlashResult::Response(response)) => {
                                self.messages.push(ChatMessage::new("user", text));
                                self.messages.push(ChatMessage::new("system", response));
                                self.refresh_status();
                            }
                            Ok(SlashResult::Notify(msg)) => {
                                self.messages.push(ChatMessage::new("system", msg));
                            }
                            Ok(SlashResult::ListModels { force_refresh }) => {
                                return Some(AppAction::LoadModels { force_refresh });
                            }
                            Ok(SlashResult::ListAgents) => {
                                // List primary agents in message panel
                                if let Some(harness) = &self.harness {
                                    let agents = harness.agent_registry.primary_switchable_agents();
                                    let list = agents
                                        .iter()
                                        .map(|a| format!("  {}: {}", a.name, a.description))
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    self.messages.push(ChatMessage::new(
                                        "system",
                                        format!("Available agents:\n{list}"),
                                    ));
                                }
                            }
                            Ok(SlashResult::ListNotifications) => {
                                // Placeholder — notifications not yet in this branch
                                self.messages
                                    .push(ChatMessage::new("system", "No notifications."));
                            }
                            Err(e) => {
                                self.messages
                                    .push(ChatMessage::new("system", format!("Error: {e}")));
                            }
                        }
                    } else {
                        self.start_agent_turn(text);
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
                    lines.push(Line::from(vec![
                        Span::styled("┌─", Style::default().fg(palette::BORDER)),
                        Span::styled(
                            if lang.is_empty() {
                                " code ".to_string()
                            } else {
                                format!(" {lang} ")
                            },
                            Style::default().fg(palette::MUTED),
                        ),
                    ]));
                    if let Some(syntax) = self.syntax_set.find_syntax_by_token(lang) {
                        highlighter = Some(HighlightLines::new(syntax, theme));
                    } else {
                        highlighter = None;
                    }
                } else {
                    highlighter = None;
                    lines.push(Line::from(Span::styled(
                        "└─",
                        Style::default().fg(palette::BORDER),
                    )));
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
                    spans.push(Span::styled("│ ", Style::default().fg(palette::BORDER)));
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
                        Span::styled("│ ", Style::default().fg(palette::BORDER)),
                        Span::styled(line.to_string(), Style::default().fg(palette::ORANGE)),
                    ]));
                }
                continue;
            }

            // Horizontal rule
            if matches!(line.trim(), "---" | "***" | "___") {
                lines.push(Line::from(Span::styled(
                    "─".repeat(40),
                    Style::default().fg(palette::BORDER),
                )));
                continue;
            }

            // Headings
            let heading = if line.starts_with("#### ") {
                Some((&line[5..], palette::CYAN, false))
            } else if line.starts_with("### ") {
                Some((&line[4..], palette::CYAN, true))
            } else if line.starts_with("## ") {
                Some((&line[3..], palette::CYAN, true))
            } else if line.starts_with("# ") {
                Some((&line[2..], palette::ACCENT, true))
            } else {
                None
            };
            if let Some((text, color, bold)) = heading {
                let mut style = Style::default().fg(color);
                if bold {
                    style = style.add_modifier(Modifier::BOLD);
                }
                lines.push(Line::from(Span::styled(text.to_string(), style)));
                continue;
            }

            // Blockquote
            if let Some(quoted) = line.strip_prefix("> ") {
                let inner = Self::parse_inline_spans(quoted);
                let mut spans = vec![Span::styled("▎ ", Style::default().fg(palette::BORDER))];
                for s in inner {
                    spans.push(Span::styled(s.content, s.style.fg(palette::MUTED)));
                }
                lines.push(Line::from(spans));
                continue;
            }

            // Unordered list with nesting
            let trimmed = line.trim_start();
            let indent_chars = line.len() - trimmed.len();
            if let Some(rest) = trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))
            {
                let depth = indent_chars / 2;
                let bullet = match depth {
                    0 => "•",
                    1 => "◦",
                    _ => "▪",
                };
                let pad = "  ".repeat(depth);
                let inner = Self::parse_inline_spans(rest);
                let mut spans: Vec<Span<'static>> = vec![
                    Span::raw(pad),
                    Span::styled(format!(" {bullet} "), Style::default().fg(palette::ACCENT)),
                ];
                spans.extend(inner);
                lines.push(Line::from(spans));
                continue;
            }

            // Ordered list (e.g. "1. ", "2. ", "10. ")
            if let Some(dot_pos) = trimmed.find(". ") {
                if dot_pos <= 3 && trimmed[..dot_pos].chars().all(|c| c.is_ascii_digit()) {
                    let num = &trimmed[..dot_pos];
                    let rest = &trimmed[dot_pos + 2..];
                    let pad = "  ".repeat(indent_chars / 2);
                    let inner = Self::parse_inline_spans(rest);
                    let mut spans: Vec<Span<'static>> = vec![
                        Span::raw(pad),
                        Span::styled(format!(" {num}. "), Style::default().fg(palette::ACCENT)),
                    ];
                    spans.extend(inner);
                    lines.push(Line::from(spans));
                    continue;
                }
            }

            // Normal paragraph with inline formatting
            let spans = Self::parse_inline_spans(line);
            if spans.is_empty() {
                lines.push(Line::from(Span::raw("")));
            } else {
                lines.push(Line::from(spans));
            }
        }
        lines
    }

    /// Parse inline markdown: `code`, **bold**, *italic*, ~~strikethrough~~, [link](url)
    fn parse_inline_spans(text: &str) -> Vec<Span<'static>> {
        let chars: Vec<char> = text.chars().collect();
        let len = chars.len();
        let mut spans = Vec::new();
        let mut plain = String::new();
        let mut i = 0;

        let flush = |spans: &mut Vec<Span<'static>>, plain: &mut String| {
            if !plain.is_empty() {
                spans.push(Span::raw(plain.clone()));
                plain.clear();
            }
        };

        while i < len {
            // Inline code `...`
            if chars[i] == '`' {
                if let Some(end) = Self::find_closing(&chars, i + 1, '`') {
                    flush(&mut spans, &mut plain);
                    let code: String = chars[i + 1..end].iter().collect();
                    spans.push(Span::styled(
                        code,
                        Style::default().fg(palette::ORANGE).bg(palette::BG),
                    ));
                    i = end + 1;
                    continue;
                }
            }

            // Strikethrough ~~...~~
            if i + 1 < len && chars[i] == '~' && chars[i + 1] == '~' {
                if let Some(end) = Self::find_closing_pair(&chars, i + 2, '~', '~') {
                    flush(&mut spans, &mut plain);
                    let text: String = chars[i + 2..end].iter().collect();
                    spans.push(Span::styled(
                        text,
                        Style::default()
                            .add_modifier(Modifier::CROSSED_OUT)
                            .fg(palette::MUTED),
                    ));
                    i = end + 2;
                    continue;
                }
            }

            // Bold **...**
            if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
                if let Some(end) = Self::find_closing_pair(&chars, i + 2, '*', '*') {
                    flush(&mut spans, &mut plain);
                    let text: String = chars[i + 2..end].iter().collect();
                    spans.push(Span::styled(
                        text,
                        Style::default()
                            .fg(palette::FG)
                            .add_modifier(Modifier::BOLD),
                    ));
                    i = end + 2;
                    continue;
                }
            }

            // Italic *...* (single, not at word boundary heuristic: next char is not space)
            if chars[i] == '*' && i + 1 < len && chars[i + 1] != ' ' {
                if let Some(end) = Self::find_closing(&chars, i + 1, '*') {
                    if end > i + 1 {
                        flush(&mut spans, &mut plain);
                        let text: String = chars[i + 1..end].iter().collect();
                        spans.push(Span::styled(
                            text,
                            Style::default().add_modifier(Modifier::ITALIC),
                        ));
                        i = end + 1;
                        continue;
                    }
                }
            }

            // Italic _..._ (must not be inside a word — check boundaries)
            if chars[i] == '_'
                && i + 1 < len
                && chars[i + 1] != ' '
                && (i == 0 || chars[i - 1] == ' ')
            {
                if let Some(end) = Self::find_closing(&chars, i + 1, '_') {
                    if end > i + 1
                        && (end + 1 >= len
                            || chars[end + 1] == ' '
                            || chars[end + 1].is_ascii_punctuation())
                    {
                        flush(&mut spans, &mut plain);
                        let text: String = chars[i + 1..end].iter().collect();
                        spans.push(Span::styled(
                            text,
                            Style::default().add_modifier(Modifier::ITALIC),
                        ));
                        i = end + 1;
                        continue;
                    }
                }
            }

            // Link [text](url) — render text in cyan, underlined
            if chars[i] == '[' {
                if let Some(bracket_end) = Self::find_closing(&chars, i + 1, ']') {
                    if bracket_end + 1 < len && chars[bracket_end + 1] == '(' {
                        if let Some(paren_end) = Self::find_closing(&chars, bracket_end + 2, ')') {
                            flush(&mut spans, &mut plain);
                            let link_text: String = chars[i + 1..bracket_end].iter().collect();
                            spans.push(Span::styled(
                                link_text,
                                Style::default()
                                    .fg(palette::ACCENT)
                                    .add_modifier(Modifier::UNDERLINED),
                            ));
                            i = paren_end + 1;
                            continue;
                        }
                    }
                }
            }

            plain.push(chars[i]);
            i += 1;
        }

        flush(&mut spans, &mut plain);
        spans
    }

    /// Find index of closing single char, skipping escaped chars
    fn find_closing(chars: &[char], start: usize, closing: char) -> Option<usize> {
        let mut j = start;
        while j < chars.len() {
            if chars[j] == '\\' {
                j += 2;
                continue;
            }
            if chars[j] == closing {
                return Some(j);
            }
            j += 1;
        }
        None
    }

    /// Find index of closing double-char pair (e.g. ** or ~~)
    fn find_closing_pair(chars: &[char], start: usize, c1: char, c2: char) -> Option<usize> {
        let mut j = start;
        while j + 1 < chars.len() {
            if chars[j] == '\\' {
                j += 2;
                continue;
            }
            if chars[j] == c1 && chars[j + 1] == c2 {
                return Some(j);
            }
            j += 1;
        }
        None
    }

    fn poll_git_status(&mut self) {
        let now = Instant::now();
        let should_poll = match self.last_git_poll {
            Some(last) => now.duration_since(last) > Duration::from_secs(3),
            None => true,
        };
        if !should_poll {
            return;
        }
        self.last_git_poll = Some(now);

        let cwd = std::env::current_dir().unwrap_or_default();
        self.modified_files = collect_git_status(&cwd);
    }

    fn render(&mut self, frame: &mut Frame) {
        self.tick = self.tick.wrapping_add(1);
        let area = frame.area();

        let main_chunks =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(32)]).split(area);
        let left_area = main_chunks[0];
        let right_area = main_chunks[1];
        self.last_status_area = right_area;

        let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).split(left_area);
        let messages_area = chunks[0];
        let input_area = chunks[1];
        self.last_messages_area = messages_area;

        // Render Messages
        let mut text_lines: Vec<Line> = Vec::new();
        let msg_width = messages_area.width.saturating_sub(2) as usize;

        let separator = |width: usize| -> Line<'static> {
            let rule = "─".repeat(width.saturating_sub(1));
            Line::from(Span::styled(rule, Style::default().fg(palette::BORDER)))
        };

        for (idx, msg) in self.messages.iter().enumerate() {
            if idx > 0 {
                text_lines.push(separator(msg_width));
            }

            let is_tool_output = msg.role == "assistant" && msg.content.starts_with("⚙");

            // Left-border accent color per role
            let accent = if msg.role == "user" {
                palette::GREEN
            } else if msg.role == "system" {
                palette::YELLOW
            } else if is_tool_output {
                palette::MUTED
            } else {
                palette::ACCENT
            };

            let bar = Span::styled("│ ", Style::default().fg(accent));

            if msg.role == "user" {
                let user_bg = palette::SURFACE_BRIGHT;
                let pad_user = |line: Line<'static>, width: usize| -> Line<'static> {
                    let visible: usize = line
                        .spans
                        .iter()
                        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                        .sum();
                    let mut spans: Vec<Span<'static>> = line
                        .spans
                        .into_iter()
                        .map(|s| Span::styled(s.content, s.style.bg(user_bg)))
                        .collect();
                    let remaining = width.saturating_sub(visible);
                    if remaining > 0 {
                        spans.push(Span::styled(
                            " ".repeat(remaining),
                            Style::default().bg(user_bg),
                        ));
                    }
                    Line::from(spans)
                };
                text_lines.push(pad_user(
                    Line::from(vec![
                        bar.clone(),
                        Span::styled(
                            "You",
                            Style::default()
                                .fg(palette::GREEN)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    msg_width,
                ));
                let content_lines = self.render_message_content(&msg.content);
                for line in content_lines {
                    let mut spans = vec![bar.clone()];
                    spans.extend(line.spans);
                    text_lines.push(pad_user(Line::from(spans), msg_width));
                }
            } else if msg.role == "assistant" {
                if !is_tool_output {
                    let mut header_spans: Vec<Span<'static>> = vec![
                        bar.clone(),
                        Span::styled(
                            "Assistant",
                            Style::default()
                                .fg(palette::ACCENT)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ];
                    if let Some(started) = &msg.started_at {
                        let timing = match msg.duration_ms {
                            Some(ms) => format!("  {started} · {:.1}s", ms as f64 / 1000.0),
                            None => {
                                let spinner = SPINNER_FRAMES[self.tick / 4 % SPINNER_FRAMES.len()];
                                format!("  {started} {spinner}")
                            }
                        };
                        header_spans
                            .push(Span::styled(timing, Style::default().fg(palette::MUTED)));
                    }
                    text_lines.push(Line::from(header_spans));
                }
                let content_lines = self.render_message_content(&msg.content);
                for line in content_lines {
                    let mut spans = vec![bar.clone()];
                    let line_text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                    let is_tool_line = line_text.starts_with("⚙") || line_text.starts_with("  │");
                    if is_tool_line {
                        spans.extend(
                            line.spans.into_iter().map(|s| {
                                Span::styled(s.content, Style::default().fg(palette::MUTED))
                            }),
                        );
                    } else {
                        spans.extend(line.spans);
                    }
                    text_lines.push(Line::from(spans));
                }
            } else if msg.role == "system" {
                let mut content = msg.content.clone();
                if let Some(stripped) = content.strip_prefix("system: ") {
                    content = stripped.to_string();
                }
                text_lines.push(Line::from(vec![
                    bar.clone(),
                    Span::styled(
                        "System",
                        Style::default()
                            .fg(palette::YELLOW)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                let content_lines = self.render_message_content(&content);
                for line in content_lines {
                    let mut spans = vec![bar.clone()];
                    spans.extend(
                        line.spans
                            .into_iter()
                            .map(|s| Span::styled(s.content, s.style.fg(palette::YELLOW))),
                    );
                    text_lines.push(Line::from(spans));
                }
            } else {
                text_lines.push(Line::from(vec![
                    bar.clone(),
                    Span::styled(format!("{}: ", msg.role), Style::default().fg(palette::FG)),
                    Span::raw(msg.content.clone()),
                ]));
            }
        }

        // Manual wrapping for character-level selection
        let inner_width = messages_area.width.saturating_sub(2) as usize;
        let (mut wrapped_lines, plain_lines) = Self::wrap_lines(text_lines, inner_width);

        // Apply character-level selection highlight
        if let (Some(anchor), Some(end)) = (self.selection_anchor, self.selection_end) {
            let sel_bg = Color::Rgb(38, 79, 120); // VS Code selection blue
            Self::apply_selection_highlight(&mut wrapped_lines, anchor, end, sel_bg);
        }

        let wrapped_count = wrapped_lines.len();
        let visible_height = messages_area.height.saturating_sub(2) as usize;
        let max_scroll = wrapped_count
            .saturating_sub(visible_height)
            .min(u16::MAX as usize) as u16;
        if self.scroll_offset > max_scroll {
            self.scroll_offset = max_scroll;
        }
        self.msg_plain_lines = plain_lines;
        self.msg_scroll = self.scroll_offset;
        self.msg_max_scroll = max_scroll;

        let current_line = max_scroll.saturating_sub(self.scroll_offset) as usize;
        let title = if wrapped_count > visible_height {
            format!(" omh [{}/{wrapped_count}] ", current_line + visible_height)
        } else {
            " omh ".to_string()
        };

        let title_style = if self.is_streaming {
            Style::default()
                .fg(palette::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette::MUTED)
        };

        let messages_widget = Paragraph::new(wrapped_lines)
            .block(
                Block::default()
                    .title(Span::styled(title, title_style))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(palette::BORDER)),
            )
            .style(Style::default().bg(palette::BG).fg(palette::FG))
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
        let prompt_char = if self.is_streaming { "⏳" } else { "❯" };
        let input_display = format!("{prompt_char} {}", self.input);
        let input_border_color = if self.is_streaming {
            palette::MAGENTA
        } else {
            palette::ACCENT
        };
        let hints = Line::from(vec![
            Span::styled(" ^Y", Style::default().fg(palette::ACCENT)),
            Span::styled("copy ", Style::default().fg(palette::MUTED)),
            Span::styled("^D", Style::default().fg(palette::ACCENT)),
            Span::styled("quit ", Style::default().fg(palette::MUTED)),
        ]);

        // Build footer left title: agent │ provider/model
        let agent_label = format!(" 🤖 {} ", DEFAULT_AGENT);
        let model_label = format!("{}/{} ", self.provider_id, self.model_id);
        let sep = "│ ";
        let footer_title = Line::from(vec![
            Span::styled(agent_label, Style::default().fg(palette::CYAN)),
            Span::styled(sep, Style::default().fg(palette::BORDER)),
            Span::styled(model_label, Style::default().fg(palette::MUTED)),
        ]);

        let input_widget = Paragraph::new(input_display.as_str())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(input_border_color))
                    .title_top(footer_title)
                    .title_bottom(hints.alignment(Alignment::Right)),
            )
            .style(Style::default().bg(palette::BG).fg(palette::FG));

        frame.render_widget(input_widget, input_area);

        if self.suggestions.is_some() {
            self.render_suggestions(frame, input_area);
        }

        // Approval prompt overlay
        if let Some(ref approval) = self.pending_approval {
            self.render_approval_prompt(frame, messages_area, approval);
        }

        if let Some((cursor_x, cursor_y)) = self.cursor_position(input_area) {
            frame.set_cursor_position((cursor_x, cursor_y));
        }

        let mut right_lines = Vec::new();

        if self.provider_id.is_empty() {
            right_lines.push(Line::from(vec![
                Span::styled(" 󰘬 ", Style::default().fg(palette::RED)),
                Span::styled("no provider", Style::default().fg(palette::RED)),
            ]));
        } else {
            right_lines.push(Line::from(vec![
                Span::styled(" 󰘬 ", Style::default().fg(palette::ACCENT)),
                Span::raw(format!("{}/{}", self.provider_id, self.model_id)),
            ]));
        }
        let session_display = match self.session_id.as_deref() {
            Some(id) if id.len() > 12 => &id[..12],
            Some(id) => id,
            None => "new",
        };
        right_lines.push(Line::from(vec![
            Span::styled(" 󱂬 ", Style::default().fg(palette::MUTED)),
            Span::raw(session_display.to_string()),
        ]));
        right_lines.push(Line::from(vec![
            Span::styled(" 󰊖 ", Style::default().fg(palette::MUTED)),
            Span::raw(format!("{} / {}", self.input_tokens, self.output_tokens)),
        ]));

        let state_span = if self.is_streaming {
            let spinner = SPINNER_FRAMES[self.tick / 4 % SPINNER_FRAMES.len()];
            Span::styled(
                format!("{spinner} streaming"),
                Style::default().fg(palette::MAGENTA),
            )
        } else if self.loading_models {
            let spinner = SPINNER_FRAMES[self.tick / 4 % SPINNER_FRAMES.len()];
            Span::styled(
                format!("{spinner} loading models"),
                Style::default().fg(palette::YELLOW),
            )
        } else {
            Span::styled("● idle", Style::default().fg(palette::GREEN))
        };
        right_lines.push(Line::from(vec![Span::raw(" "), state_span]));
        right_lines.push(Line::raw(""));

        right_lines.push(Line::from(vec![Span::styled(
            "─── MCP Servers ───",
            Style::default().fg(palette::MUTED),
        )]));
        if self.mcp_servers.is_empty() {
            right_lines.push(Line::styled(" (none)", Style::default().fg(palette::MUTED)));
        } else {
            for srv in &self.mcp_servers {
                let (indicator, color) = if srv.status == "connected" {
                    ("●", palette::GREEN)
                } else {
                    ("○", palette::RED)
                };
                right_lines.push(Line::from(vec![
                    Span::styled(format!(" {indicator} "), Style::default().fg(color)),
                    Span::raw(&*srv.name),
                    Span::styled(
                        format!(" ({}T)", srv.tools_count),
                        Style::default().fg(palette::MUTED),
                    ),
                ]));
            }
        }
        right_lines.push(Line::raw(""));

        if !self.sub_agents.is_empty() {
            right_lines.push(Line::from(vec![Span::styled(
                "─── Sub-agents ───",
                Style::default().fg(palette::MUTED),
            )]));
            for agent in &self.sub_agents {
                right_lines.push(Line::from(vec![
                    Span::styled(" ▸ ", Style::default().fg(palette::CYAN)),
                    Span::raw(format!("{} ({})", agent.name, agent.status)),
                ]));
            }
            right_lines.push(Line::raw(""));
        }

        if !self.todos.is_empty() {
            right_lines.push(Line::from(vec![Span::styled(
                "─── Todos ───",
                Style::default().fg(palette::MUTED),
            )]));
            for todo in &self.todos {
                let (marker, color) = match todo.status.as_str() {
                    "completed" => ("✓", palette::GREEN),
                    "in_progress" => ("◌", palette::YELLOW),
                    _ => ("○", palette::MUTED),
                };
                right_lines.push(Line::from(vec![
                    Span::styled(format!(" {marker} "), Style::default().fg(color)),
                    Span::raw(format!("{}", todo.content)),
                ]));
            }
            right_lines.push(Line::raw(""));
        }

        right_lines.push(Line::from(vec![Span::styled(
            "─── Git Status ───",
            Style::default().fg(palette::MUTED),
        )]));
        if self.modified_files.is_empty() {
            right_lines.push(Line::styled(
                " (clean)",
                Style::default().fg(palette::MUTED),
            ));
        } else {
            for repo in &self.modified_files {
                if repo.root != "." || self.modified_files.len() > 1 {
                    right_lines.push(Line::from(vec![Span::styled(
                        format!(" {}/", repo.root),
                        Style::default()
                            .fg(palette::FG)
                            .add_modifier(Modifier::BOLD),
                    )]));
                }
                if repo.files.is_empty() {
                    right_lines.push(Line::styled(
                        "   (clean)",
                        Style::default().fg(palette::MUTED),
                    ));
                } else {
                    for file in &repo.files {
                        let (indicator, color) = match file.status.as_str() {
                            "M" => ("M", palette::YELLOW),
                            "A" => ("A", palette::GREEN),
                            "D" => ("D", palette::RED),
                            "R" => ("R", palette::CYAN),
                            "?" => ("?", palette::MUTED),
                            "C" => ("C", palette::RED),
                            _ => (" ", palette::FG),
                        };
                        right_lines.push(Line::from(vec![
                            Span::styled(format!("  {indicator} "), Style::default().fg(color)),
                            Span::styled(file.path.clone(), Style::default().fg(palette::FG)),
                        ]));
                    }
                }
            }
        }

        let total_right_lines = right_lines.len() as u16;
        let right_height = right_area.height.saturating_sub(2);
        let max_right_scroll = total_right_lines.saturating_sub(right_height);
        let right_scroll = self.status_scroll.min(max_right_scroll);

        let right_widget = Paragraph::new(right_lines)
            .block(
                Block::default()
                    .title(" status ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(palette::BORDER)),
            )
            .style(Style::default().bg(palette::SURFACE).fg(palette::FG))
            .wrap(Wrap { trim: false })
            .scroll((right_scroll, 0));

        frame.render_widget(right_widget, right_area);

        if total_right_lines > right_height {
            let mut right_scrollbar_state = ScrollbarState::new(total_right_lines as usize)
                .position(right_scroll as usize)
                .viewport_content_length(right_height as usize);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                right_area,
                &mut right_scrollbar_state,
            );
        }
    }
}

async fn fetch_models(force_refresh: bool) -> Result<Vec<(String, Vec<ModelInfo>)>> {
    let mut harness = crate::init_harness()?;
    crate::run::register_providers_from_env(&mut harness)?;

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

fn collect_git_status(workspace: &std::path::Path) -> Vec<GitRepoStatus> {
    // Check if workspace itself is a git repo
    let is_root_git = workspace.join(".git").exists();

    if is_root_git {
        // Single repo — the workspace root
        if let Some(status) = git_status_for_repo(workspace, ".") {
            return vec![status];
        }
        return Vec::new();
    }

    // Workspace might contain multiple git repos as subdirs
    let mut repos = Vec::new();
    if let Ok(entries) = std::fs::read_dir(workspace) {
        let mut dirs: Vec<_> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter(|e| e.path().join(".git").exists())
            .collect();
        dirs.sort_by_key(|e| e.file_name());
        for entry in dirs {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(status) = git_status_for_repo(&entry.path(), &name) {
                repos.push(status);
            }
        }
    }
    repos
}

fn git_status_for_repo(repo_path: &std::path::Path, label: &str) -> Option<GitRepoStatus> {
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "-uno"])
        .current_dir(repo_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<GitFileEntry> = stdout
        .lines()
        .filter(|l| l.len() >= 4)
        .map(|line| {
            let xy = &line[..2];
            let path = line[3..].to_string();
            let status = match xy.trim() {
                "M" | "MM" => "M",
                "A" | "AM" => "A",
                "D" => "D",
                "R" | "RM" => "R",
                "??" => "?",
                "UU" | "AA" | "DD" => "C", // conflict
                other => other,
            };
            GitFileEntry {
                status: status.to_string(),
                path,
            }
        })
        .collect();

    Some(GitRepoStatus {
        root: label.to_string(),
        files,
    })
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

    (String::new(), String::new())
}

pub async fn run_tui(continue_last: bool, resume_pick: bool) -> Result<()> {
    // Restore terminal on panic so it doesn't leave raw mode / mouse capture on.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            crossterm::event::DisableMouseCapture
        );
        original_hook(info);
    }));

    let mut terminal = init_terminal()?;
    let mut app = App::new()?;

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

    let mut models_task: Option<JoinHandle<Result<Vec<(String, Vec<ModelInfo>)>>>> = None;

    loop {
        app.poll_git_status();
        terminal.draw(|frame| app.render(frame))?;

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(16)) => {
                while event::poll(Duration::ZERO)? {
                    match event::read()? {
                        Event::Key(key) => {
                            if let Some(action) = app.handle_key(key) {
                                match action {
                                    AppAction::LoadModels { force_refresh } => {
                                        app.loading_models = true;
                                        app.messages.push(ChatMessage::new(
                                            "system",
                                            if force_refresh { "Refreshing models..." } else { "Loading models..." }.to_string(),
                                        ));
                                        models_task = Some(tokio::spawn(fetch_models(force_refresh)));
                                    }
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
            result = async {
                match models_task.as_mut() {
                    Some(task) => task.await,
                    None => pending().await,
                }
            } => {
                models_task = None;
                app.loading_models = false;
                match result {
                    Ok(Ok(grouped_models)) => app.show_model_picker(grouped_models),
                    Ok(Err(error)) => {
                        app.messages.push(ChatMessage::new("system", format!("Failed to list models: {error}")));
                    }
                    Err(error) => {
                        app.messages.push(ChatMessage::new("system", format!("Failed to list models: {error}")));
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
            approval = async {
                if app.pending_approval.is_some() {
                    // Already have one pending, don't poll more
                    pending::<Option<bus::ApprovalRequest>>().await
                } else if let Some(ref harness) = app.harness {
                    harness.approval_channel.recv().await
                } else {
                    pending::<Option<bus::ApprovalRequest>>().await
                }
            } => {
                if let Some(req) = approval {
                    app.pending_approval = Some(req);
                    app.scroll_offset = 0;
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

fn pick_session_interactive(terminal: &mut AppTerminal, app: &App) -> Result<Option<String>> {
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
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(palette::ACCENT))
                .style(Style::default().fg(Color::White));
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
                    let title = if s.title.is_empty() {
                        "(untitled)"
                    } else {
                        &s.title
                    };
                    let text = format!(
                        " {} │ {} │ {} msgs │ {}",
                        &s.id[..12.min(s.id.len())],
                        s.agent_name,
                        s.message_count,
                        title
                    );
                    if i == selected {
                        Line::from(Span::styled(
                            text,
                            Style::default().fg(Color::Black).bg(palette::ACCENT),
                        ))
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
    execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut AppTerminal) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;
    terminal.show_cursor()
}
