//! Minimal full-screen log view. Keyboard: j/k move, / edits the query,
//! Enter applies it, t toggles patterns, q quits.

use crate::engine::Engine;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use std::io::{self, Stdout};
use std::time::Duration;

pub fn run(engine: &Engine) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = ui_loop(&mut terminal, engine);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

struct Model {
    query: String,
    draft: String,
    editing: bool,
    selected: usize,
    patterns: bool,
    status: String,
}

fn ui_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, engine: &Engine) -> Result<()> {
    let mut model = Model {
        query: String::new(),
        draft: String::new(),
        editing: false,
        selected: 0,
        patterns: false,
        status: format!("{} events", engine.len()),
    };
    let mut matches = engine.query("").unwrap_or_default();
    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(area);
            let title = if model.editing {
                format!(" query (editing): {} ", model.draft)
            } else {
                format!(
                    " query: {} ",
                    if model.query.is_empty() {
                        "*"
                    } else {
                        &model.query
                    }
                )
            };
            frame.render_widget(
                Paragraph::new(title).block(Block::default().borders(Borders::ALL).title("LogFog")),
                chunks[0],
            );
            if model.patterns {
                let rows = engine.patterns(&model.query, "count").unwrap_or_default();
                let mut lines = Vec::new();
                for row in rows.iter().take(chunks[1].height as usize) {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{:>6}  ", row.count),
                            Style::default().fg(Color::Yellow),
                        ),
                        Span::raw(row.template.clone()),
                    ]));
                }
                if lines.is_empty() {
                    lines.push(Line::from("no templates"));
                }
                frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[1]);
            } else {
                let height = chunks[1].height as usize;
                let start = model.selected.saturating_sub(height / 2);
                let mut lines = Vec::new();
                for (offset, id) in matches.iter().skip(start).take(height).enumerate() {
                    let idx = start + offset;
                    let Some(ev) = engine.preview(*id) else {
                        continue;
                    };
                    let style = if idx == model.selected {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default().fg(level_color(&ev.level))
                    };
                    let ts = ev.ts.unwrap_or_else(|| "-".into());
                    lines.push(Line::from(Span::styled(
                        format!(
                            "{ts} {:<5} {:<12} {}",
                            ev.level,
                            trim(&ev.service, 12),
                            ev.message
                        ),
                        style,
                    )));
                }
                if lines.is_empty() {
                    lines.push(Line::from("no matching lines"));
                }
                frame.render_widget(Paragraph::new(lines), chunks[1]);
            }
            let help = format!(
                "{}  j/k move  / edit  enter apply  t patterns  q quit",
                model.status
            );
            frame.render_widget(Paragraph::new(help), chunks[2]);
        })?;

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            break;
        }
        if model.editing {
            match key.code {
                KeyCode::Esc => model.editing = false,
                KeyCode::Enter => {
                    model.query = model.draft.clone();
                    model.editing = false;
                    matches = engine.query(&model.query).unwrap_or_default();
                    model.selected = 0;
                    model.status = format!("{} matches", matches.len());
                }
                KeyCode::Backspace => {
                    model.draft.pop();
                }
                KeyCode::Char(c) => model.draft.push(c),
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Char('q') => break,
            KeyCode::Char('j') | KeyCode::Down => {
                if model.selected + 1 < matches.len() {
                    model.selected += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                model.selected = model.selected.saturating_sub(1);
            }
            KeyCode::PageDown => {
                model.selected = (model.selected + 20).min(matches.len().saturating_sub(1));
            }
            KeyCode::PageUp => model.selected = model.selected.saturating_sub(20),
            KeyCode::Char('/') => {
                model.editing = true;
                model.draft = model.query.clone();
            }
            KeyCode::Char('t') => model.patterns = !model.patterns,
            _ => {}
        }
    }
    Ok(())
}

fn level_color(level: &str) -> Color {
    match level {
        "error" | "fatal" => Color::Red,
        "warn" => Color::Yellow,
        "info" => Color::Cyan,
        "debug" | "trace" => Color::Green,
        _ => Color::Gray,
    }
}

fn trim(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return format!("{s:<n$}");
    }
    s.chars().take(n).collect()
}
