use crate::app::{App, Line};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    symbols::border,
    text::{Line as RLine, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

const ACCENT: Color = Color::LightBlue;
const DIM: Color = Color::DarkGray;

/// Draw the whole UI: transcript + input + status bar.
pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(f.area());

    draw_transcript(f, app, chunks[0]);
    draw_input(f, app, chunks[1]);
    draw_status(f, app, chunks[2]);
}

fn draw_transcript(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let mut spans: Vec<RLine> = Vec::new();
    spans.push(RLine::from("")); // breathing room under the title
    for line in &app.lines {
        render_line(line, &mut spans);
        // Blank line between top-level entries for readability.
        if matches!(line, Line::User(_) | Line::Assistant(_) | Line::Tool { .. }) {
            spans.push(RLine::from(""));
        }
    }
    // Live streaming area.
    if !app.streaming_reasoning.is_empty() {
        for l in wrap_text(&app.streaming_reasoning, area.width.saturating_sub(4) as usize) {
            spans.push(RLine::from(Span::styled(
                format!("  · {l}"),
                Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
            )));
        }
    }
    if !app.streaming_assistant.is_empty() {
        spans.push(RLine::from(""));
        for l in wrap_text(&app.streaming_assistant, area.width.saturating_sub(2) as usize) {
            spans.push(RLine::from(Span::styled(
                l,
                Style::default().fg(Color::White),
            )));
        }
    }
    if !app.open_tools.is_empty() {
        for (call_id, (cmd, out)) in &app.open_tools {
            spans.push(RLine::from(Span::styled(
                format!("  ⚙ {cmd}"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )));
            let tail: String = out.chars().rev().take(240).collect::<Vec<_>>()
                .into_iter().rev().collect();
            for l in wrap_text(&tail, area.width.saturating_sub(6) as usize) {
                spans.push(RLine::from(Span::styled(
                    format!("    {l}"),
                    Style::default().fg(ACCENT),
                )));
            }
            let _ = call_id;
        }
    }

    // Auto-scroll to bottom unless the user scrolled up.
    let total = spans.len() as u16;
    let visible = area.height.saturating_sub(2);
    let scroll = if app.scroll == 0 {
        total.saturating_sub(visible)
    } else {
        app.scroll.min(total.saturating_sub(1))
    };

    let title = vec![
        Span::styled(
            " ⚒ forge ",
            Style::default()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {} ", app.session_title),
            Style::default().fg(DIM),
        ),
    ];
    let para = Paragraph::new(spans)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_set(border::ROUNDED)
                .border_style(Style::default().fg(DIM))
                .title(title),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(para, area);
}

fn render_line(line: &Line, out: &mut Vec<RLine<'static>>) {
    match line {
        Line::User(text) => {
            for (i, l) in text.split('\n').enumerate() {
                if i == 0 {
                    out.push(RLine::from(Span::styled(
                        format!("❯ {l}"),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    )));
                } else {
                    out.push(RLine::from(Span::styled(
                        format!("  {l}"),
                        Style::default().fg(Color::Green),
                    )));
                }
            }
        }
        Line::Assistant(text) => {
            for l in text.split('\n') {
                out.push(RLine::from(Span::styled(
                    l.to_string(),
                    Style::default().fg(Color::White),
                )));
            }
        }
        Line::Reasoning(text) => {
            out.push(RLine::from(Span::styled(
                "  ··· thought".to_string(),
                Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
            )));
            for l in text.split('\n') {
                out.push(RLine::from(Span::styled(
                    format!("  · {l}"),
                    Style::default().fg(DIM),
                )));
            }
        }
        Line::Tool { command, output, done } => {
            out.push(RLine::from(Span::styled(
                format!("⚙ {command}{}", if *done { "" } else { " …" }),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )));
            let lines: Vec<&str> = output.split('\n').collect();
            let show = lines.len().min(24);
            for l in lines.iter().take(show) {
                out.push(RLine::from(Span::styled(
                    format!("  {l}"),
                    Style::default().fg(Color::Gray),
                )));
            }
            if lines.len() > show {
                out.push(RLine::from(Span::styled(
                    format!("  … {} more lines", lines.len() - show),
                    Style::default().fg(DIM),
                )));
            }
        }
        Line::System(text) => {
            out.push(RLine::from(Span::styled(
                format!("— {text}"),
                Style::default().fg(ACCENT),
            )));
        }
        Line::Warn(text) => {
            out.push(RLine::from(Span::styled(
                format!("⚠ {text}"),
                Style::default().fg(Color::Yellow),
            )));
        }
        Line::Error(text) => {
            out.push(RLine::from(Span::styled(
                format!("✗ {text}"),
                Style::default().fg(Color::Red),
            )));
        }
    }
}

fn draw_input(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let (label, color) = if app.busy {
        (" ● ", Color::Yellow)
    } else {
        (" ❯ ", Color::Green)
    };
    let title = Span::styled(label, Style::default().fg(color).add_modifier(Modifier::BOLD));
    let para = Paragraph::new(RLine::from(vec![
        title,
        Span::raw(app.input.clone()),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_set(border::ROUNDED)
            .border_style(Style::default().fg(DIM))
            .title(Span::styled(
                " input ",
                Style::default().fg(DIM),
            )),
    );
    f.render_widget(para, area);
    // Cursor position: end of input.
    let x = area.x + 1 + label.len() as u16 + app.input.chars().count() as u16;
    let y = area.y + 1;
    if x < area.x + area.width - 1 {
        f.set_cursor_position((x, y));
    }
}

fn draw_status(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let status_color = if app.status.contains("error") {
        Color::Red
    } else if app.busy {
        Color::Yellow
    } else {
        Color::Green
    };
    let line = RLine::from(vec![
        Span::styled(
            format!(" {} ", app.status),
            Style::default().fg(Color::Black).bg(status_color),
        ),
        Span::styled("  /new ", Style::default().fg(ACCENT)),
        Span::styled("/resume ", Style::default().fg(ACCENT)),
        Span::styled("/compact ", Style::default().fg(ACCENT)),
        Span::styled("/exit", Style::default().fg(ACCENT)),
        Span::styled("  ↑↓ scroll", Style::default().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

/// Very rough word wrap for streaming display.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(20) as usize;
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            if line.len() + word.len() + 1 > width {
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                line.push_str(word);
            } else if line.is_empty() {
                line.push_str(word);
            } else {
                line.push(' ');
                line.push_str(word);
            }
        }
        out.push(line);
    }
    out
}

/// Handle a key event; returns true when the input should be submitted.
pub fn on_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Enter => true,
        KeyCode::Backspace => {
            app.input.pop();
            false
        }
        KeyCode::Char(c) => {
            if c == 'c' && key.modifiers.contains(KeyModifiers::CONTROL) {
                app.should_quit = true;
                return false;
            }
            app.input.push(c);
            false
        }
        KeyCode::Up => {
            app.scroll = app.scroll.saturating_add(3);
            false
        }
        KeyCode::Down => {
            app.scroll = app.scroll.saturating_sub(3);
            false
        }
        _ => false,
    }
}
