//! The screen (ADR 0009): threads on the left, the conversation in the middle, each
//! participant's context on the right, the input and a status line at the bottom. Colours
//! come from the 16-colour palette, so light and dark terminals both work.

use pulldown_cmark::{CodeBlockKind, Event as Md, HeadingLevel, Parser, Tag, TagEnd};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use roundtable::agent::Outcome;
use roundtable::app::{App, ChatLine, Focus, LineKind};

/// Side panes only where the chat keeps at least 60 columns (ADR 0009).
use roundtable::app::WIDE;
const THREADS_WIDTH: u16 = 26;
const CONTEXT_WIDTH: u16 = 30;
/// The input box's most rows of text; a longer input shows its last rows.
const INPUT_MAX_ROWS: usize = 8;

pub fn draw(frame: &mut Frame, app: &App) {
    // The input wraps to the box and grows with its rows, up to a few; past that it shows
    // the last ones, where the cursor is. The box spans the whole width, inside its border.
    let input_lines = input_lines(&app.input, frame.area().width.saturating_sub(2) as usize);
    let input_rows = input_lines.len().clamp(1, INPUT_MAX_ROWS) as u16;
    let completions = app.completions();
    let popup_rows = completions.len().min(6) as u16;
    let [main, popup_area, input_area, status_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(if popup_rows > 0 { popup_rows + 2 } else { 0 }),
        Constraint::Length(input_rows + 2),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let wide = main.width >= WIDE;
    let threads_w = if wide && app.show_threads {
        THREADS_WIDTH
    } else {
        0
    };
    let context_w = if wide && app.show_context {
        CONTEXT_WIDTH
    } else {
        0
    };
    let [threads_area, chat_area, context_area] = Layout::horizontal([
        Constraint::Length(threads_w),
        Constraint::Min(60.min(main.width)),
        Constraint::Length(context_w),
    ])
    .areas(main);

    if threads_w > 0 {
        draw_threads(frame, app, threads_area);
    }
    draw_chat(frame, app, chat_area);
    if context_w > 0 {
        draw_context(frame, app, context_area);
    }
    if popup_rows > 0 {
        let items: Vec<Line> = completions
            .iter()
            .take(6)
            .enumerate()
            .map(|(i, c)| {
                let style = if i == 0 {
                    Style::new().add_modifier(Modifier::BOLD)
                } else {
                    Style::new().add_modifier(Modifier::DIM)
                };
                Line::styled(c.clone(), style)
            })
            .collect();
        let popup = Paragraph::new(items).block(Block::bordered().title(" Tab completes "));
        frame.render_widget(popup, popup_area);
    }

    let visible = input_lines
        .len()
        .min(input_area.height.saturating_sub(2) as usize);
    let shown = &input_lines[input_lines.len() - visible..];
    let input = Paragraph::new(
        shown
            .iter()
            .map(|l| Line::raw(l.clone()))
            .collect::<Vec<_>>(),
    )
    .block(focused(
        Block::bordered().title_bottom(if app.focus == Focus::Input {
            " Enter send · Alt-Enter new line · Esc normal mode · /help "
        } else {
            " i insert · j/k move · gg/G · o open · y copy · x cancel · ? help "
        }),
        app.focus == Focus::Input,
    ));
    frame.render_widget(input, input_area);
    // The mode, vim-style (ADR 0015): Insert while the input has the keys.
    let mode = if app.focus == Focus::Input {
        "-- INSERT --"
    } else {
        "-- NORMAL --"
    };
    let status = Paragraph::new(Line::from(vec![
        Span::styled(
            format!(" {mode} "),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("· {}", app.status_line()),
            Style::new().add_modifier(Modifier::DIM),
        ),
        // Plan windows at 80% or more (ADR 0014); the clock is read here, not in `App`.
        Span::styled(
            roundtable::limits::warnings(
                &app.shown_limits(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as u64),
            )
            .iter()
            .map(|w| format!(" · {w}"))
            .collect::<String>(),
            Style::new().fg(Color::Yellow),
        ),
    ]));
    frame.render_widget(status, status_area);
    if app.focus == Focus::Input {
        // The cursor is always at the end of the input: after the last row.
        let last = shown.last().map_or(0, |l| l.width()) as u16;
        let row = visible.saturating_sub(1) as u16;
        frame.set_cursor_position(Position::new(
            (input_area.x + 1 + last).min(input_area.right().saturating_sub(2)),
            input_area.y + 1 + row,
        ));
    }
}

/// `"> " + input`, hard-wrapped to `width` columns by display width, so the cursor lands
/// exactly after the last row. A row that fills the width is followed by an empty one: the
/// cursor needs a cell after the text.
fn input_lines(input: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = vec![];
    for line in format!("> {input}").split('\n') {
        let mut row = String::new();
        let mut used = 0;
        // By grapheme, as the terminal draws them: `👩‍💻` is one two-column cell pair.
        for g in line.graphemes(true) {
            let w = g.width();
            if used + w > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push_str(g);
            used += w;
        }
        rows.push(row);
    }
    if rows.last().is_some_and(|r| r.width() >= width) {
        rows.push(String::new());
    }
    rows
}

/// The focused pane's border is bold.
fn focused(block: Block<'static>, focus: bool) -> Block<'static> {
    if focus {
        block.border_style(Style::new().add_modifier(Modifier::BOLD))
    } else {
        block.border_style(Style::new().add_modifier(Modifier::DIM))
    }
}

fn draw_chat(frame: &mut Frame, app: &App, area: Rect) {
    let text = if app.lines.is_empty() {
        let hint = if app.default_is_everyone() {
            "Nothing at the table yet. Type to everyone, start with @<id> for one, or /help."
        } else {
            "Nothing at the table yet. Type to Claude, start with @codex or @all, or /help."
        };
        Text::styled(hint, Style::new().add_modifier(Modifier::DIM))
    } else {
        let selected = (app.focus == Focus::Chat)
            .then_some(app.selected_line)
            .flatten();
        Text::from(
            app.lines
                .iter()
                .enumerate()
                .flat_map(|(i, line)| chat_lines(line, selected == Some(i)))
                .collect::<Vec<_>>(),
        )
    };
    // Keep the newest lines in view unless scrolled back (PgUp). Measured before the
    // border is added: `line_count` counts border rows too.
    let chat = Paragraph::new(text).wrap(Wrap { trim: false });
    let inner_height = area.height.saturating_sub(2);
    let total = chat.line_count(area.width.saturating_sub(2)) as u16;
    let bottom = total.saturating_sub(inner_height);
    let mut top = bottom.saturating_sub(app.scroll);
    // In Normal mode the selected line stays in view (j/k move it, ADR 0015).
    if let Some(i) = (app.focus == Focus::Chat && app.follow_selection)
        .then_some(app.selected_line)
        .flatten()
        .filter(|&i| i < app.lines.len())
    {
        let width = area.width.saturating_sub(2);
        let rows = |lines: &[roundtable::app::ChatLine]| {
            let text: Vec<Line> = lines
                .iter()
                .enumerate()
                .flat_map(|(j, l)| chat_lines(l, j == i))
                .collect();
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .line_count(width) as u16
        };
        let start = rows(&app.lines[..i]);
        let end = rows(&app.lines[..=i]);
        if start < top {
            top = start;
        } else if end > top + inner_height {
            top = end.saturating_sub(inner_height).min(start);
        }
    }
    let title = if top < bottom {
        format!(" roundtable · scrolled up {} lines (PgDn) ", bottom - top)
    } else {
        " roundtable ".to_string()
    };
    let chat = chat.scroll((top, 0)).block(focused(
        Block::bordered().title(title),
        app.focus == Focus::Chat,
    ));
    frame.render_widget(chat, area);
}

fn draw_threads(frame: &mut Frame, app: &App, area: Rect) {
    let focus = app.focus == Focus::Threads;
    let width = area.width.saturating_sub(4) as usize;
    let rows: Vec<Line> = if app.threads.is_empty() {
        vec![Line::styled(
            "no threads yet",
            Style::new().add_modifier(Modifier::DIM),
        )]
    } else {
        app.threads
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut style = Style::new();
                if t.elsewhere {
                    style = style.add_modifier(Modifier::DIM);
                }
                if focus && i == app.thread_pick {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                Line::styled(clip(&t.title, width), style)
            })
            .collect()
    };
    // The picked thread stays in view (`G` in a long list).
    let inner = area.height.saturating_sub(2) as usize;
    let top = (app.thread_pick + 1).saturating_sub(inner) as u16;
    let block = focused(Block::bordered().title(" threads "), focus);
    frame.render_widget(Paragraph::new(rows).scroll((top, 0)).block(block), area);
}

fn draw_context(frame: &mut Frame, app: &App, area: Rect) {
    let mut rows = vec![];
    for r in app.context_rows() {
        let name = Style::new().fg(colour(&r.id)).add_modifier(Modifier::BOLD);
        let state = Style::new().add_modifier(if r.state == "streaming" {
            Modifier::ITALIC
        } else {
            Modifier::DIM
        });
        // A full plan window blocks the CLI until it resets (ADR 0014).
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let limited = app
            .shown_limits()
            .get(&r.id)
            .and_then(|l| roundtable::limits::blocked_until(l, now))
            .map(|until| {
                let local = |s: i64| (s + crate::utc_offset_s()).max(0) as u64 * 1000;
                format!(
                    " limited until {}",
                    roundtable::limits::when(local(until), local(now as i64 / 1000))
                )
            });
        rows.push(Line::from(vec![
            Span::styled(r.id.clone(), name),
            Span::styled(format!(" {}", r.state), state),
            Span::styled(limited.unwrap_or_default(), Style::new().fg(Color::Yellow)),
        ]));
        // A bar only when the fill is knowable (ADR 0009); `?` for CLI agents.
        let fill = match r.fill {
            Some(f) => format!("{} ~{:.0}%", bar(f, 8), f * 100.0),
            None => "?".to_string(),
        };
        rows.push(Line::from(format!(
            "  {fill}  {}↑ {}↓",
            short(r.tokens.0),
            short(r.tokens.1)
        )));
        if let Some(role) = r.role {
            rows.push(Line::styled(
                format!("  {role}"),
                Style::new().add_modifier(Modifier::DIM),
            ));
        }
    }
    let block = Block::bordered()
        .title(" context ")
        .border_style(Style::new().add_modifier(Modifier::DIM));
    frame.render_widget(Paragraph::new(rows).block(block), area);
}

fn bar(fraction: f64, width: usize) -> String {
    let filled = ((fraction * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

/// `41k`-style token counts.
fn short(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

fn clip(text: &str, width: usize) -> String {
    let first = text.lines().next().unwrap_or("");
    if first.chars().count() <= width {
        first.to_string()
    } else {
        let cut: String = first.chars().take(width.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// Stable colour per speaker, from the 16-colour palette so light and dark themes work.
fn colour(who: &str) -> Color {
    match who.split(' ').next() {
        Some("claude") => Color::Yellow,
        Some("codex") => Color::Green,
        Some("roundtable") => Color::DarkGray,
        Some("you") => Color::Cyan,
        Some(_) => Color::Magenta,
        None => Color::Reset,
    }
}

pub fn chat_lines(line: &ChatLine, selected: bool) -> Vec<Line<'static>> {
    let mut who = Style::new()
        .fg(colour(&line.who))
        .add_modifier(Modifier::BOLD);
    if selected {
        who = who.add_modifier(Modifier::REVERSED);
    }
    let dim = Style::new().add_modifier(Modifier::DIM);
    // Tool and handoff lines: one row until expanded (ADR 0009).
    if line.expandable() {
        let (head, marker) = match line.kind {
            LineKind::Tool => (format!("  ↳ {} ran ", line.who), "tool"),
            _ => (format!("  ↳ {} ", line.who), "handoff"),
        };
        let arrow = if line.expanded { "▾" } else { "▸" };
        let mut out = vec![Line::from(vec![
            Span::styled(head, who),
            Span::styled(format!("[{marker} {arrow}] "), dim),
            Span::styled(
                if line.expanded {
                    String::new()
                } else {
                    clip(&line.text, 60)
                },
                dim,
            ),
        ])];
        if line.expanded {
            out.extend(
                line.text
                    .lines()
                    .map(|row| Line::styled(format!("      {row}"), Style::new())),
            );
        }
        return out;
    }
    let body = match line.kind {
        LineKind::Error => Style::new().fg(Color::Red),
        LineKind::Info => dim,
        _ => Style::new(),
    };
    let suffix = match line.kind {
        LineKind::Streaming => " …",
        LineKind::Reply(Outcome::Interrupted) => " [interrupted]",
        LineKind::Reply(Outcome::Failed) => " [failed]",
        _ => "",
    };
    let head = Span::styled(format!("{} › ", line.who), who);
    // Replies are Markdown (ADR 0009); everything else is plain text.
    let mut rows = if matches!(line.kind, LineKind::Reply(_) | LineKind::Streaming) {
        markdown(&line.text)
    } else {
        line.text
            .lines()
            .map(|row| Line::styled(row.to_string(), body))
            .collect()
    };
    if rows.is_empty() {
        rows.push(Line::default());
    }
    rows[0].spans.insert(0, head);
    if let Some(last) = rows.last_mut() {
        last.push_span(Span::styled(suffix, dim));
    }
    rows
}

/// A small Markdown renderer for replies: headings and strong text bold, emphasis italic,
/// inline code and code blocks cyan, list items bulleted. Enough for chat; not a browser.
pub fn markdown(text: &str) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = vec![];
    let mut current: Vec<Span<'static>> = vec![];
    let mut styles: Vec<Style> = vec![Style::new()];
    let mut in_code = false;
    // One entry per open list: the next number for an ordered list, `None` for bullets.
    let mut lists: Vec<Option<u64>> = vec![];
    let flush = |lines: &mut Vec<Line<'static>>, current: &mut Vec<Span<'static>>| {
        lines.push(Line::from(std::mem::take(current)));
    };
    for event in Parser::new(text) {
        let style = *styles.last().unwrap_or(&Style::new());
        // Headings end their own line.
        let ends_heading = matches!(event, Md::End(TagEnd::Heading(_)));
        match event {
            Md::Start(Tag::Heading { level, .. }) => {
                let mut s = style.add_modifier(Modifier::BOLD);
                if level == HeadingLevel::H1 {
                    s = s.add_modifier(Modifier::UNDERLINED);
                }
                styles.push(s);
            }
            Md::Start(Tag::Strong) => styles.push(style.add_modifier(Modifier::BOLD)),
            Md::Start(Tag::Emphasis) => styles.push(style.add_modifier(Modifier::ITALIC)),
            Md::Start(Tag::CodeBlock(kind)) => {
                in_code = true;
                if let CodeBlockKind::Fenced(lang) = kind
                    && !lang.is_empty()
                {
                    lines.push(Line::styled(
                        format!("  {lang}"),
                        Style::new().add_modifier(Modifier::DIM),
                    ));
                }
            }
            Md::Start(Tag::List(start)) => lists.push(start),
            Md::Start(Tag::Item) => {
                let indent = "  ".repeat(lists.len().saturating_sub(1));
                let marker = match lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        format!("{}. ", *n - 1)
                    }
                    _ => "• ".to_string(),
                };
                current.push(Span::raw(format!("{indent}{marker}")));
            }
            Md::End(TagEnd::Heading(_) | TagEnd::Strong | TagEnd::Emphasis) => {
                styles.pop();
            }
            Md::End(TagEnd::CodeBlock) => in_code = false,
            Md::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Md::End(TagEnd::Paragraph | TagEnd::Item) => flush(&mut lines, &mut current),
            Md::Text(t) if in_code => {
                for row in t.lines() {
                    lines.push(Line::styled(
                        format!("  {row}"),
                        Style::new().fg(Color::Cyan),
                    ));
                }
            }
            Md::Text(t) => current.push(Span::styled(t.into_string(), style)),
            Md::Code(t) => current.push(Span::styled(t.into_string(), style.fg(Color::Cyan))),
            Md::SoftBreak | Md::HardBreak => flush(&mut lines, &mut current),
            _ => {}
        }
        if ends_heading {
            flush(&mut lines, &mut current);
        }
    }
    if !current.is_empty() {
        flush(&mut lines, &mut current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn input_wraps_by_display_width() {
        assert_eq!(input_lines("", 10), ["> "]);
        assert_eq!(input_lines("hi", 10), ["> hi"]);
        // Exactly full: the cursor goes on a fresh row.
        assert_eq!(input_lines("12345678", 10), ["> 12345678", ""]);
        assert_eq!(input_lines("123456789", 10), ["> 12345678", "9"]);
        assert_eq!(input_lines("ab\ncd", 10), ["> ab", "cd"]);
        assert_eq!(input_lines("ab\n", 10), ["> ab", ""]);
        // Wide characters take two columns and never split across rows.
        assert_eq!(input_lines("日本語です", 7), ["> 日本", "語です"]);
        // A joined emoji is one grapheme: it moves to the next row whole.
        assert_eq!(input_lines("abc👩‍💻", 6), ["> abc", "👩‍💻"]);
        // A width of zero still makes progress.
        assert_eq!(input_lines("ab", 0).len(), 5);
    }

    #[test]
    fn markdown_keeps_the_text_and_marks_its_structure() {
        let lines = markdown(
            "# Title\n\nSome **bold** and `code`.\n\n- one\n- two\n\n3. three\n4. four\n\n```rust\nlet x = 1;\n```",
        );
        assert_eq!(
            plain(&lines),
            [
                "Title",
                "Some bold and code.",
                "• one",
                "• two",
                "3. three",
                "4. four",
                "  rust",
                "  let x = 1;"
            ]
        );
        let bold = &lines[1].spans[1];
        assert_eq!(bold.content, "bold");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn helpers() {
        assert_eq!(bar(0.5, 4), "██░░");
        assert_eq!(short(41_200), "41k");
        assert_eq!(clip("a long title here", 6), "a lon…");
    }
}
