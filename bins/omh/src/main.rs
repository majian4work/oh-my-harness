use std::{io, time::Duration};

use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    text::Line,
    widgets::{Block, Borders, Paragraph},
};

type AppTerminal = Terminal<CrosstermBackend<io::Stdout>>;

fn main() -> io::Result<()> {
    let mut terminal = init_terminal()?;
    let run_result = run(&mut terminal);
    let restore_result = restore_terminal(&mut terminal);

    run_result.and(restore_result)
}

fn init_terminal() -> io::Result<AppTerminal> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut AppTerminal) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()
}

fn run(terminal: &mut AppTerminal) -> io::Result<()> {
    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            let content = vec![
                Line::from(format!("{}: {}", agent::name(), agent::status())),
                Line::from("Press q to quit."),
            ];

            frame.render_widget(
                Paragraph::new(content).block(Block::default().title("omh").borders(Borders::ALL)),
                area,
            );
        })?;

        if event::poll(Duration::from_millis(250))?
            && matches!(event::read()?, Event::Key(key) if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc))
        {
            return Ok(());
        }
    }
}
