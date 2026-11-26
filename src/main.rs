//! roundtable: one terminal chat for Claude Code, Codex and other models.
//! Stack: ADR 0001. Event loop: ADR 0002. Transcript files: ADR 0010. CLI agents: ADR 0004.

mod ui;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
use roundtable::app::{App, Checks, EditLock, Effect, Event, FileEdit, Link, TestTarget, WIDE};
use roundtable::mcp::{self, AskAnswer};
use roundtable::openai;
use roundtable::registry::{self, Adapter, Registry};
use roundtable::transcript::{self, NewEvent, Store, Thread};
use roundtable::worktree;
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
    // `/log` files left by a crash (ADR 0024): only this data folder's own, and only now
    // that its instance lock is held, so no running roundtable still has one open.
    for file in log_files(&root) {
        let _ = fs::remove_file(file);
    }
    let cwd = std::env::current_dir().context("reading the current directory")?;

    let models_path = config_root()?.join("models.toml");
    // The writer lock lives in the checkout (ADR 0012); anything short of a clear git
    // checkout locks edits.
    let resolved = writer_lock::resolve(&cwd);
    let checkout_error = resolved.as_ref().err().cloned();
    let (checkout, lock, edit_lock) = match resolved {
        Ok(Checkout::Git { root, lock }) => {
            let crashed = lock.crashed().map(marker_lock);
            (root, Some(lock), crashed)
        }
        Ok(Checkout::Outside) => (cwd.clone(), None, Some(EditLock::OutsideGit)),
        Err(why) => (cwd.clone(), None, Some(EditLock::Unknown(why))),
    };
    let launch_sub = cwd.canonicalize().ok().and_then(|cwd| {
        let base = checkout.canonicalize().ok()?;
        cwd.strip_prefix(base).ok().map(Path::to_path_buf)
    });
    let mut runtime = Runtime {
        store,
        thread: None,
        keys: HashMap::new(),
        viewing: None,
        approvals: HashMap::new(),
        sessions: BTreeMap::new(),
        turn_cwd: HashMap::new(),
        checkout_error,
        cleaning: HashSet::new(),
        syncs: Arc::new(()),
        launch_sub,
        turns: HashMap::new(),
        checkout,
        lock,
        writers: HashMap::new(),
        // Canonical: mirror paths, their records and session keys are built from it, and the
        // same folder reached through another link must give the same ones (ADR 0016).
        data_root: root.canonicalize().unwrap_or_else(|_| root.clone()),
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
    // The last known plan limits (ADR 0014); a missing or unreadable file just means none.
    if let Ok(bytes) = fs::read(root.join("limits.json"))
        && let Ok(limits) = serde_json::from_slice(&bytes)
    {
        app.limits = limits;
    }
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

/// Hands the terminal to another program: what `ratatui::init` and bracketed paste set up,
/// undone.
fn leave_screen() -> Result<()> {
    crossterm::execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;
    crossterm::terminal::disable_raw_mode()?;
    Ok(())
}

/// Takes the terminal back after `leave_screen`.
fn enter_screen() -> Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        EnableBracketedPaste
    )?;
    Ok(())
}

/// While held, roundtable ignores SIGINT and SIGQUIT, as `system(3)` does: out of raw mode
/// Ctrl-C reaches the whole foreground group, and an interrupt meant for the editor (e.g.
/// `code --wait`) must not kill roundtable. Held from before leaving the screen until after
/// taking it back, so no moment of the hand-over is exposed.
struct InterruptGuard {
    int: libc::sighandler_t,
    quit: libc::sighandler_t,
}

impl InterruptGuard {
    fn hold() -> Self {
        // SAFETY: `signal` only swaps dispositions; `Drop` puts the old ones back.
        unsafe {
            InterruptGuard {
                int: libc::signal(libc::SIGINT, libc::SIG_IGN),
                quit: libc::signal(libc::SIGQUIT, libc::SIG_IGN),
            }
        }
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        // SAFETY: as in `hold`.
        unsafe {
            libc::signal(libc::SIGINT, self.int);
            libc::signal(libc::SIGQUIT, self.quit);
        }
    }
}

/// Runs `/log`'s editor on `file` and waits (ADR 0024). `Err` says why it failed. Call it
/// with an `InterruptGuard` held.
fn run_editor(editor: &[String], file: &Path) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let (program, args) = editor.split_first().ok_or("no editor set")?;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).arg(file);
    // The caller ignores interrupts (`InterruptGuard`); the editor gets the defaults back.
    // SAFETY: between fork and exec `signal` is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            Ok(())
        });
    }
    match cmd.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("/log: editor `{program}` exited with {status}")),
        Err(err) => Err(format!("/log: editor `{program}`: {err}")),
    }
}

/// The owner-only folder in the system temp dir that holds `/log` files of the data folder
/// `root` (ADR 0024): one per data folder, so instances never touch each other's files.
fn log_dir(root: &Path) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    // The data folder itself, as the instance lock sees it (device and inode), so any
    // spelling of its path (a symlink, `XDG_DATA_HOME`) names the same log folder. A path
    // that doesn't exist yet falls back to its raw bytes (`display()` would be lossy).
    let key = match fs::metadata(root) {
        Ok(meta) => format!("dev{}ino{}", meta.dev(), meta.ino()),
        Err(_) => root
            .as_os_str()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    };
    let id = roundtable::registry::fingerprint(&key);
    std::env::temp_dir().join(format!("roundtable-log-{id}"))
}

/// The `/log` files of data folder `root`; none unless the folder is verified ours.
fn log_files(root: &Path) -> impl Iterator<Item = PathBuf> {
    let dir = log_dir(root);
    let ours = transcript::owned_private_dir(&dir).is_ok();
    ours.then(|| fs::read_dir(dir))
        .into_iter()
        .flatten()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        // Only names roundtable makes, `<ULID>.md`: a copy the owner saved there stays.
        .filter(|path| {
            path.extension().is_some_and(|e| e == "md")
                && path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.len() == 26 && ulid::Ulid::from_string(s).is_ok())
        })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The local time zone's offset from UTC at Unix second `at`, in seconds: its own, so a
/// time after a daylight-saving change is shown right.
pub fn utc_offset_at(at: i64) -> i64 {
    // SAFETY: `localtime_r` only writes into the `tm` we pass; a null result leaves it zeroed.
    unsafe {
        let at = at as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&at, &mut tm);
        tm.tm_gmtoff
    }
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
    /// Per running writer turn: where its approval answers go (ADR 0018).
    approvals: HashMap<String, mpsc::UnboundedSender<String>>,
    /// A thread resumed from another checkout: shown and readable (`/log`), never written.
    viewing: Option<Thread>,
    /// `App`'s `Link` keys to the `seq` they were written at, for the open thread only.
    keys: HashMap<u64, u64>,
    /// Native sessions by `id/identity/cwd`, so a new destination, or a move between the
    /// checkout and a mirror (ADR 0016), never resumes a session made elsewhere.
    sessions: BTreeMap<String, String>,
    /// The folder each running turn runs in, for keying the session it reports.
    turn_cwd: HashMap<String, PathBuf>,
    /// Why the checkout couldn't be named, if git failed: then no agent gets a mirror and
    /// non-writer turns fail rather than run in the checkout (ADR 0016).
    checkout_error: Option<String>,
    /// Mirrors `/worktrees clean` is removing: their agents' turns wait for it to finish.
    cleaning: HashSet<String>,
    /// One clone per running mirror sync or removal: quitting waits for them (ADR 0016).
    syncs: Arc<()>,
    /// Where roundtable was started, relative to the checkout root, fixed at startup: the
    /// same folder in each mirror. `None` if it can't be placed (then no mirrors).
    launch_sub: Option<PathBuf>,
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
    fn record(&mut self, mut event: NewEvent, durable: bool, link: Link) -> Result<()> {
        transcript::redact(&mut event.payload, &self.secrets);
        if self.thread.is_none() {
            self.keys.clear();
        }
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
                self.viewing = None;
                self.thread.insert(thread)
            }
        };
        let resolve = |key| (key != 0).then(|| self.keys.get(&key).copied()).flatten();
        event.turn = resolve(link.turn);
        event.parent = resolve(link.parent);
        let seq = thread.append(event, durable)?;
        if link.key != 0 {
            self.keys.insert(link.key, seq);
        }
        Ok(())
    }

    /// Mirror syncs and removals must end before the instance lock goes, or the next
    /// roundtable could work on the same mirror (ADR 0016); a cancelled sync ends promptly.
    async fn mirror_work_done(&self) {
        while Arc::strong_count(&self.syncs) > 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn save_session(&mut self, id: &str, identity: &str, session: String) -> Result<()> {
        let cwd = self.turn_cwd.get(id).unwrap_or(&self.cwd);
        self.sessions
            .insert(session_key(id, identity, cwd), session);
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
                        self.record(transcript::settle_event(), true, Link::default())
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
                            Link::default(),
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
            // What the card shows is cleaned; the input goes back to Claude as it came.
            PEvent::Approval(mut request) => {
                request.summary = clean(request.summary);
                Some(PEvent::Approval(request))
            }
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
        if elsewhere.is_none() {
            self.thread = Some(thread);
            self.viewing = None;
        } else {
            self.thread = None;
            self.viewing = Some(thread);
        }
        self.keys.clear();
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

    /// `/log` (ADR 0024): the open thread as Markdown in a new owner-only temp file;
    /// `None` when there is no message to log yet.
    fn write_log(&self) -> Result<Option<PathBuf>> {
        let Some(thread) = self.thread.as_ref().or(self.viewing.as_ref()) else {
            return Ok(None);
        };
        let events = thread.events()?;
        if !events.iter().any(|e| e.kind == transcript::Kind::Msg) {
            return Ok(None);
        }
        let text = roundtable::log::render(&thread.meta.title, &events, utc_offset_at);
        let dir = log_dir(&self.data_root);
        transcript::owned_private_dir(&dir).with_context(|| format!("using {}", dir.display()))?;
        let path = dir.join(format!("{}.md", ulid::Ulid::generate()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        std::io::Write::write_all(&mut file, text.as_bytes())?;
        Ok(Some(path))
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
    // Events the loop raises itself: handled before the channel, never dropped (a full
    // channel must not lose, say, the `Done` of a turn that never started). The first is
    // the start width: the pane layout and `h`/`Tab` depend on it.
    let mut local: VecDeque<Event> = VecDeque::new();
    local.push_back(Event::Resize(terminal.size().map_or(WIDE, |s| s.width)));
    let mut input = tokio::spawn(read_terminal(tx.clone()));
    // `/log`'s temp file while its editor has the terminal (ADR 0024): no drawing then. The
    // guard keeps interrupts away from roundtable for the whole hand-over.
    let mut editing: Option<(PathBuf, InterruptGuard)> = None;
    tokio::spawn(tick(tx.clone()));
    // Held for the whole run: dropping it stops the watch.
    let _watcher = watch_registry(&rt.models_path, tx.clone())?;
    listen_for_asks(&rt.ask_socket, tx.clone())?;

    let mut quit_by: Option<tokio::time::Instant> = None;
    // The last tick's time: a plan window resetting since then needs a redraw (ADR 0014).
    let mut last_tick = now_ms();
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
                rt.mirror_work_done().await;
                return Ok(());
            }
            }
        };
        if let Event::Tick = event {
            let now = now_ms();
            // Through the local queue, as every event the loop raises; drawn next tick.
            if roundtable::limits::resets_between(&app.shown_limits(), last_tick, now) {
                local.push_back(Event::Clock);
            }
            last_tick = now;
        }
        let event = match event {
            Event::EditorClosed(failure) => {
                let guard = editing.take().map(|(path, guard)| {
                    let _ = fs::remove_file(path);
                    guard
                });
                enter_screen()?;
                terminal.clear()?;
                drop(guard);
                input = tokio::spawn(read_terminal(tx.clone()));
                // The terminal may have been resized meanwhile: refresh the width either way.
                if let Some(why) = failure {
                    local.push_back(Event::Notice(why));
                }
                Event::Resize(terminal.size().map_or(WIDE, |s| s.width))
            }
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
            Event::WorktreesDone { ids, report } => {
                for id in ids {
                    rt.cleaning.remove(&id);
                }
                Event::Report(report)
            }
            Event::Agent(id, output) => match rt.redact_output(&id, output, &mut local) {
                Some(output) => Event::Agent(id, output),
                None => continue,
            },
            other => other,
        };
        if let Event::Agent(id, PEvent::Done(_)) = &event {
            rt.turns.remove(id);
            rt.approvals.remove(id);
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
                Effect::Redraw if editing.is_some() => {}
                Effect::Redraw => {
                    let mut shown = app.scroll;
                    terminal.draw(|frame| shown = ui::draw(frame, &app))?;
                    if shown != app.scroll {
                        local.push_back(Event::Viewport(shown));
                    }
                }
                Effect::AnswerApproval {
                    id,
                    request_id,
                    allow,
                    input,
                    message,
                } => {
                    let line = agent::approval_response(&request_id, allow, &input, &message);
                    // Gone: the turn ended meanwhile; the app already expired the request.
                    if let Some(answers) = rt.approvals.get(&id) {
                        let _ = answers.send(line);
                    }
                }
                Effect::Copy(text) => {
                    use crossterm::clipboard::CopyToClipboard;
                    // OSC 52: the terminal sets the clipboard, so it works over ssh and in tmux
                    // (with `set-clipboard on`).
                    if let Err(err) = crossterm::execute!(
                        std::io::stdout(),
                        CopyToClipboard::to_clipboard_from(text)
                    ) {
                        local.push_back(Event::Notice(format!("copy: {err}")));
                    }
                }
                Effect::OpenLog { .. } if editing.is_some() => {
                    local.push_back(Event::Notice("/log is already open".into()));
                }
                Effect::OpenLog { editor } => match rt.write_log() {
                    Ok(None) => local.push_back(Event::Report("nothing to log yet".into())),
                    Err(err) => local.push_back(Event::Notice(format!("/log: {err:#}"))),
                    Ok(Some(path)) => {
                        // The editor gets the keyboard: stop reading it, then leave the screen.
                        let guard = InterruptGuard::hold();
                        input.abort();
                        let _ = (&mut input).await;
                        leave_screen()?;
                        let (tx, file) = (tx.clone(), path.clone());
                        tokio::task::spawn_blocking(move || {
                            let failure = run_editor(&editor, &file).err();
                            let _ = tx.blocking_send(Event::EditorClosed(failure));
                        });
                        editing = Some((path, guard));
                    }
                },
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
                    auto_review,
                    ask_tool,
                    identity,
                    mirror,
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
                    // Every CLI agent but the writer runs in its own mirror (ADR 0016),
                    // inside git only; the sync runs in the turn's task, off this loop.
                    let refuse = if !mirror {
                        None
                    } else if let Some(why) = &rt.checkout_error {
                        Some(format!("no mirror, git can't name the checkout: {why}"))
                    } else if rt.lock.is_none()
                        && !matches!(writer_lock::resolve(&rt.cwd), Ok(Checkout::Outside))
                    {
                        // Outside git at startup, but `git init` since: no running in it.
                        Some("this folder is in a git checkout now: restart roundtable".into())
                    } else if rt.lock.is_some() && rt.launch_sub.is_none() {
                        Some("no mirror: the launch folder can't be placed in the checkout".into())
                    } else if rt.cleaning.contains(&id) {
                        Some("its mirror is being removed (/worktrees clean); try again".into())
                    } else {
                        None
                    };
                    if let Some(why) = refuse {
                        // Fail closed: never fall back to running in the checkout.
                        local.push_back(Event::Agent(id.clone(), PEvent::Error(why)));
                        local.push_back(Event::Agent(id, PEvent::Done(Outcome::Failed)));
                        continue;
                    }
                    let mirror = (mirror && rt.lock.is_some()).then(|| rt.launch_sub.clone());
                    let mirror = mirror.flatten();
                    let cwd = match &mirror {
                        Some(sub) => worktree::cwd(&rt.data_root, &rt.checkout, &id, sub),
                        None => rt.cwd.clone(),
                    };
                    // The app's session says whether to resume at all (it drops one the CLI
                    // lost); which one is bound to the folder.
                    let found = rt.sessions.get(&session_key(&id, &identity, &cwd)).cloned();
                    if session.is_some() && found.is_none() {
                        local.push_back(Event::Notice(format!(
                            "{id}: new session, it now runs in {}",
                            cwd.display()
                        )));
                    }
                    let session = session.and(found);
                    let prompt = match &mirror {
                        Some(_) => format!(
                            "[roundtable: you are in a read-only mirror of `{}` at `{}`; give \
                             paths relative to the repo root]\n\n{prompt}",
                            rt.checkout.display(),
                            worktree::cwd(&rt.data_root, &rt.checkout, &id, Path::new(""))
                                .display()
                        ),
                        None => prompt,
                    };
                    // A relative command path (`../tools/codex`) was checked from the launch
                    // folder; from the mirror it would name another file.
                    let cmd =
                        if mirror.is_some() && cmd.contains('/') && Path::new(&cmd).is_relative() {
                            rt.cwd.join(&cmd).to_string_lossy().into_owned()
                        } else {
                            cmd
                        };
                    rt.turn_cwd.insert(id.clone(), cwd.clone());
                    let token = CancellationToken::new();
                    rt.turns.insert(id.clone(), token.clone());
                    // The writer's Claude turns may ask for approvals (ADR 0018): their answers
                    // go down this channel to the CLI's stdin.
                    let answers = (writable && kind == agent::Kind::Claude).then(|| {
                        let (answer_tx, answer_rx) = mpsc::unbounded_channel();
                        rt.approvals.insert(id.clone(), answer_tx);
                        answer_rx
                    });
                    let turn = Turn {
                        cmd,
                        prompt,
                        session,
                        cwd,
                        outside_repo: rt.outside_repo,
                        model,
                        writable,
                        permission_mode,
                        auto_review,
                        ask,
                        mirror: mirror.is_some(),
                    };
                    let tx = tx.clone();
                    let (data, checkout) = (rt.data_root.clone(), rt.checkout.clone());
                    // Only a mirror sync holds the quit barrier, and only while it syncs.
                    let syncing = mirror.is_some().then(|| rt.syncs.clone());
                    tokio::spawn(async move {
                        if let Some(sub) = mirror {
                            let who = id.clone();
                            // A cancel kills the sync's git and stops its copying.
                            let stop = Arc::new(AtomicBool::new(false));
                            let watch = tokio::spawn({
                                let (stop, token) = (stop.clone(), token.clone());
                                async move {
                                    token.cancelled().await;
                                    stop.store(true, Ordering::Relaxed);
                                }
                            });
                            let synced = tokio::task::spawn_blocking(move || {
                                worktree::sync(&data, &checkout, &who, &sub, &stop)
                            })
                            .await
                            .unwrap_or_else(|e| Err(e.to_string()));
                            watch.abort();
                            drop(syncing);
                            if token.is_cancelled() {
                                let done = PEvent::Done(Outcome::Interrupted);
                                let _ = tx.send(Event::Agent(id, done)).await;
                                return;
                            }
                            // Fail closed: never fall back to running in the checkout.
                            if let Err(why) = synced {
                                let why = format!("mirror sync failed: {why}");
                                let _ = tx.send(Event::Agent(id.clone(), PEvent::Error(why))).await;
                                let _ = tx
                                    .send(Event::Agent(id, PEvent::Done(Outcome::Failed)))
                                    .await;
                                return;
                            }
                        }
                        agent::run(id, kind, turn, tx, token, answers).await;
                    });
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
                    rt.viewing = None;
                    rt.sessions.clear();
                }
                Effect::Worktrees { clean, id } => {
                    let found = worktree::list(&rt.data_root, &rt.checkout);
                    let found: Vec<_> = found
                        .into_iter()
                        .filter(|(who, _)| id.as_ref().is_none_or(|id| id == who))
                        .collect();
                    let busy: Vec<String> = found
                        .iter()
                        .filter(|(who, _)| {
                            clean && (rt.turns.contains_key(who) || rt.cleaning.contains(who))
                        })
                        .map(|(who, _)| who.clone())
                        .collect();
                    // Reserved on this loop before any removal starts: a turn for one of
                    // them now fails instead of syncing into a folder being deleted.
                    let ids: Vec<String> = found
                        .iter()
                        .filter(|(who, _)| clean && !busy.contains(who))
                        .map(|(who, _)| who.clone())
                        .collect();
                    rt.cleaning.extend(ids.iter().cloned());
                    let tx = tx.clone();
                    // Removals hold the quit barrier too: the next roundtable must not
                    // sync a mirror this one is still deleting.
                    let removing = clean.then(|| rt.syncs.clone());
                    // Sizes and removals walk whole trees: off the loop.
                    tokio::task::spawn_blocking(move || {
                        let mut lines = vec![];
                        for (who, record) in &found {
                            if busy.contains(who) {
                                lines
                                    .push(format!("{who}: kept, its turn or a removal is running"));
                            } else if clean {
                                lines.push(match worktree::remove(who, record) {
                                    Ok(note) => format!("{who}: {note}"),
                                    Err(err) => format!("{who}: {err}"),
                                });
                            } else {
                                let mb = worktree::size(&record.mirror) as f64 / 1e6;
                                lines.push(format!(
                                    "{who}: {mb:.1} MB  {}",
                                    record.mirror.display()
                                ));
                            }
                        }
                        if lines.is_empty() {
                            lines.push("no mirrors for this checkout".into());
                        }
                        let report = lines.join("\n");
                        // Released before sending: a quitting loop waits on it and no longer
                        // drains the channel.
                        drop(removing);
                        let _ = tx.blocking_send(Event::WorktreesDone { ids, report });
                    });
                }
                Effect::HandoffEntries => {
                    let entries = match rt.thread.as_ref().map(Thread::events) {
                        None => Err("nothing to hand off yet".to_string()),
                        Some(Err(err)) => Err(format!("{err:#}")),
                        Some(Ok(events)) => Ok(roundtable::log::entries(&events, utc_offset_at)),
                    };
                    local.push_back(Event::HandoffEntries(entries));
                }
                Effect::Usage(scope) => {
                    let agents = app.cli_agents();
                    let agents: Vec<&str> = agents.iter().map(String::as_str).collect();
                    let limits = roundtable::limits::report(
                        &agents,
                        &app.shown_limits(),
                        now_ms(),
                        utc_offset_at,
                    );
                    let usage = rt.usage_report(&scope);
                    local.push_back(Event::Report(format!("{limits}\n{usage}")));
                }
                Effect::SaveLimits(limits) => {
                    let path = rt.data_root.join("limits.json");
                    let saved = serde_json::to_vec_pretty(&limits)
                        .map_err(std::io::Error::other)
                        .and_then(|bytes| transcript::write_atomic(&path, &bytes));
                    if let Err(err) = saved {
                        tracing::warn!(%err, "saving limits.json failed");
                    }
                }
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
                Effect::Record {
                    mut event,
                    durable,
                    link,
                } => {
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
                    match rt.record(event, durable, link) {
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
            rt.mirror_work_done().await;
            return Ok(());
        }
    }
}

/// Native sessions are bound to the folder the CLI ran in (ADR 0016): Claude Code only
/// resumes a session from the folder it was started in.
fn session_key(id: &str, identity: &str, cwd: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    // Lossless: `display()` would map two non-UTF-8 folders to one key.
    let cwd = match cwd.to_str() {
        Some(cwd) => cwd.to_string(),
        None => cwd
            .as_os_str()
            .as_bytes()
            .iter()
            .fold("hex:".to_string(), |s, b| format!("{s}{b:02x}")),
    };
    format!("{id}/{identity}/{cwd}")
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
    transcript::owned_private_dir(&dir).with_context(|| format!("using {}", dir.display()))?;
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
            Some(Ok(TermEvent::Resize(width, _))) => Event::Resize(width),
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

    /// A loop runtime over a fresh data folder, with no checkout lock.
    fn runtime(root: &std::path::Path) -> Runtime {
        Runtime {
            store: Store::open(root).unwrap(),
            thread: None,
            keys: HashMap::new(),
            viewing: None,
            approvals: HashMap::new(),
            sessions: BTreeMap::new(),
            turn_cwd: HashMap::new(),
            checkout_error: None,
            cleaning: HashSet::new(),
            syncs: Arc::new(()),
            launch_sub: Some(PathBuf::new()),
            turns: HashMap::new(),
            cwd: root.into(),
            checkout: root.into(),
            lock: None,
            writers: HashMap::new(),
            data_root: root.into(),
            secrets: vec![],
            ask_socket: root.join("ask.sock"),
            ask_tokens: HashMap::new(),
            ask_files: HashMap::new(),
            pending_asks: HashMap::new(),
            next_ask: 1,
            redactors: HashMap::new(),
            threads_changed: false,
            outside_repo: true,
            models_path: root.join("models.toml"),
            models_hash: None,
            checked: HashMap::new(),
        }
    }

    #[test]
    fn log_reads_a_thread_resumed_from_another_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let mut rt = runtime(dir.path());
        let mut thread = rt.store.create_thread("elsewhere", other.path()).unwrap();
        let mut app = App::default();
        for code in [KeyCode::Char('h'), KeyCode::Char('i'), KeyCode::Enter] {
            for effect in app.update(Event::Key(KeyEvent::new(code, KeyModifiers::NONE))) {
                if let Effect::Record { event, durable, .. } = effect {
                    thread.append(event, durable).unwrap();
                }
            }
        }
        drop(thread);
        assert!(matches!(
            rt.resume(Some("1")),
            Event::Resumed {
                elsewhere: Some(_),
                ..
            }
        ));
        assert!(rt.thread.is_none(), "never opened for writing");
        let path = rt.write_log().unwrap().expect("the shown thread is logged");
        let text = fs::read_to_string(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(text.starts_with("# elsewhere · "), "{text}");
    }

    #[test]
    fn paging_moves_the_view_even_with_a_line_selected() {
        let mut app = App::default();
        for n in 0..40 {
            app.note(format!("line {n}"), false);
        }
        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        // Draws as the loop does, feeding back where the chat was shown; returns the title.
        let draw = |app: &mut App| {
            let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
            let mut shown = 0;
            terminal.draw(|frame| shown = ui::draw(frame, app)).unwrap();
            app.update(Event::Viewport(shown));
            terminal.backend().buffer().content[..60]
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };
        app.update(key(KeyCode::Esc));
        assert!(!draw(&mut app).contains("scrolled up"));
        app.update(key(KeyCode::PageUp));
        assert!(draw(&mut app).contains("scrolled up 10 lines"));
        app.update(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        assert!(draw(&mut app).contains("scrolled up 20 lines"));
        // Moving the selection brings it back into view.
        app.update(key(KeyCode::Char('G')));
        assert!(!draw(&mut app).contains("scrolled up"));
        // The selection above the view, then PgUp: further up, never back down.
        for _ in 0..25 {
            app.update(key(KeyCode::Char('k')));
        }
        let before = draw(&mut app);
        let up = |t: &str| {
            t.split("scrolled up ")
                .nth(1)
                .and_then(|r| r.split(' ').next()?.parse::<u16>().ok())
                .unwrap_or(0)
        };
        app.update(key(KeyCode::PageUp));
        assert!(up(&draw(&mut app)) > up(&before), "{before}");
        // gg then PgDn before any redraw: the PgDn is not lost.
        app.update(key(KeyCode::Char('g')));
        app.update(key(KeyCode::Char('g')));
        app.update(key(KeyCode::PageDown));
        let top = up(&draw(&mut app));
        assert!(top > 0, "drawn once: at the top");
        assert_eq!(
            up(&draw(&mut app)),
            top - 10,
            "the PgDn is applied once the top is known"
        );
        app.update(key(KeyCode::Char('G')));
        draw(&mut app);
        // At the top after gg, PgDn comes down at once.
        app.update(key(KeyCode::Char('g')));
        app.update(key(KeyCode::Char('g')));
        let top = up(&draw(&mut app));
        app.update(key(KeyCode::PageDown));
        assert_eq!(up(&draw(&mut app)), top - 10);
    }

    #[test]
    fn log_folders_differ_for_paths_that_differ_only_in_non_utf8_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let a = Path::new(std::ffi::OsStr::from_bytes(b"/data/\xff"));
        let b = Path::new(std::ffi::OsStr::from_bytes(b"/data/\xfe"));
        assert_eq!(
            a.display().to_string(),
            b.display().to_string(),
            "lossy alike"
        );
        assert_ne!(log_dir(a), log_dir(b));
    }

    #[test]
    fn a_chat_longer_than_u16_rows_still_shows_its_newest_line() {
        let mut app = App::default();
        for n in 0..70_000 {
            app.note(format!("line {n}"), false);
        }
        app.update(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.update(Event::Key(KeyEvent::new(
            KeyCode::Char('G'),
            KeyModifiers::NONE,
        )));
        let screen = render(&app, 60, 12);
        let text: String = screen.buffer().content.iter().map(|c| c.symbol()).collect();
        assert!(text.contains("line 69999"), "the newest line is on screen");
        assert!(!text.contains("scrolled up"));
        // From the very top, PgDn moves ten rows, not thousands.
        let mut shown = 0;
        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        app.update(key(KeyCode::Char('g')));
        app.update(key(KeyCode::Char('g')));
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal
            .draw(|frame| shown = ui::draw(frame, &app))
            .unwrap();
        app.update(Event::Viewport(shown));
        app.update(key(KeyCode::PageDown));
        terminal
            .draw(|frame| shown = ui::draw(frame, &app))
            .unwrap();
        let top_row: String = terminal.backend().buffer().content[60..120]
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(top_row.contains("line 10 "), "{top_row}");
        // One entry alone past u16::MAX rows: its last line still shows at the bottom.
        let mut app = App::default();
        let huge: Vec<String> = (0..70_000).map(|n| format!("row {n}")).collect();
        app.note(huge.join("\n"), false);
        let screen = render(&app, 60, 12);
        let text: String = screen.buffer().content.iter().map(|c| c.symbol()).collect();
        assert!(
            text.contains("row 69999"),
            "the end of the huge entry is on screen"
        );
    }

    #[test]
    fn g_shows_the_end_of_a_last_reply_taller_than_the_pane() {
        let mut app = App::default();
        app.note("first".into(), false);
        let tall: Vec<String> = (0..50).map(|n| format!("row {n}")).collect();
        app.note(tall.join("\n"), false);
        let key = |c| Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        app.update(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.update(key('k'));
        app.update(key('G'));
        let screen = render(&app, 60, 12);
        let text: String = screen.buffer().content.iter().map(|c| c.symbol()).collect();
        assert!(text.contains("row 49"), "the reply's end, not its start");
        assert!(!text.contains("scrolled up"));
    }

    #[test]
    fn one_unbroken_line_past_u16_rows_shows_its_end() {
        let mut app = App::default();
        app.note(format!("{}END", "a".repeat(58 * 70_000)), false);
        let screen = render(&app, 60, 12);
        let text: String = screen.buffer().content.iter().map(|c| c.symbol()).collect();
        assert!(text.contains("END"));
    }

    #[test]
    fn g_in_a_huge_thread_list_shows_the_picked_thread() {
        let mut app = App::default();
        app.update(Event::Threads(
            (0..70_000)
                .map(|n| roundtable::app::ThreadRow {
                    id: format!("{n}"),
                    title: format!("thread {n}"),
                    day: "2026-11-25".into(),
                    elsewhere: false,
                })
                .collect(),
        ));
        app.update(Event::Resize(130));
        let key = |c| Event::Key(KeyEvent::new(c, KeyModifiers::NONE));
        app.update(key(KeyCode::Esc));
        app.update(key(KeyCode::Char('h')));
        app.update(key(KeyCode::Char('G')));
        let screen = render(&app, 130, 14);
        let text: String = screen.buffer().content.iter().map(|c| c.symbol()).collect();
        assert!(text.contains("thread 69999"));
    }

    #[test]
    fn an_interrupt_meant_for_the_editor_does_not_kill_roundtable() {
        let file = Path::new("/dev/null");
        let sh = |script: &str| vec!["sh".to_string(), "-c".into(), script.into()];
        // The "editor" interrupts roundtable (its parent): ignored while the guard is held.
        let guard = InterruptGuard::hold();
        assert_eq!(run_editor(&sh("kill -INT $PPID; exit 0"), file), Ok(()));
        // The editor itself still gets the default action.
        let err = run_editor(&sh("kill -INT $$; sleep 5"), file).unwrap_err();
        assert!(err.contains("exited with"), "{err}");
        drop(guard);
    }

    #[test]
    fn log_writes_the_thread_to_an_owner_only_temp_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut rt = runtime(dir.path());
        assert!(rt.write_log().unwrap().is_none(), "no thread yet");
        let mut app = App::default();
        for code in [KeyCode::Char('h'), KeyCode::Char('i'), KeyCode::Enter] {
            for effect in app.update(Event::Key(KeyEvent::new(code, KeyModifiers::NONE))) {
                if let Effect::Record {
                    event,
                    durable,
                    link,
                } = effect
                {
                    rt.record(event, durable, link).unwrap();
                }
            }
        }
        let path = rt.write_log().unwrap().expect("a file");
        let text = fs::read_to_string(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        fs::remove_file(&path).unwrap();
        assert!(text.starts_with("# hi · "), "{text}");
        assert!(text.contains("\n\nhi\n"), "{text}");
        assert_eq!(mode, 0o600);
        assert!(path.starts_with(log_dir(dir.path())));
        // Something the owner saved beside it is never treated as a snapshot.
        let notes = log_dir(dir.path()).join("notes.md");
        fs::write(&notes, "mine").unwrap();
        assert!(log_files(dir.path()).all(|f| f != notes));
        fs::remove_file(&notes).unwrap();
        // Another spelling of the same data folder (a symlink) names the same log folder.
        let link = tempfile::tempdir().unwrap();
        let alias = link.path().join("alias");
        std::os::unix::fs::symlink(dir.path(), &alias).unwrap();
        assert_eq!(log_dir(&alias), log_dir(dir.path()));
        assert!(log_files(dir.path()).all(|f| f != path));
        assert!(
            log_files(Path::new("/elsewhere")).all(|f| !f.starts_with(log_dir(dir.path()))),
            "another data folder's files are never listed"
        );
    }

    #[test]
    fn records_carry_turn_and_parent_as_written_seqs() {
        let dir = tempfile::tempdir().unwrap();
        let mut rt = runtime(dir.path());
        let mut app = App::default();
        let write = |effects: Vec<Effect>, rt: &mut Runtime| {
            for effect in effects {
                if let Effect::Record {
                    event,
                    durable,
                    link,
                } = effect
                {
                    rt.record(event, durable, link).unwrap();
                }
            }
        };
        // A status line first, so seqs and App keys can't line up by accident.
        rt.record(transcript::settle_event(), true, Link::default())
            .unwrap();
        let mut typed = vec![];
        for code in [KeyCode::Char('h'), KeyCode::Char('i'), KeyCode::Enter] {
            typed.extend(app.update(Event::Key(KeyEvent::new(code, KeyModifiers::NONE))));
        }
        write(typed, &mut rt);
        write(
            app.update(Event::Agent(
                "claude".into(),
                roundtable::agent::PEvent::Done(roundtable::agent::Outcome::Complete),
            )),
            &mut rt,
        );
        let events = rt.thread.as_ref().unwrap().events().unwrap();
        let by_kind = |kind| events.iter().find(|e| e.kind == kind).unwrap();
        let msg = events
            .iter()
            .find(|e| e.kind == transcript::Kind::Msg && e.from.id == "user")
            .unwrap();
        let delivery = by_kind(transcript::Kind::Delivery);
        let reply = events
            .iter()
            .find(|e| e.kind == transcript::Kind::Msg && e.from.id == "claude")
            .unwrap();
        assert_eq!((msg.turn, msg.parent), (None, None));
        assert_eq!(
            (delivery.turn, delivery.parent),
            (Some(msg.seq), Some(msg.seq))
        );
        assert_eq!(
            (reply.turn, reply.parent),
            (Some(msg.seq), Some(delivery.seq))
        );
    }

    fn render(app: &App, width: u16, height: u16) -> TestBackend {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, app);
            })
            .unwrap();
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
        // Esc to Normal mode, k up to the handoff, Enter expands it (ADR 0015).
        app.update(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        let handoff = app
            .lines
            .iter()
            .position(|l| l.kind == roundtable::app::LineKind::Handoff)
            .unwrap();
        while app.selected_line != Some(handoff) {
            app.update(Event::Key(KeyEvent::new(
                KeyCode::Char('k'),
                KeyModifiers::NONE,
            )));
        }
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
        terminal
            .draw(|frame| {
                ui::draw(frame, app);
            })
            .unwrap();
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
