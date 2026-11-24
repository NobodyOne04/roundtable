//! roundtable: one terminal chat for Claude Code, Codex and other models.
//! Stack and terminal rules: ADR 0001. Event loop: ADR 0002. Transcript files: ADR 0010.

use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event as TermEvent, EventStream};
use futures_util::StreamExt;
use ratatui::layout::Alignment;
use ratatui::widgets::{Block, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use roundtable::app::{App, Effect, Event};
use roundtable::transcript::Store;

/// Bounded so a flood of events slows its senders instead of growing memory (ADR 0002).
const CHANNEL_CAPACITY: usize = 1024;
/// ~30 fps redraw cap.
const TICK: Duration = Duration::from_millis(33);

#[tokio::main]
async fn main() -> ExitCode {
    match start().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // The terminal is restored by now, so the message is readable.
            eprintln!("roundtable: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn start() -> Result<()> {
    let root = data_root()?;
    init_logging(&root)?;
    // Before the terminal is taken over, so "already running" prints plainly.
    let _store = Store::open(&root)?;
    // Enters raw mode and the alternate screen, and installs a panic hook that restores the
    // terminal first, so a crash never leaves the user's shell broken.
    let terminal = ratatui::init();
    let result = run(terminal).await;
    ratatui::restore();
    result
}

async fn run(mut terminal: DefaultTerminal) -> Result<()> {
    let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
    tokio::spawn(read_terminal(tx.clone()));
    tokio::spawn(tick(tx));

    let mut app = App::default();
    while let Some(event) = rx.recv().await {
        for effect in app.update(event) {
            match effect {
                Effect::Redraw => {
                    terminal.draw(draw)?;
                }
                Effect::Quit => {
                    tracing::info!("quit");
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// Keyboard and resize events from the terminal.
async fn read_terminal(tx: mpsc::Sender<Event>) {
    let mut input = EventStream::new();
    let reason = loop {
        let event = match input.next().await {
            Some(Ok(TermEvent::Key(key))) => Event::Key(key),
            Some(Ok(TermEvent::Resize(..))) => Event::Resize,
            Some(Ok(_)) => continue,
            Some(Err(err)) => break err.to_string(),
            None => break "end of input".to_string(),
        };
        if tx.send(event).await.is_err() {
            return; // the loop is gone: the app is exiting
        }
    };
    let _ = tx.send(Event::InputClosed(reason)).await;
}

async fn tick(tx: mpsc::Sender<Event>) {
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if tx.send(Event::Tick).await.is_err() {
            return;
        }
    }
}

fn draw(frame: &mut Frame) {
    let body = Paragraph::new("Nothing at the table yet.\n\nq or Ctrl-C to quit")
        .alignment(Alignment::Center)
        .block(Block::bordered().title(" roundtable "));
    frame.render_widget(body, frame.area());
}

/// `<data_dir>/roundtable`: transcript, lock and log (ADR 0010).
fn data_root() -> Result<PathBuf> {
    let dir = dirs::data_dir()
        .context("no data directory on this system")?
        .join("roundtable");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Logs go to `<root>/roundtable.log`: the TUI owns stdout. Level from `RUST_LOG`.
fn init_logging(root: &std::path::Path) -> Result<()> {
    let path = root.join("roundtable.log");
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
}
