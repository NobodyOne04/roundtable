//! roundtable: one terminal chat for Claude Code, Codex and other models.
//! Stack: ADR 0001. Event loop: ADR 0002. Transcript files: ADR 0010. CLI agents: ADR 0004.

use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event as TermEvent, EventStream};
use futures_util::StreamExt;
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use roundtable::agent::{self, Kind, Outcome, PEvent, Turn};
use roundtable::app::{App, ChatLine, Effect, Event, LineKind};
use roundtable::transcript::{NewEvent, Store, Thread};

/// Bounded so a flood of events slows its senders instead of growing memory (ADR 0002).
const CHANNEL_CAPACITY: usize = 1024;
/// ~30 fps redraw cap.
const TICK: Duration = Duration::from_millis(33);
/// How long quitting waits for cancelled turns to stop and be recorded.
const QUIT_GRACE: Duration = Duration::from_secs(5);

const USAGE: &str = "usage: roundtable [--outside-repo]

  --outside-repo   let Codex run outside a git repo (it refuses by default)";

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
    let Some(outside_repo) = parse_args()? else {
        println!("{USAGE}");
        return Ok(());
    };
    let root = data_root()?;
    init_logging(&root)?;
    // Before the terminal is taken over, so "already running" prints plainly.
    let store = Store::open(&root)?;
    let cwd = std::env::current_dir().context("reading the current directory")?;

    let checks: Vec<_> = Kind::ALL
        .iter()
        .map(|&kind| (kind, agent::preflight(kind, &cwd, outside_repo)))
        .collect();
    let mut app = App::new(
        checks
            .iter()
            .map(|(kind, check)| (*kind, check.as_ref().map(|_| ()).map_err(Clone::clone)))
            .collect(),
    );
    for (kind, check) in &checks {
        tracing::info!(agent = kind.id(), ?check, "preflight");
        match check {
            Ok(version) => app.note(format!("{}: {version}", kind.id()), false),
            Err(why) => app.note(format!("{}: unavailable: {why}", kind.id()), true),
        }
    }

    // Enters raw mode and the alternate screen, and installs a panic hook that restores the
    // terminal first, so a crash never leaves the user's shell broken.
    let terminal = ratatui::init();
    let runtime = Runtime {
        store,
        thread: None,
        sessions: BTreeMap::new(),
        turns: HashMap::new(),
        cwd,
        outside_repo,
    };
    let result = run(terminal, app, runtime).await;
    ratatui::restore();
    result
}

/// `Ok(None)` means help was asked for.
fn parse_args() -> Result<Option<bool>> {
    let mut outside_repo = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--outside-repo" => outside_repo = true,
            "-h" | "--help" => return Ok(None),
            other => anyhow::bail!("unknown argument `{other}`\n\n{USAGE}"),
        }
    }
    Ok(Some(outside_repo))
}

/// What the loop owns besides `App`: handles that do I/O.
struct Runtime {
    store: Store,
    /// Opened on the first message, so starting and quitting leaves no empty thread.
    thread: Option<Thread>,
    sessions: BTreeMap<String, String>,
    turns: HashMap<Kind, CancellationToken>,
    cwd: PathBuf,
    outside_repo: bool,
}

impl Runtime {
    // shortcut: appends run on the loop task, not a writer task with acks (ADR 0010);
    // fine while writes are small, move them off the loop when fsync latency shows.
    fn record(&mut self, event: NewEvent, durable: bool) -> Result<()> {
        let thread = match &mut self.thread {
            Some(thread) => thread,
            None => {
                let title: String = event.payload["text"]
                    .as_str()
                    .unwrap_or("untitled")
                    .chars()
                    .take(60)
                    .collect();
                let thread = self.store.create_thread(&title, &self.cwd)?;
                tracing::info!(thread = %thread.meta.id, "thread opened");
                self.thread.insert(thread)
            }
        };
        thread.append(event, durable)?;
        Ok(())
    }

    fn save_session(&mut self, agent: Kind, session: String) -> Result<()> {
        self.sessions.insert(agent.id().into(), session);
        if let Some(thread) = &self.thread {
            thread.save_sessions(&self.sessions)?;
        }
        Ok(())
    }

    fn cancel_all(&self) {
        for token in self.turns.values() {
            token.cancel();
        }
    }
}

async fn run(mut terminal: DefaultTerminal, mut app: App, mut rt: Runtime) -> Result<()> {
    let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
    tokio::spawn(read_terminal(tx.clone()));
    tokio::spawn(tick(tx.clone()));

    let mut quit_by: Option<tokio::time::Instant> = None;
    loop {
        let deadline = quit_by.unwrap_or_else(tokio::time::Instant::now);
        let event = tokio::select! {
            event = rx.recv() => match event {
                Some(event) => event,
                None => return Ok(()),
            },
            _ = tokio::time::sleep_until(deadline), if quit_by.is_some() => {
                tracing::warn!(left = rt.turns.len(), "turns did not stop in time");
                return Ok(());
            }
        };
        if let Event::Agent(kind, PEvent::Done(_)) = &event {
            rt.turns.remove(kind);
        }
        for effect in app.update(event) {
            match effect {
                Effect::Redraw => {
                    terminal.draw(|frame| draw(frame, &app))?;
                }
                Effect::Quit => {
                    tracing::info!("quit");
                    // Let cancelled turns report `Done`, so no CLI outlives roundtable and
                    // each interrupted reply is still recorded.
                    rt.cancel_all();
                    quit_by.get_or_insert(tokio::time::Instant::now() + QUIT_GRACE);
                }
                Effect::StartTurn {
                    agent: kind,
                    prompt,
                    session,
                } => {
                    let token = CancellationToken::new();
                    rt.turns.insert(kind, token.clone());
                    let turn = Turn {
                        prompt,
                        session,
                        cwd: rt.cwd.clone(),
                        outside_repo: rt.outside_repo,
                    };
                    tokio::spawn(agent::run(kind, turn, tx.clone(), token));
                }
                Effect::CancelTurns => rt.cancel_all(),
                Effect::Record { event, durable } => {
                    if let Err(err) = rt.record(event, durable) {
                        tracing::error!(%err, "transcript write failed");
                        let _ = tx.try_send(Event::Notice(format!("transcript: {err:#}")));
                    }
                }
                Effect::SaveSession {
                    agent: kind,
                    session,
                } => {
                    if let Err(err) = rt.save_session(kind, session) {
                        tracing::error!(%err, "saving the session failed");
                        let _ = tx.try_send(Event::Notice(format!("sessions: {err:#}")));
                    }
                }
            }
        }
        if quit_by.is_some() && rt.turns.is_empty() {
            return Ok(());
        }
    }
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

fn draw(frame: &mut Frame, app: &App) {
    let [chat_area, input_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(frame.area());

    let text = if app.lines.is_empty() {
        Text::styled(
            "Nothing at the table yet. Type to Claude, or start with @codex or @all.",
            Style::new().add_modifier(Modifier::DIM),
        )
    } else {
        Text::from(app.lines.iter().flat_map(chat_lines).collect::<Vec<_>>())
    };
    // Keep the newest lines in view. Measured before the border is added: `line_count`
    // counts border rows too.
    let chat = Paragraph::new(text).wrap(Wrap { trim: false });
    let inner_height = chat_area.height.saturating_sub(2);
    let total = chat.line_count(chat_area.width.saturating_sub(2)) as u16;
    let chat = chat
        .scroll((total.saturating_sub(inner_height), 0))
        .block(Block::bordered().title(" roundtable "));
    frame.render_widget(chat, chat_area);

    let input = Paragraph::new(format!("> {}", app.input)).block(
        Block::bordered()
            .title_bottom(" Enter send · @codex @all · Esc cancel · Ctrl-C twice quit "),
    );
    frame.render_widget(input, input_area);
    let typed = app.input.chars().count() as u16;
    frame.set_cursor_position(Position::new(
        (input_area.x + 3 + typed).min(input_area.right().saturating_sub(2)),
        input_area.y + 1,
    ));
}

/// Stable colour per speaker, from the 16-colour palette so light and dark themes work.
fn colour(who: &str) -> Color {
    match who.split(' ').next() {
        Some("claude") => Color::Yellow,
        Some("codex") => Color::Cyan,
        Some("roundtable") => Color::DarkGray,
        _ => Color::Reset,
    }
}

fn chat_lines(line: &ChatLine) -> Vec<Line<'static>> {
    let who = Style::new()
        .fg(colour(&line.who))
        .add_modifier(Modifier::BOLD);
    let body = match line.kind {
        LineKind::Error => Style::new().fg(Color::Red),
        LineKind::Info | LineKind::Tool => Style::new().add_modifier(Modifier::DIM),
        _ => Style::new(),
    };
    let suffix = match line.kind {
        LineKind::Streaming => " …",
        LineKind::Reply(Outcome::Interrupted) => " [interrupted]",
        LineKind::Reply(Outcome::Failed) => " [failed]",
        _ => "",
    };
    let head = match line.kind {
        LineKind::Tool => format!("  ↳ {} ran ", line.who),
        _ => format!("{} › ", line.who),
    };
    let mut rows = line.text.lines();
    let first = rows.next().unwrap_or("");
    let mut out = vec![Line::from(vec![
        Span::styled(head, who),
        Span::styled(first.to_string(), body),
    ])];
    out.extend(rows.map(|row| Line::from(Span::styled(row.to_string(), body))));
    if let Some(last) = out.last_mut() {
        last.push_span(Span::styled(
            suffix,
            Style::new().add_modifier(Modifier::DIM),
        ));
    }
    out
}

/// `<data_dir>/roundtable`: transcript, lock and log (ADR 0010).
fn data_root() -> Result<PathBuf> {
    let dir = dirs::data_dir()
        .context("no data directory on this system")?
        .join("roundtable");
    roundtable::transcript::private_dir(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Logs go to `<root>/roundtable.log`: the TUI owns stdout. Level from `RUST_LOG`.
fn init_logging(root: &Path) -> Result<()> {
    let path = root.join("roundtable.log");
    let file = OpenOptions::new()
        .mode(0o600)
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
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn render(app: &App, width: u16, height: u16) -> TestBackend {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal.backend().clone()
    }

    #[test]
    fn draws_the_empty_table() {
        insta::assert_snapshot!(render(&App::default(), 60, 8));
    }

    #[test]
    fn draws_a_conversation_scrolled_to_the_newest_line() {
        let mut app = App::default();
        for c in "@all review the plan".chars() {
            app.update(Event::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
        app.update(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        app.update(Event::Agent(
            Kind::Codex,
            PEvent::Tool("cat CLAUDE.md".into()),
        ));
        app.update(Event::Agent(
            Kind::Codex,
            PEvent::Message("Looks fine.".into()),
        ));
        app.update(Event::Agent(Kind::Codex, PEvent::Done(Outcome::Complete)));
        app.update(Event::Agent(
            Kind::Claude,
            PEvent::TextDelta("Two notes:\n1. a".into()),
        ));
        insta::assert_snapshot!(render(&app, 60, 10));
    }
}
