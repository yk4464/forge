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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const ACCENT: Color = Color::LightBlue;
const DIM: Color = Color::DarkGray;

/// One rendered transcript row: the styled line plus its wrapped height.
/// Heights are computed by our own wrap (below) so the total matches what
/// `Paragraph` renders — its `scroll` unit is wrapped rows, not logical
/// lines, and mixing the two broke auto-follow and scrolling.
struct Row {
    line: RLine<'static>,
    height: u16,
}

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
    // Text area inside the rounded border.
    let tw = area.width.saturating_sub(2) as usize;
    let mut rows: Vec<Row> = Vec::new();
    push_wrapped(&mut rows, String::new(), Style::default(), tw); // breathing room under the title
    for line in &app.lines {
        render_line(line, &mut rows, tw);
        // Blank line between top-level entries for readability.
        if matches!(line, Line::User(_) | Line::Assistant(_) | Line::Tool { .. }) {
            push_wrapped(&mut rows, String::new(), Style::default(), tw);
        }
    }
    // Live streaming area.
    if !app.streaming_reasoning.is_empty() {
        for para in app.streaming_reasoning.split('\n') {
            push_wrapped(
                &mut rows,
                format!("  · {para}"),
                Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
                tw,
            );
        }
    }
    if !app.streaming_assistant.is_empty() {
        push_wrapped(&mut rows, String::new(), Style::default(), tw);
        push_wrapped(
            &mut rows,
            app.streaming_assistant.clone(),
            Style::default().fg(Color::White),
            tw,
        );
    }
    if !app.open_tools.is_empty() {
        for (_call_id, (cmd, out)) in &app.open_tools {
            push_wrapped(
                &mut rows,
                format!("  ⚙ {cmd}"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                tw,
            );
            let tail: String = out.chars().rev().take(240).collect::<Vec<_>>()
                .into_iter().rev().collect();
            push_wrapped(
                &mut rows,
                format!("    {tail}"),
                Style::default().fg(ACCENT),
                tw,
            );
        }
    }

    let total: u16 = rows
        .iter()
        .map(|r| r.height as usize)
        .sum::<usize>()
        .min(u16::MAX as usize) as u16;
    let visible = area.height.saturating_sub(2);
    let scroll = scroll_offset(total, visible, app.scroll_from_bottom);

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
    let lines: Vec<RLine<'static>> = rows.into_iter().map(|r| r.line).collect();
    let para = Paragraph::new(lines)
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

fn render_line(line: &Line, rows: &mut Vec<Row>, width: usize) {
    match line {
        Line::User(text) => {
            for (i, l) in text.split('\n').enumerate() {
                if i == 0 {
                    push_wrapped(
                        rows,
                        format!("❯ {l}"),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                        width,
                    );
                } else {
                    push_wrapped(rows, format!("  {l}"), Style::default().fg(Color::Green), width);
                }
            }
        }
        Line::Assistant(text) => {
            for l in text.split('\n') {
                push_wrapped(rows, l.to_string(), Style::default().fg(Color::White), width);
            }
        }
        Line::Reasoning(text) => {
            push_wrapped(
                rows,
                "  ··· thought".to_string(),
                Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
                width,
            );
            for l in text.split('\n') {
                push_wrapped(rows, format!("  · {l}"), Style::default().fg(DIM), width);
            }
        }
        Line::Tool { command, output, done } => {
            push_wrapped(
                rows,
                format!("⚙ {command}{}", if *done { "" } else { " …" }),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                width,
            );
            let lines: Vec<&str> = output.split('\n').collect();
            let show = lines.len().min(24);
            for l in lines.iter().take(show) {
                push_wrapped(rows, format!("  {l}"), Style::default().fg(Color::Gray), width);
            }
            if lines.len() > show {
                push_wrapped(
                    rows,
                    format!("  … {} more lines", lines.len() - show),
                    Style::default().fg(DIM),
                    width,
                );
            }
        }
        Line::System(text) => {
            push_wrapped(rows, format!("— {text}"), Style::default().fg(ACCENT), width);
        }
        Line::Warn(text) => {
            push_wrapped(rows, format!("⚠ {text}"), Style::default().fg(Color::Yellow), width);
        }
        Line::Error(text) => {
            push_wrapped(rows, format!("✗ {text}"), Style::default().fg(Color::Red), width);
        }
    }
}

fn push_wrapped(rows: &mut Vec<Row>, text: String, style: Style, width: usize) {
    for seg in wrap_width(&text, width) {
        rows.push(Row {
            line: RLine::from(Span::styled(seg, style)),
            height: 1,
        });
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
    // Cursor position: end of input. The prompt label is measured in
    // display columns, not bytes (the ❯/● glyphs are multi-byte).
    let x = area.x + 1 + label.width() as u16 + input_width(&app.input) as u16;
    let y = area.y + 1;
    if x < area.x + area.width - 1 {
        f.set_cursor_position((x, y));
    }
}

/// Display-column width of the input buffer (CJK chars are two columns).
fn input_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
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

/// Greedy word wrap measured in display columns (unicode width), with hard
/// breaks for words wider than a line (CJK runs, URLs, no-space output).
/// The old byte-based wrap mis-measured CJK (3 bytes / 2 columns) and let
/// lines overflow the terminal.
fn wrap_width(text: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        let mut line_w = 0usize;
        for word in para.split(' ') {
            let ww = UnicodeWidthStr::width(word);
            if line_w > 0 && line_w + 1 + ww > width {
                out.push(std::mem::take(&mut line));
                line_w = 0;
            }
            if line_w > 0 {
                line.push(' ');
                line_w += 1;
            }
            for ch in word.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if line_w > 0 && line_w + cw > width {
                    out.push(std::mem::take(&mut line));
                    line_w = 0;
                }
                line.push(ch);
                line_w += cw;
            }
        }
        out.push(line);
    }
    out
}

/// Bottom-anchored scroll offset for `Paragraph::scroll`. `from_bottom` is
/// how many rows the view sits above the newest content (0 = follow the
/// tail); 0 must always land exactly on the bottom.
fn scroll_offset(total: u16, visible: u16, from_bottom: u16) -> u16 {
    total
        .saturating_sub(visible)
        .saturating_sub(from_bottom)
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
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(3);
            false
        }
        KeyCode::Down => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(3);
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_ascii_respects_width() {
        let out = wrap_width("aaaa bbbb cccc dddd", 10);
        assert!(out.iter().all(|l| UnicodeWidthStr::width(l.as_str()) <= 10));
        assert_eq!(out.join(" "), "aaaa bbbb cccc dddd");
    }

    #[test]
    fn wrap_cjk_counts_columns_not_bytes() {
        // 20 CJK chars = 40 columns; a 12-column line must hard-break them.
        let text = "汉".repeat(20);
        let out = wrap_width(&text, 12);
        assert!(out.len() >= 3, "expected hard breaks, got {} lines", out.len());
        assert!(out.iter().all(|l| UnicodeWidthStr::width(l.as_str()) <= 12));
        assert_eq!(out.concat(), text);
    }

    #[test]
    fn wrap_long_unbroken_ascii_hard_breaks() {
        let out = wrap_width(&"x".repeat(25), 10);
        assert_eq!(out.len(), 3);
        assert_eq!(out.concat(), "x".repeat(25));
    }

    #[test]
    fn scroll_offset_pins_bottom_at_zero() {
        // 100 rows, 10 visible: 0 = exactly at the bottom.
        assert_eq!(scroll_offset(100, 10, 0), 90);
        // Scrolling up 3 moves exactly 3 rows from the bottom.
        assert_eq!(scroll_offset(100, 10, 3), 87);
        // Far beyond the top clamps at 0 (top of content).
        assert_eq!(scroll_offset(100, 10, 500), 0);
        // Content shorter than the viewport stays at the top.
        assert_eq!(scroll_offset(5, 10, 0), 0);
    }
}
