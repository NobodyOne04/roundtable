//! roundtable: one terminal chat for Claude Code, Codex and other models.
//! The stack and terminal rules are ADR 0001.

use std::fs::{self, OpenOptions};
use std::sync::Mutex;

use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Alignment;
use ratatui::widgets::{Block, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    init_logging()?;
    // Enters raw mode and the alternate screen, and installs a panic hook that restores the
    // terminal first, so a crash never leaves the user's shell broken.
    let terminal = ratatui::init();
    let result = run(terminal);
    ratatui::restore();
    result
}

// shortcut: blocking key reads, no async loop yet; ADR 0002 replaces this with the event loop.
fn run(mut terminal: DefaultTerminal) -> Result<()> {
    loop {
        terminal.draw(draw)?;
        if let Event::Key(key) = event::read()?
            && is_quit(key)
        {
            tracing::info!("quit");
            return Ok(());
        }
    }
}

fn is_quit(key: KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && (key.code == KeyCode::Char('q')
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)))
}

fn draw(frame: &mut Frame) {
    let body = Paragraph::new("Nothing at the table yet.\n\nq or Ctrl-C to quit")
        .alignment(Alignment::Center)
        .block(Block::bordered().title(" roundtable "));
    frame.render_widget(body, frame.area());
}

/// Logs go to `<data_dir>/roundtable/roundtable.log`: the TUI owns stdout. Level from `RUST_LOG`.
fn init_logging() -> Result<()> {
    let dir = dirs::data_dir()
        .context("no data directory on this system")?
        .join("roundtable");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("roundtable.log");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    tracing_subscriber::fmt()
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn draws_the_empty_table() {
        let mut terminal = Terminal::new(TestBackend::new(40, 7)).unwrap();
        terminal.draw(draw).unwrap();
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn quits_on_q_and_ctrl_c_only() {
        let key = |code, mods| KeyEvent::new(code, mods);
        assert!(is_quit(key(KeyCode::Char('q'), KeyModifiers::NONE)));
        assert!(is_quit(key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(!is_quit(key(KeyCode::Char('c'), KeyModifiers::NONE)));
        assert!(!is_quit(key(KeyCode::Esc, KeyModifiers::NONE)));
    }
}
