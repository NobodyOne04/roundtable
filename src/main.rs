//! roundtable: one terminal chat for Claude Code, Codex and other models.
//! Stack: ADR 0001. Event loop: ADR 0002. Transcript files: ADR 0010. CLI agents: ADR 0004.

mod ui;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, EventStream,
};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use notify_debouncer_mini::notify::RecursiveMode;
use notify_debouncer_mini::{DebounceEventResult, DebouncedEventBatch, new_debouncer};
use roundtable::agent::{self, AskBridge, Outcome, PEvent, Turn};
use roundtable::app::{App, Checks, EditLock, Effect, Event, FileEdit, TestTarget};
use roundtable::mcp::{self, AskAnswer};
use roundtable::openai;
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

fn main() -> ExitCode {
    // `roundtable mcp` is the `ask` tool's server, launched by an agent CLI: plain stdio, no
    // terminal, no data lock (the running roundtable holds it).
    if std::env::args().nth(1).as_deref() == Some("mcp") {
        return match mcp::serve() {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        };
    }
    tui_main()
}

#[tokio::main]
async fn tui_main() -> ExitCode {
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
            let crashed = lock.crashed().map(marker_lock);
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
        secrets: vec![],
        ask_socket: ask_socket_path(&root)?,
        ask_tokens: HashMap::new(),
        ask_files: HashMap::new(),
        pending_asks: HashMap::new(),
        next_ask: 1,
        redactors: HashMap::new(),
        threads_changed: true,
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
    // Bracketed paste: a pasted block arrives whole, so its line breaks don't send it.
    let _ = crossterm::execute!(std::io::stdout(), EnableBracketedPaste);
    let restore_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
        restore_hook(info);
    }));
    let result = run(terminal, app, runtime).await;
    let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
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
    /// Resolved API keys with their references, redacted from everything shown or written
    /// (ADR 0010). Values only ever live in memory.
    secrets: Vec<(String, String)>,
    /// `ask` (ADR 0007): the socket `roundtable mcp` connects to, the one-time token of each
    /// running turn that may ask (token -> caller), and the connections waiting for answers.
    ask_socket: PathBuf,
    ask_tokens: HashMap<String, String>,
    /// Per caller: its turn's MCP config file, deleted when the turn ends.
    ask_files: HashMap<String, PathBuf>,
    pending_asks: HashMap<u64, tokio::sync::oneshot::Sender<AskAnswer>>,
    next_ask: u64,
    /// The threads list changed (a thread was created or resumed): resend it to the pane.
    threads_changed: bool,
    /// Per participant: streamed text held back while it could still be part of a key.
    redactors: HashMap<String, transcript::StreamRedactor>,
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
    fn record(&mut self, mut event: NewEvent, durable: bool) -> Result<()> {
        transcript::redact(&mut event.payload, &self.secrets);
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
                self.threads_changed = true;
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
    fn edit_registry(&mut self, edit: &FileEdit) -> Result<String, String> {
        let text = fs::read_to_string(&self.models_path).map_err(|e| e.to_string())?;
        let text = match edit {
            FileEdit::Enabled { id, enabled } => registry::set_enabled(&text, id, *enabled),
            FileEdit::Default(id) => registry::set_default(&text, id),
            FileEdit::Talk(on) => registry::set_talk_enabled(&text, *on),
            FileEdit::Allow { from, targets } => registry::set_allow(&text, from, targets),
            FileEdit::Budget(n) => registry::set_talk_number(&text, "budget_tokens", *n as i64),
            FileEdit::Turns(n) => registry::set_talk_number(&text, "max_turns", i64::from(*n)),
        }
        .map_err(|e| format!("{e} (fix models.toml; the change applies in this session only)"))?;
        transcript::write_atomic(&self.models_path, text.as_bytes()).map_err(|e| e.to_string())?;
        self.models_hash = Some(registry::fingerprint(&text));
        Ok(text)
    }

    /// Takes the checkout lock and writes the crash marker for a writable turn of `id`,
    /// before its delivery is recorded (ADR 0012).
    fn begin_writer(&mut self, id: &str) -> Result<(), Refusal> {
        let lock = self
            .lock
            .as_ref()
            .ok_or_else(|| Refusal::Failed("this checkout has no writer lock".into()))?;
        // The folder may have become a different checkout since start (say, a nested
        // `git init`): never lock one checkout while editing another.
        match writer_lock::resolve(&self.cwd) {
            Ok(Checkout::Git { lock: now, .. }) if now == *lock => {}
            _ => {
                return Err(Refusal::Failed(
                    "the checkout changed since roundtable started; restart it".into(),
                ));
            }
        }
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
                // History: close the crashed turn in its own thread when it is in this data
                // folder (through the live handle if it is the open thread, so sequence
                // numbers stay unique); otherwise note the recovery in the current thread.
                let active = self.thread.as_ref().map(|t| t.meta.id.clone());
                let result = match &marker {
                    Some(m)
                        if m.data_root == self.data_root && active.as_ref() == Some(&m.thread) =>
                    {
                        self.record(transcript::settle_event(), true)
                    }
                    Some(m) if m.data_root == self.data_root && !m.thread.is_empty() => self
                        .store
                        .settle_writer(&m.thread)
                        .map_err(anyhow::Error::from),
                    _ => {
                        let note = serde_json::json!({
                            "recovered": "crash marker cleared by /quiet",
                            "marker": marker,
                        });
                        self.record(
                            NewEvent {
                                turn: None,
                                parent: None,
                                from: transcript::Party {
                                    id: "user".into(),
                                    identity: None,
                                },
                                to: vec![],
                                kind: transcript::Kind::Status,
                                payload: note,
                                tokens_in: None,
                                tokens_out: None,
                            },
                            true,
                        )
                    }
                };
                if let Err(err) = result {
                    tracing::warn!(%err, "recording the recovery after /quiet failed");
                }
                Event::QuietDone(Ok(()))
            }
            Ok(Cleared::Changed(state)) => {
                local.push_back(Event::LockEdits(marker_lock(state)));
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

    /// See `Event::Agent` in the loop. Held-back text is released before `Done`, so a turn
    /// that ends early still shows and records what it said.
    fn redact_output(
        &mut self,
        id: &str,
        output: PEvent,
        local: &mut VecDeque<Event>,
    ) -> Option<PEvent> {
        let secrets = &self.secrets;
        let clean = |text: String| {
            let mut value = serde_json::Value::String(text);
            transcript::redact(&mut value, secrets);
            value.as_str().unwrap_or_default().to_string()
        };
        let redactor = self.redactors.entry(id.to_string()).or_default();
        match output {
            PEvent::TextDelta(text) => {
                let shown = redactor.push(&text, secrets);
                (!shown.is_empty()).then_some(PEvent::TextDelta(shown))
            }
            PEvent::Message(text) => Some(PEvent::Message(clean(text))),
            PEvent::Error(text) => Some(PEvent::Error(clean(text))),
            PEvent::Tool(text) => Some(PEvent::Tool(clean(text))),
            PEvent::Done(outcome) => {
                let rest = redactor.flush();
                if rest.is_empty() {
                    Some(PEvent::Done(outcome))
                } else {
                    local.push_front(Event::Agent(id.to_string(), PEvent::Done(outcome)));
                    Some(PEvent::TextDelta(rest))
                }
            }
            other => Some(other),
        }
    }

    /// A one-time token for `id`'s turn and its MCP config, written owner-only so the token
    /// stays off command lines (ADR 0007).
    fn ask_bridge(&mut self, id: &str) -> Result<AskBridge> {
        let token = ulid::Ulid::generate().to_string();
        let dir = self.data_root.join("mcp");
        transcript::private_dir(&dir)?;
        // The file name is not the token.
        let config_file = dir.join(format!("{}.json", ulid::Ulid::generate()));
        let bridge = AskBridge {
            exe: std::env::current_exe().unwrap_or_else(|_| "roundtable".into()),
            socket: self.ask_socket.clone(),
            caller: id.to_string(),
            token: token.clone(),
            config_file,
        };
        transcript::write_atomic(&bridge.config_file, bridge.claude_config().as_bytes())?;
        self.ask_tokens.insert(token, id.to_string());
        self.ask_files
            .insert(id.to_string(), bridge.config_file.clone());
        Ok(bridge)
    }

    /// `/resume`: without a choice, the recent threads, numbered; with one (a number from that
    /// list or an id prefix), the thread opened for further messages.
    fn resume(&mut self, which: Option<&str>) -> Event {
        let threads = match self.store.list_threads() {
            Ok(threads) => threads,
            Err(err) => return Event::Notice(format!("listing threads: {err}")),
        };
        let Some(which) = which else {
            if threads.is_empty() {
                return Event::Report("no past threads".into());
            }
            let mut rows =
                vec!["past threads (newest first); /resume <n> reopens one:".to_string()];
            for (n, meta) in threads.iter().take(20).enumerate() {
                let here = if meta.checkout == self.checkout {
                    ""
                } else {
                    "  [elsewhere]"
                };
                rows.push(format!(
                    "{:>2}. {}  {}{here}",
                    n + 1,
                    transcript::day_of(meta.created_at),
                    meta.title
                ));
            }
            return Event::Report(rows.join("\n"));
        };
        let Some(meta) = pick_thread(&threads, which) else {
            return Event::Notice(format!("no thread {which}; /resume lists them"));
        };
        let thread = match self.store.open_thread(&meta.id) {
            Ok(thread) => thread,
            Err(err) => return Event::Notice(format!("opening the thread: {err}")),
        };
        let events = match thread.events() {
            Ok(events) => events,
            Err(err) => return Event::Notice(format!("reading the thread: {err}")),
        };
        let sessions = thread.load_sessions();
        let elsewhere = (meta.checkout != self.checkout).then(|| meta.checkout.clone());
        self.sessions = sessions.clone();
        // Further messages go into this thread (ADR 0010); nothing is sent into one from
        // another checkout, so it is not opened for writing then.
        self.thread = elsewhere.is_none().then_some(thread);
        Event::Resumed {
            title: meta.title.clone(),
            events,
            sessions,
            elsewhere,
        }
    }

    /// The threads pane's rows, newest first.
    fn thread_rows(&self) -> Vec<roundtable::app::ThreadRow> {
        self.store
            .list_threads()
            .unwrap_or_default()
            .into_iter()
            .take(50)
            .map(|m| roundtable::app::ThreadRow {
                elsewhere: m.checkout != self.checkout,
                day: transcript::day_of(m.created_at),
                id: m.id,
                title: m.title,
            })
            .collect()
    }

    /// `/usage <scope>`: tokens and cost per model from `usage.json` (ADR 0010).
    fn usage_report(&self, scope: &str) -> String {
        let totals = match scope {
            "thread" => match &self.thread {
                Some(thread) => {
                    let mut total = BTreeMap::<String, transcript::Tally>::new();
                    for by in thread.usage().values() {
                        for (id, tally) in by {
                            total.entry(id.clone()).or_default().add(tally);
                        }
                    }
                    Ok(total)
                }
                None => Ok(BTreeMap::new()),
            },
            "today" => self
                .store
                .usage(Some(&transcript::today()))
                .map_err(|e| e.to_string()),
            _ => self.store.usage(None).map_err(|e| e.to_string()),
        };
        match totals {
            Err(why) => format!("usage: {why}"),
            Ok(totals) if totals.is_empty() => format!("usage ({scope}): nothing yet"),
            Ok(totals) => {
                let mut rows = vec![format!("usage ({scope}):")];
                for (id, t) in totals {
                    let cost = if t.cost_usd > 0.0 {
                        format!(", ${:.4}", t.cost_usd)
                    } else {
                        String::new()
                    };
                    let estimated = if t.estimated {
                        " (partly estimated)"
                    } else {
                        ""
                    };
                    rows.push(format!(
                        "{id}: {} in, {} out{cost}{estimated}",
                        t.input, t.output
                    ));
                }
                rows.join("\n")
            }
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
    listen_for_asks(&rt.ask_socket, tx.clone())?;

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
            Event::AskIncoming { request, reply } => {
                // Only a live turn's own token may ask, as itself.
                if rt.ask_tokens.get(&request.token) != Some(&request.caller) {
                    let _ = reply.0.send(AskAnswer {
                        text: "blocked: not a running roundtable turn".into(),
                        refused: true,
                    });
                    continue;
                }
                let ask = rt.next_ask;
                rt.next_ask += 1;
                rt.pending_asks.insert(ask, reply.0);
                Event::Asked {
                    ask,
                    caller: request.caller,
                    to: request.to,
                    prompt: request.prompt,
                }
            }
            Event::KeyResolved { reference, value } => {
                if !rt.secrets.iter().any(|(v, _)| *v == value.0) {
                    rt.secrets.push((value.0, reference));
                }
                continue;
            }
            // Every participant's output, any model or CLI, is cleaned of every known key
            // before the app shows or records it.
            Event::Agent(id, output) => match rt.redact_output(&id, output, &mut local) {
                Some(output) => Event::Agent(id, output),
                None => continue,
            },
            other => other,
        };
        if let Event::Agent(id, PEvent::Done(_)) = &event {
            rt.turns.remove(id);
            // Its token dies with the turn: a late `ask` from it is refused.
            rt.ask_tokens.retain(|_, caller| caller != id);
            if let Some(file) = rt.ask_files.remove(id) {
                let _ = fs::remove_file(file);
            }
        }
        // Writable turns refused in this batch (lock, marker or transcript), with why.
        let mut refused: Vec<(String, String)> = vec![];
        for effect in app.update(event) {
            match effect {
                Effect::Redraw => {
                    terminal.draw(|frame| ui::draw(frame, &app))?;
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
                    ask_tool,
                } => {
                    if writable && let Some((_, why)) = refused.iter().find(|(r, _)| *r == id) {
                        // Fail closed (ADR 0012): an editable turn never starts without its
                        // lock and its marker, or a crash during it would leave nothing.
                        let why = format!("not started: {why}");
                        local.push_back(Event::Agent(id.clone(), PEvent::Error(why)));
                        local.push_back(Event::Agent(id, PEvent::Done(Outcome::Failed)));
                        continue;
                    }
                    let ask = if ask_tool {
                        match rt.ask_bridge(&id) {
                            Ok(bridge) => Some(bridge),
                            Err(err) => {
                                // Without its config file the turn just runs without `ask`.
                                local.push_back(Event::Notice(format!("ask tool: {err:#}")));
                                None
                            }
                        }
                    } else {
                        None
                    };
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
                        ask,
                    };
                    tokio::spawn(agent::run(id, kind, turn, tx.clone(), token));
                }
                Effect::StartChat {
                    id,
                    base_url,
                    model,
                    key_ref,
                    system,
                    history,
                    prompt,
                    context_window,
                    max_output,
                    connect_timeout_s,
                    idle_timeout_s,
                } => {
                    // The key is resolved inside the turn, off this loop (the keychain may
                    // block); it comes back as `KeyResolved` before any reply.
                    let token = CancellationToken::new();
                    rt.turns.insert(id.clone(), token.clone());
                    let chat = openai::Chat {
                        base_url,
                        model,
                        key_ref,
                        system,
                        history,
                        prompt,
                        context_window,
                        max_output,
                        connect_timeout: Duration::from_secs(connect_timeout_s),
                        idle_timeout: Duration::from_secs(idle_timeout_s),
                    };
                    tokio::spawn(openai::run(id, chat, tx.clone(), token));
                }
                Effect::CancelTurn(id) => {
                    if let Some(token) = rt.turns.get(&id) {
                        token.cancel();
                    }
                }
                Effect::CancelTurns => rt.cancel_all(),
                Effect::Answer { ask, text, refused } => {
                    if let Some(reply) = rt.pending_asks.remove(&ask) {
                        let _ = reply.send(AskAnswer { text, refused });
                    }
                }
                Effect::NewThread => {
                    // The next message opens a new thread; native sessions start over.
                    rt.thread = None;
                    rt.sessions.clear();
                }
                Effect::Usage(scope) => local.push_back(Event::Report(rt.usage_report(&scope))),
                Effect::Resume(which) => local.push_back(rt.resume(which.as_deref())),
                Effect::TestModel { id, target } => {
                    let tx = tx.clone();
                    let (cwd, outside) = (rt.cwd.clone(), rt.outside_repo);
                    tokio::spawn(async move {
                        let started = std::time::Instant::now();
                        let text = match target {
                            TestTarget::Cli { kind, cmd } => {
                                let check = tokio::task::spawn_blocking(move || {
                                    agent::preflight(kind, &cmd, &cwd, outside)
                                })
                                .await
                                .unwrap_or_else(|e| Err(e.to_string()));
                                match check {
                                    Ok(version) => format!(
                                        "{id}: ready, {version} ({} ms)",
                                        started.elapsed().as_millis()
                                    ),
                                    Err(why) => format!("{id}: unavailable: {why}"),
                                }
                            }
                            TestTarget::Http {
                                base_url,
                                model,
                                key_ref,
                            } => match openai::ping(&base_url, &model, key_ref).await {
                                Ok(took) => format!("{id}: answered in {} ms", took.as_millis()),
                                Err(why) => format!("{id}: {why}"),
                            },
                        };
                        let _ = tx.send(Event::Report(text)).await;
                    });
                }
                Effect::ConfirmQuiet(marker) => {
                    let result = rt.confirm_quiet(&marker, &mut local);
                    local.push_back(result);
                }
                Effect::EditRegistry(edit) => match rt.edit_registry(&edit) {
                    // The file may carry hand edits the app hasn't applied yet (say, a
                    // revoked writer): apply the whole written file, not just the command.
                    Ok(text) => {
                        local.push_back(Event::Registry(Registry::parse(&text).map(|registry| {
                            let checks = rt.checks(&registry);
                            (registry, checks)
                        })))
                    }
                    Err(err) => {
                        tracing::error!(%err, "editing models.toml failed");
                        local.push_back(Event::Notice(format!("models.toml: {err}")));
                    }
                },
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
                        if let Refusal::Unfinished(state) = &refusal {
                            local.push_back(Event::LockEdits(marker_lock(state.clone())));
                        }
                        refused.push((id.clone(), refusal.to_string()));
                    }
                    // The writer's own reply closes its turn: clear the marker right after it
                    // is recorded, so a queued edit in this same batch finds the lock free.
                    let reply_of = (event.kind == transcript::Kind::Msg
                        && event.payload["writable"] == true
                        && rt.writers.contains_key(&event.from.id))
                    .then(|| event.from.id.clone());
                    match rt.record(event, durable) {
                        Ok(()) => {
                            if let Some(id) = reply_of
                                && let Err(err) = rt.end_writer(&id)
                            {
                                tracing::error!(%err, "ending the writer turn failed");
                                local.push_back(Event::Notice(format!("{err:#}")));
                            }
                        }
                        Err(err) => {
                            tracing::error!(%err, "transcript write failed");
                            local.push_back(Event::Notice(format!("transcript: {err:#}")));
                            if let Some(id) = reply_of {
                                // The reply isn't on disk: keep the marker (fail closed),
                                // only let the lock go.
                                rt.writers.remove(&id);
                            }
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
        if std::mem::take(&mut rt.threads_changed) {
            local.push_back(Event::Threads(rt.thread_rows()));
        }
        if quit_by.is_some() && rt.turns.is_empty() {
            return Ok(());
        }
    }
}

/// Where the `ask` socket lives: the owner-only data folder, unless that path is too long
/// for a Unix socket (about 104 bytes on macOS); then an owner-only folder in the temp dir.
fn ask_socket_path(root: &Path) -> Result<PathBuf> {
    const MAX: usize = 100;
    let preferred = root.join("ask.sock");
    if preferred.as_os_str().len() < MAX {
        return Ok(preferred);
    }
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let dir = std::env::temp_dir().join(format!("roundtable-{uid}"));
    // private_dir sets 0700, which fails on a folder another user owns: never shared.
    transcript::private_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("ask.sock");
    anyhow::ensure!(
        path.as_os_str().len() < MAX,
        "no short enough place for the ask socket ({})",
        path.display()
    );
    Ok(path)
}

/// The socket `roundtable mcp` connects to (ADR 0007), in the owner-only data folder. Each
/// connection carries one `ask` and waits for its answer.
fn listen_for_asks(path: &Path, tx: mpsc::Sender<Event>) -> Result<()> {
    // We hold the data lock, so a leftover socket file is from a dead run.
    let _ = fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path)
        .with_context(|| format!("listening on {}", path.display()))?;
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                // One request line, bounded: nothing else is spoken here.
                let mut reader = BufReader::new(read.take(1024 * 1024));
                if reader.read_line(&mut line).await.is_err() {
                    return;
                }
                let answer = match serde_json::from_str::<mcp::AskRequest>(&line) {
                    Ok(request) => {
                        let (send, recv) = tokio::sync::oneshot::channel();
                        let event = Event::AskIncoming {
                            request,
                            reply: roundtable::app::Responder(send),
                        };
                        if tx.send(event).await.is_err() {
                            return;
                        }
                        recv.await.unwrap_or(AskAnswer {
                            text: "blocked: roundtable is shutting down".into(),
                            refused: true,
                        })
                    }
                    Err(_) => AskAnswer {
                        text: "blocked: malformed request".into(),
                        refused: true,
                    },
                };
                if let Ok(mut bytes) = serde_json::to_vec(&answer) {
                    bytes.push(b'\n');
                    let _ = write.write_all(&bytes).await;
                }
            });
        }
    });
    Ok(())
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
            Some(Ok(TermEvent::Paste(text))) => Event::Paste(text),
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

/// `/resume <which>`: a number from the list (newest first) or the start of an id.
fn pick_thread<'a>(threads: &'a [transcript::Meta], which: &str) -> Option<&'a transcript::Meta> {
    match which.parse::<usize>() {
        Ok(n) if n >= 1 => threads.get(n - 1),
        _ => {
            let which = which.to_uppercase();
            let mut found = threads.iter().filter(|m| m.id.starts_with(&which));
            // Ambiguous prefixes pick nothing rather than a guess.
            match (found.next(), found.next()) {
                (Some(one), None) => Some(one),
                _ => None,
            }
        }
    }
}

/// The edit lock for a crash marker. One that can't be read can't be confirmed by `/quiet`
/// either: it locks until it can be read.
fn marker_lock(state: MarkerState) -> EditLock {
    match state {
        MarkerState::Present { text, key } => EditLock::Crashed { text, key },
        MarkerState::Unreadable(why) => {
            EditLock::Unknown(format!("the crash marker can't be read: {why}"))
        }
        MarkerState::Absent => EditLock::Unknown("the crash marker vanished".into()),
    }
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
        terminal.draw(|frame| ui::draw(frame, app)).unwrap();
        terminal.backend().clone()
    }

    #[test]
    fn draws_the_completion_popup_and_a_scrolled_view() {
        let mut app = App::default();
        for c in "/mo".chars() {
            app.update(Event::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
        insta::assert_snapshot!("popup", render(&app, 60, 12));
        let mut app = App::default();
        for n in 0..12 {
            app.note(format!("line {n}"), false);
        }
        app.update(Event::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )));
        insta::assert_snapshot!("scrolled", render(&app, 60, 9));
    }

    #[test]
    fn the_ask_socket_path_always_fits() {
        assert_eq!(
            ask_socket_path(Path::new("/d")).unwrap(),
            Path::new("/d/ask.sock")
        );
        let long = Path::new("/").join("x".repeat(150));
        let path = ask_socket_path(&long).unwrap();
        assert!(path.as_os_str().len() < 100, "{}", path.display());
        assert!(path.ends_with("ask.sock"));
    }

    #[test]
    fn resume_picks_by_number_or_unambiguous_id_prefix() {
        let meta = |id: &str| roundtable::transcript::Meta {
            id: id.into(),
            title: id.into(),
            created_at: 0,
            checkout: "/".into(),
            parent_id: None,
            fork_event: None,
        };
        let threads = [meta("01AAA"), meta("01AAB"), meta("01BCC")];
        assert_eq!(pick_thread(&threads, "2").unwrap().id, "01AAB");
        assert!(pick_thread(&threads, "9").is_none());
        assert!(pick_thread(&threads, "0").is_none());
        assert_eq!(
            pick_thread(&threads, "01b").unwrap().id,
            "01BCC",
            "case-insensitive"
        );
        assert!(
            pick_thread(&threads, "01AA").is_none(),
            "ambiguous picks nothing"
        );
    }

    #[test]
    fn draws_three_panes_when_wide_and_one_when_narrow() {
        let mut app = App::default();
        app.update(Event::Threads(vec![
            roundtable::app::ThreadRow {
                id: "01A".into(),
                title: "fix the login bug".into(),
                day: "2026-11-25".into(),
                elsewhere: false,
            },
            roundtable::app::ThreadRow {
                id: "01B".into(),
                title: "review in another repo".into(),
                day: "2026-11-24".into(),
                elsewhere: true,
            },
        ]));
        for c in "@claude check with codex".chars() {
            app.update(Event::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
        app.update(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        app.update(Event::Asked {
            ask: 1,
            caller: "claude".into(),
            to: "codex".into(),
            prompt: "is the **fix** right?\nline two".into(),
        });
        app.update(Event::Agent(
            "codex".into(),
            PEvent::Message("Yes, `ok`.".into()),
        ));
        // Tab to the chat, Enter expands the handoff.
        app.update(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        app.update(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        insta::assert_snapshot!("wide", render(&app, 130, 14));
        insta::assert_snapshot!("narrow", render(&app, 80, 14));
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

    /// The screen and where the cursor ended up.
    fn render_with_cursor(app: &App, width: u16, height: u16) -> (TestBackend, (u16, u16)) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| ui::draw(frame, app)).unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        (terminal.backend().clone(), (cursor.x, cursor.y))
    }

    #[test]
    fn a_long_prompt_wraps_and_keeps_the_cursor_in_view() {
        let mut app = App::default();
        let prompt = "please review the writer lock and tell me whether a crash mid-turn leaves the marker behind";
        for c in prompt.chars() {
            app.update(Event::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
        let (screen, cursor) = render_with_cursor(&app, 60, 12);
        insta::assert_snapshot!("long_input", screen);
        // "> " + 91 characters in 58 columns: 58 on the first row, 35 on the second.
        assert_eq!(cursor, (36, 9));
    }

    #[test]
    fn a_tall_prompt_shows_its_last_rows() {
        let mut app = App::default();
        let text = (1..=12)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.update(Event::Paste(text));
        let (screen, cursor) = render_with_cursor(&app, 60, 16);
        insta::assert_snapshot!("tall_input", screen);
        assert_eq!(cursor, (8, 13));
    }

    #[test]
    fn a_paste_goes_into_the_input_and_is_not_sent() {
        let mut app = App::default();
        let effects = app.update(Event::Paste("first\r\nsecond\rthird".into()));
        assert!(effects.is_empty());
        assert_eq!(app.input, "first\nsecond\nthird");
    }
}
