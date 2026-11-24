//! roundtable: one terminal chat for Claude Code, Codex and other models.
//! Stack: ADR 0001. Event loop: ADR 0002. Transcript files: ADR 0010. CLI agents: ADR 0004.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, OpenOptions};
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

use notify_debouncer_mini::notify::RecursiveMode;
use notify_debouncer_mini::{DebounceEventResult, DebouncedEventBatch, new_debouncer};
use roundtable::agent::{self, Outcome, PEvent, Turn};
use roundtable::app::{App, ChatLine, Checks, EditLock, Effect, Event, FileEdit, LineKind};
use roundtable::registry::{self, Adapter, Registry};
use roundtable::transcript::{self, NewEvent, Store, Thread};
use roundtable::writer_lock::{
    self, Checkout, CheckoutLock, Cleared, MarkerState, Refusal, WriterGuard,
};

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

    let models_path = config_root()?.join("models.toml");
    // The writer lock lives in the checkout (ADR 0012); anything short of a clear git
    // checkout locks edits.
    let (checkout, lock, edit_lock) = match writer_lock::resolve(&cwd) {
        Ok(Checkout::Git { root, lock }) => {
            let crashed = lock.crashed().map(|state| match state {
                MarkerState::Present(text) | MarkerState::Unreadable(text) => {
                    EditLock::Crashed(text)
                }
                MarkerState::Absent => unreachable!("crashed() never returns Absent"),
            });
            (root, Some(lock), crashed)
        }
        Ok(Checkout::Outside) => (cwd.clone(), None, Some(EditLock::OutsideGit)),
        Err(why) => (cwd.clone(), None, Some(EditLock::Unknown(why))),
    };
    let mut runtime = Runtime {
        store,
        thread: None,
        sessions: BTreeMap::new(),
        turns: HashMap::new(),
        checkout,
        lock,
        writers: HashMap::new(),
        data_root: root.clone(),
        cwd,
        outside_repo,
        models_path,
        models_hash: None,
        checked: HashMap::new(),
    };
    let (registry, checks, load_error) = match runtime.load_registry() {
        Ok((registry, checks)) => (registry, checks, None),
        // Not applied at all (ADR 0011): start empty and say why; fixing the file applies it.
        Err(why) => (
            Registry::parse("").expect("empty"),
            Checks::new(),
            Some(why),
        ),
    };
    let mut app = App::new(registry, checks.clone());
    for (id, check) in &checks {
        tracing::info!(id, ?check, "preflight");
        match check {
            Ok(version) => app.note(format!("{id}: {version}"), false),
            Err(why) => app.note(format!("{id}: unavailable: {why}"), true),
        }
    }
    if let Some(edit_lock) = edit_lock {
        app.lock_edits(edit_lock);
    }
    if let Some(why) = load_error {
        app.note(
            format!("{} not applied: {why}", runtime.models_path.display()),
            true,
        );
    }

    // Enters raw mode and the alternate screen, and installs a panic hook that restores the
    // terminal first, so a crash never leaves the user's shell broken.
    let terminal = ratatui::init();
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
    /// Native sessions by `id/identity`, so a new destination never resumes an old session.
    sessions: BTreeMap<String, String>,
    turns: HashMap<String, CancellationToken>,
    cwd: PathBuf,
    /// The git checkout root around `cwd` (or `cwd` outside git), recorded in each thread.
    checkout: PathBuf,
    /// The checkout's writer lock (ADR 0012); `None` outside git or when git failed, and
    /// then the app keeps edits locked.
    lock: Option<CheckoutLock>,
    /// The running writable turn's lock and marker, by participant id.
    writers: HashMap<String, WriterGuard>,
    data_root: PathBuf,
    outside_repo: bool,
    models_path: PathBuf,
    /// Fingerprint of the `models.toml` text last applied or written, so own writes and
    /// unrelated changes in the folder are not reloaded.
    models_hash: Option<String>,
    /// Preflight per CLI command, so a reload doesn't rerun it for known commands.
    checked: HashMap<String, Result<String, String>>,
}

impl Runtime {
    // shortcut: appends run on the loop task, not a writer task with acks (ADR 0010);
    // fine while writes are small, move them off the loop when fsync latency shows.
    fn record(&mut self, event: NewEvent, durable: bool) -> Result<()> {
        let thread = match &mut self.thread {
            Some(thread) => thread,
            // Only a message opens a thread; registry notes before it go to the log only.
            None if event.kind == transcript::Kind::Status => {
                tracing::info!(payload = %event.payload, "status before any thread");
                return Ok(());
            }
            None => {
                let title: String = event.payload["text"]
                    .as_str()
                    .unwrap_or("untitled")
                    .chars()
                    .take(60)
                    .collect();
                let thread = self.store.create_thread(&title, &self.checkout)?;
                tracing::info!(thread = %thread.meta.id, "thread opened");
                self.thread.insert(thread)
            }
        };
        thread.append(event, durable)?;
        Ok(())
    }

    fn save_session(&mut self, id: &str, identity: &str, session: String) -> Result<()> {
        self.sessions.insert(format!("{id}/{identity}"), session);
        if let Some(thread) = &self.thread {
            thread.save_sessions(&self.sessions)?;
        }
        Ok(())
    }

    /// Reads `models.toml`, writing the default file first if there is none.
    fn load_registry(&mut self) -> Result<(Registry, Checks), String> {
        let text = match fs::read_to_string(&self.models_path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                transcript::write_atomic(&self.models_path, registry::DEFAULT_FILE.as_bytes())
                    .map_err(|e| format!("writing the default file: {e}"))?;
                registry::DEFAULT_FILE.to_string()
            }
            Err(err) => return Err(err.to_string()),
        };
        self.models_hash = Some(registry::fingerprint(&text));
        let registry = Registry::parse(&text)?;
        let checks = self.checks(&registry);
        Ok((registry, checks))
    }

    /// `None` when the file is unchanged since the last read or write.
    fn reload_registry(&mut self) -> Option<Result<(Registry, Checks), String>> {
        let text = match fs::read_to_string(&self.models_path) {
            Ok(text) => text,
            Err(err) => return Some(Err(format!("reading models.toml: {err}"))),
        };
        let hash = registry::fingerprint(&text);
        if self.models_hash.as_ref() == Some(&hash) {
            return None;
        }
        self.models_hash = Some(hash);
        Some(Registry::parse(&text).map(|registry| {
            let checks = self.checks(&registry);
            (registry, checks)
        }))
    }

    // shortcut: preflight runs on the loop; only new commands are checked, each in ~100 ms.
    fn checks(&mut self, registry: &Registry) -> Checks {
        let mut checks = Checks::new();
        for p in &registry.participants {
            let (Adapter::Cli(kind), Some(cmd)) = (p.adapter, &p.cmd) else {
                continue;
            };
            let result = self
                .checked
                .entry(cmd.clone())
                .or_insert_with(|| agent::preflight(kind, cmd, &self.cwd, self.outside_repo))
                .clone();
            checks.insert(p.id.clone(), result);
        }
        checks
    }

    /// Writes a command's change into `models.toml`, keeping comments (ADR 0011).
    fn edit_registry(&mut self, edit: &FileEdit) -> Result<(), String> {
        let text = fs::read_to_string(&self.models_path).map_err(|e| e.to_string())?;
        let text = match edit {
            FileEdit::Enabled { id, enabled } => registry::set_enabled(&text, id, *enabled),
            FileEdit::Default(id) => registry::set_default(&text, id),
        }
        .map_err(|e| format!("{e} (fix models.toml; the change applies in this session only)"))?;
        transcript::write_atomic(&self.models_path, text.as_bytes()).map_err(|e| e.to_string())?;
        self.models_hash = Some(registry::fingerprint(&text));
        Ok(())
    }

    /// Takes the checkout lock and writes the crash marker for a writable turn of `id`,
    /// before its delivery is recorded (ADR 0012).
    fn begin_writer(&mut self, id: &str) -> Result<(), Refusal> {
        let lock = self
            .lock
            .as_ref()
            .ok_or_else(|| Refusal::Failed("this checkout has no writer lock".into()))?;
        let thread = self
            .thread
            .as_ref()
            .map(|t| t.meta.id.as_str())
            .unwrap_or("");
        let guard = lock.begin(&writer_lock::marker_for(thread, &self.data_root))?;
        self.writers.insert(id.into(), guard);
        Ok(())
    }

    /// The writable turn of `id` is over (or never started): clear its marker, release the
    /// lock. A failure leaves the marker, so later edits stay refused (fail closed).
    fn end_writer(&mut self, id: &str) -> Result<()> {
        if let Some(guard) = self.writers.remove(id) {
            guard.finish().context("removing the writer marker")?;
        }
        Ok(())
    }

    /// `/quiet`: removes exactly the crash marker the user confirmed, durably (ADR 0012).
    fn confirm_quiet(&mut self, confirmed: &str, local: &mut VecDeque<Event>) -> Event {
        let Some(lock) = &self.lock else {
            return Event::QuietDone(Err("this checkout has no writer lock".into()));
        };
        match lock.clear(confirmed) {
            Ok(Cleared::Removed(marker)) => {
                tracing::info!(?marker, "crash marker removed after /quiet");
                // History: close the crashed turn in its thread when it is ours.
                if let Some(m) = marker
                    && m.data_root == self.data_root
                    && !m.thread.is_empty()
                    && let Err(err) = self.store.settle_writer(&m.thread)
                {
                    tracing::warn!(thread = %m.thread, %err, "settle after /quiet failed");
                }
                Event::QuietDone(Ok(()))
            }
            Ok(Cleared::Changed(text)) => {
                local.push_back(Event::LockEdits(EditLock::Crashed(text)));
                Event::QuietDone(Err(
                    "the marker changed since it was shown; check the new one, then /quiet".into(),
                ))
            }
            Ok(Cleared::Busy) => Event::QuietDone(Err(
                "a writer is running right now in another roundtable".into(),
            )),
            Err(err) => Event::QuietDone(Err(format!("removing the marker: {err}"))),
        }
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
    // Held for the whole run: dropping it stops the watch.
    let _watcher = watch_registry(&rt.models_path, tx.clone())?;

    let mut quit_by: Option<tokio::time::Instant> = None;
    // Events the loop raises itself: handled before the channel, never dropped (a full
    // channel must not lose, say, the `Done` of a turn that never started).
    let mut local: VecDeque<Event> = VecDeque::new();
    loop {
        let deadline = quit_by.unwrap_or_else(tokio::time::Instant::now);
        let event = if let Some(event) = local.pop_front() {
            event
        } else {
            tokio::select! {
            event = rx.recv() => match event {
                Some(event) => event,
                None => return Ok(()),
            },
            _ = tokio::time::sleep_until(deadline), if quit_by.is_some() => {
                tracing::warn!(left = rt.turns.len(), "turns did not stop in time");
                return Ok(());
            }
            }
        };
        let event = match event {
            Event::RegistryFileChanged => match rt.reload_registry() {
                Some(result) => Event::Registry(result),
                None => continue,
            },
            other => other,
        };
        let mut done = None;
        if let Event::Agent(id, PEvent::Done(_)) = &event {
            rt.turns.remove(id);
            done = Some(id.clone());
        }
        // Writable turns refused in this batch (lock, marker or transcript), with why.
        let mut refused: Vec<(String, String)> = vec![];
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
                    id,
                    kind,
                    cmd,
                    model,
                    prompt,
                    session,
                    writable,
                    permission_mode,
                } => {
                    if writable && let Some((_, why)) = refused.iter().find(|(r, _)| *r == id) {
                        // Fail closed (ADR 0012): an editable turn never starts without its
                        // lock and its marker, or a crash during it would leave nothing.
                        let why = format!("not started: {why}");
                        local.push_back(Event::Agent(id.clone(), PEvent::Error(why)));
                        local.push_back(Event::Agent(id, PEvent::Done(Outcome::Failed)));
                        continue;
                    }
                    let token = CancellationToken::new();
                    rt.turns.insert(id.clone(), token.clone());
                    let turn = Turn {
                        cmd,
                        prompt,
                        session,
                        cwd: rt.cwd.clone(),
                        outside_repo: rt.outside_repo,
                        model,
                        writable,
                        permission_mode,
                    };
                    tokio::spawn(agent::run(id, kind, turn, tx.clone(), token));
                }
                Effect::CancelTurn(id) => {
                    if let Some(token) = rt.turns.get(&id) {
                        token.cancel();
                    }
                }
                Effect::CancelTurns => rt.cancel_all(),
                Effect::ConfirmQuiet(marker) => {
                    let result = rt.confirm_quiet(&marker, &mut local);
                    local.push_back(result);
                }
                Effect::EditRegistry(edit) => {
                    if let Err(err) = rt.edit_registry(&edit) {
                        tracing::error!(%err, "editing models.toml failed");
                        local.push_back(Event::Notice(format!("models.toml: {err}")));
                    }
                }
                Effect::Record { mut event, durable } => {
                    let writer = (event.kind == transcript::Kind::Delivery
                        && event.payload["writable"] == true)
                        .then(|| event.to.first().map(|p| p.id.clone()))
                        .flatten();
                    // Lock and marker first, then the delivery (ADR 0012); a refused turn is
                    // recorded as refused, not sent.
                    if let Some(id) = &writer
                        && let Err(refusal) = rt.begin_writer(id)
                    {
                        event.payload["status"] = "refused".into();
                        event.payload["writable"] = false.into();
                        event.payload["refused"] = refusal.to_string().into();
                        if let Refusal::Unfinished(marker) = &refusal {
                            local.push_back(Event::LockEdits(EditLock::Crashed(marker.clone())));
                        }
                        refused.push((id.clone(), refusal.to_string()));
                    }
                    if let Err(err) = rt.record(event, durable) {
                        tracing::error!(%err, "transcript write failed");
                        local.push_back(Event::Notice(format!("transcript: {err:#}")));
                        if let Some(id) = writer
                            && !refused.iter().any(|(r, _)| *r == id)
                        {
                            if let Err(err) = rt.end_writer(&id) {
                                local.push_back(Event::Notice(format!("{err:#}")));
                            }
                            refused.push((id, "the transcript could not be written".into()));
                        }
                    }
                }
                Effect::SaveSession {
                    id,
                    identity,
                    session,
                } => {
                    if let Err(err) = rt.save_session(&id, &identity, session) {
                        tracing::error!(%err, "saving the session failed");
                        local.push_back(Event::Notice(format!("sessions: {err:#}")));
                    }
                }
            }
        }
        // After the reply was recorded above: clear the marker, release the lock.
        if let Some(id) = done
            && let Err(err) = rt.end_writer(&id)
        {
            tracing::error!(%err, "ending the writer turn failed");
            local.push_back(Event::Notice(format!("{err:#}")));
        }
        if quit_by.is_some() && rt.turns.is_empty() {
            return Ok(());
        }
    }
}

/// Watches the folder of `models.toml` (editors save by rename, so the file itself can't be
/// watched) and reports changes to it, debounced 200 ms (ADR 0011).
fn watch_registry(
    path: &Path,
    tx: mpsc::Sender<Event>,
) -> Result<notify_debouncer_mini::Debouncer<notify_debouncer_mini::notify::RecommendedWatcher>> {
    let name = path.file_name().map(ToOwned::to_owned);
    let mut debouncer = new_debouncer(
        Duration::from_millis(200),
        move |result: DebounceEventResult| {
            let ours = match &result {
                Ok(DebouncedEventBatch::Events(events)) => {
                    events.iter().any(|e| e.path.file_name() == name.as_deref())
                }
                // Missed events: re-read to be safe; the hash check drops non-changes.
                Ok(_) => true,
                Err(err) => {
                    tracing::warn!(?err, "watching models.toml");
                    false
                }
            };
            if ours {
                // Runs on the watcher's own thread, outside the runtime.
                let _ = tx.blocking_send(Event::RegistryFileChanged);
            }
        },
    )
    .context("starting the models.toml watcher")?;
    let dir = path.parent().context("models.toml has no folder")?;
    debouncer
        .watcher()
        .watch(dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {}", dir.display()))?;
    Ok(debouncer)
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

/// `<config_dir>/roundtable`: `models.toml` (ADR 0011). Same folder as the data on macOS.
fn config_root() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("no config directory on this system")?
        .join("roundtable");
    transcript::private_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
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
            "codex".into(),
            PEvent::Tool("cat CLAUDE.md".into()),
        ));
        app.update(Event::Agent(
            "codex".into(),
            PEvent::Message("Looks fine.".into()),
        ));
        app.update(Event::Agent(
            "codex".into(),
            PEvent::Done(Outcome::Complete),
        ));
        app.update(Event::Agent(
            "claude".into(),
            PEvent::TextDelta("Two notes:\n1. a".into()),
        ));
        insta::assert_snapshot!(render(&app, 60, 10));
    }
}
