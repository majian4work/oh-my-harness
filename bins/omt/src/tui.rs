use std::collections::HashMap;
use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

use a2a::TeamMember;

use crate::events::{OmtBus, OmtEvent, RunState};
use crate::planner;
use crate::scheduler;
use crate::state;
use crate::task::{OmtTask, TaskId, TaskState};
use crate::team::TeamManager;

type AppTerminal = Terminal<CrosstermBackend<io::Stdout>>;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

mod palette {
    use ratatui::style::Color;
    pub const BG: Color = Color::Rgb(31, 31, 31);
    pub const SURFACE: Color = Color::Rgb(37, 37, 38);
    pub const BORDER: Color = Color::Rgb(60, 60, 60);
    pub const FG: Color = Color::Rgb(204, 204, 204);
    pub const MUTED: Color = Color::Rgb(110, 118, 129);
    pub const ACCENT: Color = Color::Rgb(79, 193, 255);
    pub const GREEN: Color = Color::Rgb(137, 209, 133);
    pub const YELLOW: Color = Color::Rgb(204, 167, 0);
    pub const RED: Color = Color::Rgb(244, 135, 113);
    pub const CYAN: Color = Color::Rgb(156, 220, 254);
    #[allow(dead_code)]
    pub const MAGENTA: Color = Color::Rgb(197, 134, 192);
}

/// Per-task display state tracked by the TUI.
struct TaskView {
    name: String,
    agent: String,
    state: TaskState,
    attempt: u32,
    max_attempts: u32,
    duration_secs: Option<f64>,
    last_output: Vec<String>,
    error: Option<String>,
    depends_on: Vec<TaskId>,
    token_budget: u64,
    input_tokens: u64,
    output_tokens: u64,
}

struct App {
    /// Input prompt
    input: String,
    cursor_pos: usize,

    /// Input history (newest last)
    history: Vec<String>,
    /// Current position in history (-1 = composing new input)
    history_idx: Option<usize>,
    /// Saved in-progress input when browsing history
    history_draft: String,

    /// Current phase
    phase: AppPhase,

    /// Task views indexed by task_id, insertion-ordered keys
    task_order: Vec<TaskId>,
    tasks: HashMap<TaskId, TaskView>,

    /// Pending plan (for PlanReview phase)
    pending_plan: Vec<OmtTask>,

    /// Currently selected task in the DAG panel
    selected: usize,

    /// Overall run info
    run_id: Option<String>,
    run_state: RunState,

    /// Spinner
    spinner_idx: usize,

    /// Team manager (if A2A server is running)
    team: Option<TeamManager>,
    /// Cached team members (refreshed each tick)
    members: Vec<TeamMember>,
    /// Cached recent runs (refreshed on phase change)
    recent_runs: Vec<RecentRun>,

    should_quit: bool,
}

/// Lightweight summary of a past run for the dashboard.
struct RecentRun {
    run_id: String,
    state: String,
    completed: usize,
    total: usize,
}

#[derive(PartialEq)]
enum AppPhase {
    /// Waiting for user to type a prompt
    Input,
    /// Showing generated plan, waiting for approval
    PlanReview,
    /// Tasks are executing
    Running,
    /// All done
    Finished,
}

impl App {
    fn new(team: Option<TeamManager>) -> Self {
        let history = load_history();
        Self {
            input: String::new(),
            cursor_pos: 0,
            history,
            history_idx: None,
            history_draft: String::new(),
            phase: AppPhase::Input,
            pending_plan: Vec::new(),
            task_order: Vec::new(),
            tasks: HashMap::new(),
            selected: 0,
            run_id: None,
            run_state: RunState::Planned,
            spinner_idx: 0,
            team,
            members: Vec::new(),
            recent_runs: Vec::new(),
            should_quit: false,
        }
    }

    fn refresh_recent_runs(&mut self) {
        self.recent_runs.clear();
        if let Ok(runs) = state::list_runs() {
            for run_id in runs.into_iter().take(5) {
                if let Ok(rs) = state::load_state(&run_id) {
                    let total = rs.graph.tasks.len();
                    let completed = rs
                        .graph
                        .summary()
                        .get(&TaskState::Completed)
                        .copied()
                        .unwrap_or(0);
                    self.recent_runs.push(RecentRun {
                        run_id,
                        state: rs.state.to_string(),
                        completed,
                        total,
                    });
                }
            }
        }
    }

    fn apply_event(&mut self, event: &OmtEvent) {
        match event {
            OmtEvent::TaskStateChanged {
                task_id, new_state, ..
            } => {
                if let Some(tv) = self.tasks.get_mut(task_id) {
                    tv.state = *new_state;
                }
            }
            OmtEvent::TaskRetrying {
                task_id,
                attempt,
                max_attempts,
                error,
                ..
            } => {
                if let Some(tv) = self.tasks.get_mut(task_id) {
                    tv.state = TaskState::Retrying;
                    tv.attempt = *attempt;
                    tv.max_attempts = *max_attempts;
                    tv.error = Some(error.clone());
                }
            }
            OmtEvent::TaskOutput { task_id, text } => {
                if let Some(tv) = self.tasks.get_mut(task_id) {
                    tv.last_output.push(text.clone());
                    if tv.last_output.len() > 50 {
                        tv.last_output.drain(..tv.last_output.len() - 50);
                    }
                }
            }
            OmtEvent::TaskCompleted {
                task_id,
                duration_secs,
                input_tokens,
                output_tokens,
            } => {
                if let Some(tv) = self.tasks.get_mut(task_id) {
                    tv.state = TaskState::Completed;
                    tv.duration_secs = Some(*duration_secs);
                    tv.input_tokens = *input_tokens;
                    tv.output_tokens = *output_tokens;
                }
            }
            OmtEvent::TaskFailed { task_id, error } => {
                if let Some(tv) = self.tasks.get_mut(task_id) {
                    tv.state = TaskState::Failed;
                    tv.error = Some(error.clone());
                }
            }
            OmtEvent::TaskCancelled { task_id, .. } => {
                if let Some(tv) = self.tasks.get_mut(task_id) {
                    tv.state = TaskState::Cancelled;
                }
            }
            OmtEvent::MergeConflict { task_id, files } => {
                if let Some(tv) = self.tasks.get_mut(task_id) {
                    tv.error = Some(format!("merge conflict: {}", files.join(", ")));
                }
            }
            _ => {}
        }
    }

    fn load_plan(&mut self, tasks: &[OmtTask]) {
        self.task_order.clear();
        self.tasks.clear();
        for t in tasks {
            self.task_order.push(t.id.clone());
            self.tasks.insert(
                t.id.clone(),
                TaskView {
                    name: t.name.clone(),
                    agent: t.agent.clone(),
                    state: t.state,
                    attempt: t.attempt_count,
                    max_attempts: t.max_attempts,
                    duration_secs: None,
                    last_output: Vec::new(),
                    error: None,
                    depends_on: t.depends_on.clone(),
                    token_budget: t.token_budget,
                    input_tokens: 0,
                    output_tokens: 0,
                },
            );
        }
        if self.selected >= self.task_order.len() && !self.task_order.is_empty() {
            self.selected = 0;
        }
    }

    fn count_by_state(&self) -> (usize, usize, usize, usize) {
        let mut running = 0;
        let mut pending = 0;
        let mut completed = 0;
        let mut failed = 0;
        for tv in self.tasks.values() {
            match tv.state {
                TaskState::Running => running += 1,
                TaskState::Pending | TaskState::Queued | TaskState::Retrying => pending += 1,
                TaskState::Completed => completed += 1,
                TaskState::Failed | TaskState::Cancelled => failed += 1,
            }
        }
        (running, pending, completed, failed)
    }

    fn selected_task_id(&self) -> Option<&TaskId> {
        self.task_order.get(self.selected)
    }

    #[allow(dead_code)]
    fn is_all_done(&self) -> bool {
        self.tasks.values().all(|t| t.state.is_terminal())
    }
}

pub async fn run_tui(concurrency: usize, team: Option<TeamManager>) -> Result<()> {
    // Set up terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));

    let result = run_app(&mut terminal, concurrency, team).await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    result
}

async fn run_app(
    terminal: &mut AppTerminal,
    concurrency: usize,
    team: Option<TeamManager>,
) -> Result<()> {
    // Spawn periodic stale-member expiry (every 60s, timeout 90s)
    if let Some(ref tm) = team {
        let tm = tm.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                tm.expire_stale(90).await;
            }
        });
    }

    let mut app = App::new(team.clone());
    app.refresh_recent_runs();
    let bus = OmtBus::new();
    let mut event_rx = bus.subscribe();

    // For scheduler control
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut scheduler_handle: Option<tokio::task::JoinHandle<Result<()>>> = None;

    let tick_rate = Duration::from_millis(80);

    loop {
        // Draw
        terminal.draw(|f| draw_ui(f, &app))?;

        // Poll events
        let _timeout = tick_rate;
        let has_event = event::poll(Duration::from_millis(16))?;

        if has_event {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    // skip
                } else if key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    if app.phase == AppPhase::Running {
                        cancel.cancel();
                    }
                    app.should_quit = true;
                } else if key.code == KeyCode::Char('q') && app.phase != AppPhase::Input {
                    if app.phase == AppPhase::Running {
                        cancel.cancel();
                    }
                    app.should_quit = true;
                } else {
                    match &app.phase {
                        AppPhase::Input => match key.code {
                            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                app.should_quit = true;
                            }
                            KeyCode::Enter => {
                                let prompt = app.input.trim().to_string();
                                if !prompt.is_empty() {
                                    // Save to history
                                    app.history.retain(|h| h != &prompt);
                                    app.history.push(prompt.clone());
                                    save_history(&app.history);
                                    app.history_idx = None;
                                    app.history_draft.clear();

                                    app.input.clear();
                                    app.cursor_pos = 0;

                                    // Generate plan
                                    let plan = planner::plan(&prompt).await?;
                                    app.load_plan(&plan);
                                    app.pending_plan = plan;
                                    app.phase = AppPhase::PlanReview;
                                }
                            }
                            KeyCode::Char(c) => {
                                let byte_idx = char_to_byte(&app.input, app.cursor_pos);
                                app.input.insert(byte_idx, c);
                                app.cursor_pos += 1;
                            }
                            KeyCode::Backspace => {
                                if app.cursor_pos > 0 {
                                    app.cursor_pos -= 1;
                                    let byte_idx = char_to_byte(&app.input, app.cursor_pos);
                                    app.input.remove(byte_idx);
                                }
                            }
                            KeyCode::Left => {
                                app.cursor_pos = app.cursor_pos.saturating_sub(1);
                            }
                            KeyCode::Right => {
                                if app.cursor_pos < app.input.chars().count() {
                                    app.cursor_pos += 1;
                                }
                            }
                            KeyCode::Up => {
                                if !app.history.is_empty() {
                                    let new_idx = match app.history_idx {
                                        None => {
                                            app.history_draft = app.input.clone();
                                            app.history.len() - 1
                                        }
                                        Some(i) => i.saturating_sub(1),
                                    };
                                    app.history_idx = Some(new_idx);
                                    app.input = app.history[new_idx].clone();
                                    app.cursor_pos = app.input.chars().count();
                                }
                            }
                            KeyCode::Down => {
                                if let Some(i) = app.history_idx {
                                    if i + 1 < app.history.len() {
                                        let new_idx = i + 1;
                                        app.history_idx = Some(new_idx);
                                        app.input = app.history[new_idx].clone();
                                        app.cursor_pos = app.input.chars().count();
                                    } else {
                                        // Back to draft
                                        app.history_idx = None;
                                        app.input = std::mem::take(&mut app.history_draft);
                                        app.cursor_pos = app.input.chars().count();
                                    }
                                }
                            }
                            _ => {}
                        },
                        AppPhase::PlanReview => match key.code {
                            KeyCode::Enter | KeyCode::Char('y') => {
                                let plan = std::mem::take(&mut app.pending_plan);
                                let run_id = state::create_run(&plan)?;
                                app.run_id = Some(run_id.clone());
                                app.phase = AppPhase::Running;
                                app.run_state = RunState::Running;

                                let config = scheduler::SchedulerConfig {
                                    max_concurrent: concurrency,
                                    team: team.clone(),
                                };
                                let sched_cancel = cancel.clone();
                                let sched_bus = bus.clone();
                                scheduler_handle = Some(tokio::spawn(async move {
                                    scheduler::run(run_id, config, sched_cancel, sched_bus).await
                                }));
                            }
                            KeyCode::Char('n') | KeyCode::Esc => {
                                app.phase = AppPhase::Input;
                                app.task_order.clear();
                                app.tasks.clear();
                                app.pending_plan.clear();
                            }
                            KeyCode::Up => {
                                app.selected = app.selected.saturating_sub(1);
                            }
                            KeyCode::Down => {
                                if app.selected + 1 < app.task_order.len() {
                                    app.selected += 1;
                                }
                            }
                            _ => {}
                        },
                        AppPhase::Running => match key.code {
                            KeyCode::Up => {
                                app.selected = app.selected.saturating_sub(1);
                            }
                            KeyCode::Down => {
                                if app.selected + 1 < app.task_order.len() {
                                    app.selected += 1;
                                }
                            }
                            _ => {}
                        },
                        AppPhase::Finished => match key.code {
                            KeyCode::Enter => {
                                // Return to input for a new run
                                app.phase = AppPhase::Input;
                                app.task_order.clear();
                                app.tasks.clear();
                                app.run_id = None;
                                app.refresh_recent_runs();
                            }
                            KeyCode::Esc | KeyCode::Char('q') => {
                                app.should_quit = true;
                            }
                            _ => {}
                        },
                    }
                }
            }
        }

        // Drain bus events
        while let Ok(evt) = event_rx.try_recv() {
            app.apply_event(&evt);
        }

        // Check if scheduler finished
        if let Some(ref handle) = scheduler_handle {
            if handle.is_finished() {
                if let Some(h) = scheduler_handle.take() {
                    let _ = h.await;
                }
                app.phase = AppPhase::Finished;
                app.run_state = RunState::Finished;
            }
        }

        // Update spinner & team members
        app.spinner_idx = app.spinner_idx.wrapping_add(1);
        if let Some(ref tm) = app.team {
            let status = tm.status().await;
            app.members = status.members;
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

/// Convert a char-index to a byte-index in a UTF-8 string.
fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(byte, _)| byte)
        .unwrap_or(s.len())
}

const MAX_HISTORY: usize = 200;

fn history_path() -> std::path::PathBuf {
    state::runs_dir()
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("history.txt")
}

fn load_history() -> Vec<String> {
    let path = history_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => content
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn save_history(history: &[String]) {
    let path = history_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tail = if history.len() > MAX_HISTORY {
        &history[history.len() - MAX_HISTORY..]
    } else {
        history
    };
    let content = tail.join("\n") + "\n";
    let _ = std::fs::write(&path, content);
}

fn draw_ui(f: &mut Frame, app: &App) {
    let size = f.area();

    // Background
    let bg = Block::default().style(Style::default().bg(palette::BG));
    f.render_widget(bg, size);

    // Main layout: header (3) | body | footer (3)
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(10),
        Constraint::Length(3),
    ])
    .split(size);

    draw_header(f, app, chunks[0]);

    match &app.phase {
        AppPhase::Input => {
            let body =
                Layout::vertical([Constraint::Length(3), Constraint::Min(5)]).split(chunks[1]);
            draw_input_prompt(f, app, body[0]);
            draw_dashboard(f, app, body[1]);
            draw_input_footer(f, chunks[2]);
        }
        AppPhase::PlanReview => {
            let body = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(chunks[1]);
            draw_task_list(f, app, body[0]);
            draw_task_detail(f, app, body[1]);
            draw_plan_footer(f, chunks[2]);
        }
        AppPhase::Running | AppPhase::Finished => {
            let body = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
                .split(chunks[1]);
            draw_task_list(f, app, body[0]);
            draw_task_detail(f, app, body[1]);
            if app.phase == AppPhase::Finished {
                draw_finished_footer(f, app, chunks[2]);
            } else {
                draw_running_footer(f, chunks[2]);
            }
        }
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let (running, pending, completed, failed) = app.count_by_state();
    let total = app.tasks.len();

    let title = match &app.phase {
        AppPhase::Input => {
            if app.team.is_some() && app.members.is_empty() {
                " omt — oh-my-team  ⚠ no agents connected ".to_string()
            } else if !app.members.is_empty() {
                format!(" omt — oh-my-team  {} agent(s) online ", app.members.len())
            } else {
                " omt — oh-my-team ".to_string()
            }
        }
        AppPhase::PlanReview => format!(" omt — review plan ({total} tasks) "),
        AppPhase::Running => {
            let spinner = SPINNER_FRAMES[app.spinner_idx % SPINNER_FRAMES.len()];
            format!(" omt {spinner} {running} running, {pending} pending, {completed} completed ")
        }
        AppPhase::Finished => {
            format!(" omt ✓ {completed}/{total} completed, {failed} failed ")
        }
    };

    let border_color =
        if app.phase == AppPhase::Input && app.team.is_some() && app.members.is_empty() {
            palette::YELLOW
        } else {
            palette::ACCENT
        };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(palette::SURFACE));

    let run_info = if let Some(ref id) = app.run_id {
        format!("run: {}", &id[..id.len().min(12)])
    } else {
        String::new()
    };

    let p = Paragraph::new(run_info)
        .style(Style::default().fg(palette::MUTED))
        .block(block)
        .alignment(Alignment::Right);

    f.render_widget(p, area);
}

fn draw_input_prompt(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Describe your task ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::BORDER))
        .style(Style::default().bg(palette::SURFACE));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let input_area = Rect {
        x: inner.x + 1,
        y: inner.y,
        width: inner.width.saturating_sub(2),
        height: 1,
    };

    let display = if app.input.is_empty() {
        vec![
            Span::styled("❯ ", Style::default().fg(palette::ACCENT)),
            Span::styled(
                "Enter a prompt to decompose into parallel tasks...",
                Style::default().fg(palette::MUTED),
            ),
        ]
    } else {
        vec![
            Span::styled("❯ ", Style::default().fg(palette::ACCENT)),
            Span::raw(&app.input),
        ]
    };

    f.render_widget(Paragraph::new(Line::from(display)), input_area);

    // Cursor
    if app.phase == AppPhase::Input {
        let prefix: String = app.input.chars().take(app.cursor_pos).collect();
        let display_offset = prefix.width() as u16;
        f.set_cursor_position((input_area.x + 2 + display_offset, input_area.y));
    }
}

fn draw_dashboard(f: &mut Frame, app: &App, area: Rect) {
    // Split dashboard: left = team members, right = recent runs
    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);

    // ── Team Members ──
    let team_block = Block::default()
        .title(" Team Members ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::BORDER))
        .style(Style::default().bg(palette::SURFACE));

    let team_inner = team_block.inner(cols[0]);
    f.render_widget(team_block, cols[0]);

    if app.members.is_empty() {
        let hint = if app.team.is_some() {
            "Waiting for agents to connect...\n\nRun `omh` instances to join this team."
        } else {
            "A2A server not started."
        };
        let p = Paragraph::new(hint)
            .style(Style::default().fg(palette::MUTED))
            .wrap(Wrap { trim: false });
        f.render_widget(p, team_inner);
    } else {
        let mut lines = Vec::new();
        for m in &app.members {
            let status_icon = match m.status {
                a2a::MemberStatus::Active => ("●", palette::GREEN),
                a2a::MemberStatus::Draining => ("◐", palette::YELLOW),
                a2a::MemberStatus::Offline => ("○", palette::RED),
            };
            let load = format!("{}/{}", m.active_tasks, m.capacity);
            lines.push(Line::from(vec![
                Span::styled(status_icon.0, Style::default().fg(status_icon.1)),
                Span::raw(" "),
                Span::styled(&m.card.name, Style::default().fg(palette::FG)),
                Span::styled(format!(" [{}]", m.role), Style::default().fg(palette::CYAN)),
                Span::styled(format!("  {load}"), Style::default().fg(palette::MUTED)),
                Span::styled(
                    format!("  {}", &m.endpoint),
                    Style::default().fg(palette::MUTED),
                ),
            ]));
        }
        let p = Paragraph::new(lines);
        f.render_widget(p, team_inner);
    }

    // ── Recent Runs ──
    let runs_block = Block::default()
        .title(" Recent Runs ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::BORDER))
        .style(Style::default().bg(palette::SURFACE));

    let runs_inner = runs_block.inner(cols[1]);
    f.render_widget(runs_block, cols[1]);

    if app.recent_runs.is_empty() {
        let p = Paragraph::new("No runs yet.").style(Style::default().fg(palette::MUTED));
        f.render_widget(p, runs_inner);
    } else {
        let mut lines = Vec::new();
        for r in &app.recent_runs {
            let state_color = match r.state.as_str() {
                "finished" => palette::GREEN,
                "running" => palette::ACCENT,
                "failed" => palette::RED,
                "cancelled" => palette::MUTED,
                _ => palette::YELLOW,
            };
            let short_id = &r.run_id[..r.run_id.len().min(13)];
            lines.push(Line::from(vec![
                Span::styled(short_id, Style::default().fg(palette::FG)),
                Span::raw("  "),
                Span::styled(&r.state, Style::default().fg(state_color)),
                Span::styled(
                    format!("  {}/{}", r.completed, r.total),
                    Style::default().fg(palette::MUTED),
                ),
            ]));
        }
        let p = Paragraph::new(lines);
        f.render_widget(p, runs_inner);
    }
}

fn draw_task_list(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Task DAG ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::BORDER))
        .style(Style::default().bg(palette::SURFACE));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = Vec::new();
    for (i, task_id) in app.task_order.iter().enumerate() {
        let tv = match app.tasks.get(task_id) {
            Some(t) => t,
            None => continue,
        };

        let is_selected = i == app.selected;
        let prefix = if is_selected { "▸ " } else { "  " };

        let (icon, icon_color) = match tv.state {
            TaskState::Completed => ("✓", palette::GREEN),
            TaskState::Running => {
                let s = SPINNER_FRAMES[app.spinner_idx % SPINNER_FRAMES.len()];
                (s, palette::ACCENT)
            }
            TaskState::Retrying => ("↻", palette::YELLOW),
            TaskState::Failed => ("✗", palette::RED),
            TaskState::Cancelled => ("⊘", palette::MUTED),
            TaskState::Pending | TaskState::Queued => ("○", palette::MUTED),
        };

        let name_style = if is_selected {
            Style::default()
                .fg(palette::FG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette::FG)
        };

        let duration = tv
            .duration_secs
            .map(|d| format!(" {d:.1}s"))
            .unwrap_or_default();

        let retry_info = if tv.attempt > 1 {
            format!(" [{}/{}]", tv.attempt, tv.max_attempts)
        } else {
            String::new()
        };

        // Show dependency arrows
        let dep_indicator = if !tv.depends_on.is_empty() {
            let dep_names: Vec<&str> = tv
                .depends_on
                .iter()
                .filter_map(|dep_id| app.tasks.get(dep_id).map(|t| t.name.as_str()))
                .collect();
            if dep_names.is_empty() {
                String::new()
            } else {
                format!(" ← {}", dep_names.join(", "))
            }
        } else {
            String::new()
        };

        let line = Line::from(vec![
            Span::raw(prefix),
            Span::styled(icon, Style::default().fg(icon_color)),
            Span::raw(" "),
            Span::styled(&tv.name, name_style),
            Span::styled(
                format!(" [{}]", tv.agent),
                Style::default().fg(palette::MUTED),
            ),
            Span::styled(retry_info, Style::default().fg(palette::YELLOW)),
            Span::styled(duration, Style::default().fg(palette::CYAN)),
            Span::styled(dep_indicator, Style::default().fg(palette::MUTED)),
        ]);

        lines.push(line);
    }

    let p = Paragraph::new(lines);
    f.render_widget(p, inner);
}

fn draw_task_detail(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Task Detail ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::BORDER))
        .style(Style::default().bg(palette::SURFACE));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let task_id = match app.selected_task_id() {
        Some(id) => id.clone(),
        None => {
            let hint =
                Paragraph::new("No task selected").style(Style::default().fg(palette::MUTED));
            f.render_widget(hint, inner);
            return;
        }
    };

    let tv = match app.tasks.get(&task_id) {
        Some(t) => t,
        None => return,
    };

    let mut lines = Vec::new();

    // Name & status
    let (status_text, status_color) = match tv.state {
        TaskState::Completed => ("completed", palette::GREEN),
        TaskState::Running => ("running", palette::ACCENT),
        TaskState::Retrying => ("retrying", palette::YELLOW),
        TaskState::Failed => ("failed", palette::RED),
        TaskState::Cancelled => ("cancelled", palette::MUTED),
        TaskState::Pending => ("pending", palette::MUTED),
        TaskState::Queued => ("queued", palette::MUTED),
    };

    lines.push(Line::from(vec![
        Span::styled(
            &tv.name,
            Style::default()
                .fg(palette::FG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            status_text,
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    lines.push(Line::from(vec![
        Span::styled("Agent: ", Style::default().fg(palette::MUTED)),
        Span::styled(&tv.agent, Style::default().fg(palette::CYAN)),
    ]));

    if let Some(d) = tv.duration_secs {
        lines.push(Line::from(vec![
            Span::styled("Duration: ", Style::default().fg(palette::MUTED)),
            Span::styled(format!("{d:.1}s"), Style::default().fg(palette::FG)),
        ]));
    }

    // Token usage
    if tv.input_tokens > 0 || tv.output_tokens > 0 {
        let total = tv.input_tokens + tv.output_tokens;
        let budget_info = if tv.token_budget > 0 {
            format!(" / {} budget", tv.token_budget)
        } else {
            String::new()
        };
        lines.push(Line::from(vec![
            Span::styled("Tokens: ", Style::default().fg(palette::MUTED)),
            Span::styled(
                format!(
                    "{}in + {}out = {}{}",
                    tv.input_tokens, tv.output_tokens, total, budget_info
                ),
                Style::default().fg(if tv.token_budget > 0 && total > tv.token_budget {
                    palette::RED
                } else {
                    palette::FG
                }),
            ),
        ]));
    } else if tv.token_budget > 0 {
        lines.push(Line::from(vec![
            Span::styled("Token budget: ", Style::default().fg(palette::MUTED)),
            Span::styled(
                format!("{}", tv.token_budget),
                Style::default().fg(palette::FG),
            ),
        ]));
    }

    if tv.attempt > 0 {
        lines.push(Line::from(vec![
            Span::styled("Attempts: ", Style::default().fg(palette::MUTED)),
            Span::styled(
                format!("{}/{}", tv.attempt, tv.max_attempts),
                Style::default().fg(if tv.attempt > 1 {
                    palette::YELLOW
                } else {
                    palette::FG
                }),
            ),
        ]));
    }

    if let Some(ref err) = tv.error {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Error:",
            Style::default()
                .fg(palette::RED)
                .add_modifier(Modifier::BOLD),
        )));
        for err_line in err.lines().take(5) {
            lines.push(Line::from(Span::styled(
                err_line,
                Style::default().fg(palette::RED),
            )));
        }
    }

    // Live output section
    if !tv.last_output.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "─── Live Output ───",
            Style::default().fg(palette::BORDER),
        )));

        let max_output_lines = inner.height.saturating_sub(lines.len() as u16 + 1) as usize;
        let start = tv.last_output.len().saturating_sub(max_output_lines);
        for line in &tv.last_output[start..] {
            let truncated = if line.len() > inner.width as usize {
                format!("{}…", &line[..inner.width as usize - 1])
            } else {
                line.clone()
            };
            lines.push(Line::from(Span::styled(
                truncated,
                Style::default().fg(palette::MUTED),
            )));
        }
    }

    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}

fn draw_input_footer(f: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::BORDER))
        .style(Style::default().bg(palette::SURFACE));

    let p = Paragraph::new(Line::from(vec![
        Span::styled(
            " Enter ",
            Style::default()
                .fg(palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("submit  ", Style::default().fg(palette::MUTED)),
        Span::styled(
            " Ctrl+D ",
            Style::default()
                .fg(palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("quit", Style::default().fg(palette::MUTED)),
    ]))
    .block(block);

    f.render_widget(p, area);
}

fn draw_plan_footer(f: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::BORDER))
        .style(Style::default().bg(palette::SURFACE));

    let p = Paragraph::new(Line::from(vec![
        Span::styled(
            " Enter/y ",
            Style::default()
                .fg(palette::GREEN)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("approve  ", Style::default().fg(palette::MUTED)),
        Span::styled(
            " n/Esc ",
            Style::default()
                .fg(palette::RED)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("reject  ", Style::default().fg(palette::MUTED)),
        Span::styled(
            " ↑↓ ",
            Style::default()
                .fg(palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("navigate", Style::default().fg(palette::MUTED)),
    ]))
    .block(block);

    f.render_widget(p, area);
}

fn draw_running_footer(f: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::BORDER))
        .style(Style::default().bg(palette::SURFACE));

    let p = Paragraph::new(Line::from(vec![
        Span::styled(
            " ↑↓ ",
            Style::default()
                .fg(palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("navigate  ", Style::default().fg(palette::MUTED)),
        Span::styled(
            " Ctrl+C ",
            Style::default()
                .fg(palette::RED)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("cancel  ", Style::default().fg(palette::MUTED)),
        Span::styled(
            " q ",
            Style::default()
                .fg(palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("quit", Style::default().fg(palette::MUTED)),
    ]))
    .block(block);

    f.render_widget(p, area);
}

fn draw_finished_footer(f: &mut Frame, app: &App, area: Rect) {
    let (_, _, completed, failed) = app.count_by_state();
    let total = app.tasks.len();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if failed > 0 {
            palette::RED
        } else {
            palette::GREEN
        }))
        .style(Style::default().bg(palette::SURFACE));

    let p = Paragraph::new(Line::from(vec![
        Span::styled(
            format!(" Done: {completed}/{total} completed, {failed} failed  "),
            Style::default().fg(if failed > 0 {
                palette::YELLOW
            } else {
                palette::GREEN
            }),
        ),
        Span::styled(
            " Enter ",
            Style::default()
                .fg(palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("new run  ", Style::default().fg(palette::MUTED)),
        Span::styled(
            " q ",
            Style::default()
                .fg(palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("quit", Style::default().fg(palette::MUTED)),
    ]))
    .block(block);

    f.render_widget(p, area);
}
