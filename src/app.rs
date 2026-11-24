//! App state and its single update function (ADR 0002).
//! Only the main loop owns `App`; every other task talks to it through `Event`s.

use std::collections::{BTreeMap, VecDeque};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::json;

use crate::agent::{self, Outcome, PEvent};
use crate::registry::{Adapter, Participant, Registry};
use crate::transcript::{self, NewEvent, Party};

/// Preflight result per participant id: the CLI version, or why it can't take turns.
pub type Checks = BTreeMap<String, Result<String, String>>;

/// Everything that can happen to the app. Every source sends these into one channel.
#[derive(Debug)]
pub enum Event {
    Key(KeyEvent),
    Resize,
    /// Redraw throttle: at most one draw per tick.
    Tick,
    /// The terminal input stream ended or failed; nothing more can be typed.
    InputClosed(String),
    /// Output of a running turn of participant `id`.
    Agent(String, PEvent),
    /// A participant called `ask` (ADR 0007); the loop has checked its token, so `caller`
    /// is genuine. Answered with `Effect::Answer`.
    Asked {
        ask: u64,
        caller: String,
        to: String,
        prompt: String,
    },
    /// A connection on roundtable's socket asking on behalf of `request.caller`. Handled by
    /// the loop (it checks the token first), never by `update`.
    AskIncoming {
        request: crate::mcp::AskRequest,
        reply: Responder,
    },
    /// A chat turn resolved its API key: the loop adds it to the transcript redaction list.
    /// Handled by the loop, never by `update`.
    KeyResolved {
        reference: String,
        value: Secret,
    },
    /// The watcher saw `models.toml` change. The loop reads it (I/O) and turns this into
    /// `Registry`, so `update` never sees it.
    RegistryFileChanged,
    /// `models.toml` changed on disk: the new registry, or why it was not applied.
    Registry(Result<(Registry, Checks), String>),
    /// Something the loop failed to do, shown in the chat.
    Notice(String),
    /// An answer to a command the loop had to work out (`/usage`, `/model test`).
    Report(String),
    /// `/resume`: the loop opened a past thread; the app rebuilds it.
    Resumed {
        title: String,
        events: Vec<transcript::EventLine>,
        /// `sessions.json`: `id/identity -> native session`.
        sessions: BTreeMap<String, String>,
        /// The thread belongs to another checkout: read it, send nothing (ADR 0010).
        elsewhere: Option<std::path::PathBuf>,
    },
    /// After `/quiet`: the marker was removed durably, or why not.
    QuietDone(Result<(), String>),
    /// The loop found a reason to lock edits mid-run (another instance crashed, or the
    /// marker changed before `/quiet`).
    LockEdits(EditLock),
}

/// Why edits are locked (ADR 0012).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditLock {
    /// A writer crashed and left this marker (`text` to show); `/quiet` removes exactly the
    /// marker with this `key`.
    Crashed { text: String, key: String },
    /// The checkout can't be named reliably (a git problem): fix it and restart.
    Unknown(String),
    /// Not a git checkout: nobody edits here.
    OutsideGit,
}

/// Where an `ask`'s answer goes: the waiting `roundtable mcp` connection.
pub struct Responder(pub tokio::sync::oneshot::Sender<crate::mcp::AskAnswer>);

impl std::fmt::Debug for Responder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Responder")
    }
}

/// A secret value whose `Debug` never shows it, so no event dump can leak a key.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// What `/model test` pings.
#[derive(Debug, Clone, PartialEq)]
pub enum TestTarget {
    /// Rerun the CLI's preflight (version, repo and config checks).
    Cli { kind: agent::Kind, cmd: String },
    /// One minimal request to the chat API.
    Http {
        base_url: String,
        model: String,
        key_ref: Option<String>,
    },
}

/// A change to `models.toml` asked for by a command; the loop writes it.
#[derive(Debug, Clone, PartialEq)]
pub enum FileEdit {
    Enabled {
        id: String,
        enabled: bool,
    },
    Default(String),
    /// `[talk] enabled`, the global switch for model-to-model talk.
    Talk(bool),
    /// `[talk] allow.<from>`: the full list of who `from` may ask.
    Allow {
        from: String,
        targets: Vec<String>,
    },
    Budget(u64),
    Turns(u32),
}

/// What `update` asks the loop to do. The loop executes these; results come back as `Event`s.
#[derive(Debug, PartialEq)]
pub enum Effect {
    Redraw,
    Quit,
    /// A chat turn for an OpenAI-compatible model (ADR 0005); the loop resolves the key.
    StartChat {
        id: String,
        base_url: String,
        model: String,
        key_ref: Option<String>,
        system: String,
        history: std::sync::Arc<Vec<(String, String)>>,
        prompt: String,
        context_window: u64,
        max_output: u64,
        connect_timeout_s: u64,
        idle_timeout_s: u64,
    },
    StartTurn {
        id: String,
        kind: agent::Kind,
        cmd: String,
        model: Option<String>,
        prompt: String,
        session: Option<String>,
        /// The writer's own turn, which may edit files (ADR 0004).
        writable: bool,
        permission_mode: Option<String>,
        /// Give this turn the `ask` tool (ADR 0007); the loop builds the bridge.
        ask_tool: bool,
    },
    /// Cancel the running turn of one participant.
    CancelTurn(String),
    /// Cancel every running turn (`Esc`).
    CancelTurns,
    /// Append to the transcript; the loop opens the thread on the first one.
    Record {
        event: NewEvent,
        durable: bool,
    },
    /// Persist a native session id, bound to the participant's identity (ADR 0010).
    SaveSession {
        id: String,
        identity: String,
        session: String,
    },
    EditRegistry(FileEdit),
    /// The answer to an `ask`, back to the calling model as its tool result.
    Answer {
        ask: u64,
        text: String,
        refused: bool,
    },
    /// The user confirmed the checkout is quiet: remove the crash marker with this key
    /// (ADR 0012).
    ConfirmQuiet(String),
    /// `/clear`: the next message opens a new thread.
    NewThread,
    /// `/resume`: list past threads (`None`) or reopen one (a number from the list, or an id).
    Resume(Option<String>),
    /// `/usage <scope>`: the loop sums the transcript's usage files.
    Usage(String),
    /// `/model test <id>`: ping it and report latency or the error.
    TestModel {
        id: String,
        target: TestTarget,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// One participant asking another (ADR 0007): shown inline, with the prompt.
    Handoff,
    User,
    Reply(Outcome),
    Streaming,
    Tool,
    Info,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatLine {
    pub who: String,
    pub text: String,
    pub kind: LineKind,
}

/// Runtime state of one participant; survives registry reloads for as long as it runs.
#[derive(Debug)]
struct Seat {
    id: String,
    busy: bool,
    session: Option<String>,
    /// Direct messages waiting while busy (ADR 0007: queued, not dropped), each with
    /// whether it may edit.
    queue: VecDeque<(String, bool, u64)>,
    /// The user message the running turn descends from: limits count per origin.
    origin: u64,
    /// The `ask` this turn answers, and who asked.
    answering: Option<u64>,
    asked_by: Option<String>,
    /// The running turn may edit; its reply then ends the writer marker.
    writable: bool,
    /// The front of `queue` is an edit waiting for another writer to finish (ADR 0004: one
    /// writable turn at a time). The seat counts as occupied, so later messages queue
    /// behind the edit instead of overtaking it.
    held: bool,
    /// Index of the reply being streamed in `lines`.
    reply: Option<usize>,
    /// Input and output tokens, cost, and whether roundtable estimated them (ADR 0005);
    /// `None` until the turn reports usage, which a cut-off turn may never do.
    usage: Option<(u64, u64, Option<f64>, bool)>,
    /// An OpenAI-compatible model's own history, as prompt/reply pairs (ADR 0005); it
    /// starts empty for a new destination.
    // shortcut: kept in memory for the open thread; `/resume` (ADR 0008) rebuilds it from
    // the transcript.
    history: std::sync::Arc<Vec<(String, String)>>,
    /// The running turn's prompt, added to `history` when the reply completes.
    prompt: Option<String>,
    /// The identity the running turn was sent to. Its events belong to that destination,
    /// even if the registry changed meanwhile (ADR 0011).
    turn_identity: Option<String>,
    /// Tokens sent and received in this thread (for `/context`).
    thread_tokens: (u64, u64),
}

#[derive(Debug)]
pub struct App {
    /// State changed since the last draw.
    dirty: bool,
    pub lines: Vec<ChatLine>,
    pub input: String,
    registry: Registry,
    checks: Checks,
    seats: Vec<Seat>,
    /// Why nobody may edit right now; `None` lets the writer edit.
    edit_lock: Option<EditLock>,
    /// Lines scrolled up from the newest (PgUp/PgDn); 0 follows the conversation.
    pub scroll: u16,
    /// A resumed thread from another checkout: nothing may be sent into it (ADR 0010).
    read_only: Option<std::path::PathBuf>,
    /// Quitting: running turns are being cancelled and nothing new may start.
    quitting: bool,
    /// The global talk switch (`[talk] enabled`, `/talk on|off`).
    talk_on: bool,
    /// Per user message: model-to-model turns started and tokens they used (ADR 0007).
    origins: BTreeMap<u64, (u32, u64)>,
    next_origin: u64,
    /// First Ctrl-C arms, the second quits.
    quit_armed: bool,
}

impl Default for App {
    /// The default registry with every CLI available; the binary passes real ones to `new`.
    fn default() -> Self {
        let registry = Registry::parse(crate::registry::DEFAULT_FILE).expect("valid default");
        let checks = registry
            .participants
            .iter()
            .map(|p| (p.id.clone(), Ok("test".into())))
            .collect();
        Self::new(registry, checks)
    }
}

impl App {
    pub fn new(registry: Registry, checks: Checks) -> Self {
        let mut app = Self {
            // Dirty from the start, so the first tick draws the screen.
            dirty: true,
            lines: vec![],
            input: String::new(),
            registry: Registry::parse("").expect("empty registry"),
            checks: Checks::new(),
            seats: vec![],
            edit_lock: None,
            scroll: 0,
            read_only: None,
            quitting: false,
            talk_on: true,
            origins: BTreeMap::new(),
            next_origin: 1,
            quit_armed: false,
        };
        app.apply_registry(registry, checks, false);
        app
    }

    /// Adds a line before the first draw, e.g. startup notes.
    pub fn note(&mut self, text: String, is_error: bool) {
        let kind = if is_error {
            LineKind::Error
        } else {
            LineKind::Info
        };
        self.push("roundtable", text, kind);
    }

    /// At startup: a previous writer in this checkout may still be running (ADR 0010).
    pub fn lock_edits(&mut self, lock: EditLock) {
        let text = match &lock {
            EditLock::Crashed { text: marker, .. } => format!(
                "edits are locked: a writer never finished in this checkout and may still be \
                 running. Check nothing is editing it, then type /quiet. Its marker:\n{marker}"
            ),
            EditLock::Unknown(why) => format!(
                "edits are locked: git can't tell this checkout ({why}), so a writer elsewhere \
                 in it could be missed. Fix that, then restart."
            ),
            EditLock::OutsideGit => {
                "not a git checkout: every turn is read-only here (ADR 0012)".to_string()
            }
        };
        let kind = if lock == EditLock::OutsideGit {
            LineKind::Info
        } else {
            LineKind::Error
        };
        self.push("roundtable", text, kind);
        self.edit_lock = Some(lock);
    }

    /// How to lift the lock, for messages.
    fn unlock_hint(&self) -> &'static str {
        match self.edit_lock {
            Some(EditLock::Crashed { .. }) => "until /quiet",
            Some(EditLock::Unknown(_)) => "until the git problem is fixed and roundtable restarted",
            Some(EditLock::OutsideGit) | None => "outside a git checkout",
        }
    }

    fn locked(&self) -> bool {
        self.edit_lock.is_some()
    }

    /// The only place state changes. No I/O here, so it is testable without a terminal.
    pub fn update(&mut self, event: Event) -> Vec<Effect> {
        if let Event::Tick = event {
            if !self.dirty {
                return vec![];
            }
            self.dirty = false;
            return vec![Effect::Redraw];
        }
        self.dirty = true;
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.key(key),
            Event::Key(_)
            | Event::Resize
            | Event::Tick
            | Event::RegistryFileChanged
            | Event::KeyResolved { .. }
            | Event::AskIncoming { .. } => vec![],
            Event::InputClosed(reason) => {
                tracing::error!(%reason, "terminal input closed");
                self.quit()
            }
            Event::Agent(id, event) => self.agent_event(&id, event),
            Event::Asked {
                ask,
                caller,
                to,
                prompt,
            } => self.asked(ask, caller, to, prompt),
            Event::Registry(Ok((registry, checks))) => self.apply_registry(registry, checks, true),
            Event::Registry(Err(why)) => {
                self.push(
                    "roundtable",
                    format!("models.toml not applied, the previous registry stays: {why}"),
                    LineKind::Error,
                );
                vec![]
            }
            Event::QuietDone(Ok(())) => {
                self.edit_lock = None;
                self.push("roundtable", "edits unlocked".into(), LineKind::Info);
                vec![]
            }
            Event::QuietDone(Err(why)) => {
                self.push(
                    "roundtable",
                    format!("edits are still locked: {why}"),
                    LineKind::Error,
                );
                vec![]
            }
            Event::LockEdits(lock) => {
                self.lock_edits(lock);
                vec![]
            }
            Event::Resumed {
                title,
                events,
                sessions,
                elsewhere,
            } => self.resume(title, &events, &sessions, elsewhere),
            Event::Report(text) => {
                self.push("roundtable", text, LineKind::Info);
                vec![]
            }
            Event::Notice(text) => {
                self.push("roundtable", text, LineKind::Error);
                vec![]
            }
        }
    }

    fn key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            if self.quit_armed {
                return self.quit();
            }
            self.quit_armed = true;
            self.push("roundtable", "Ctrl-C again to quit".into(), LineKind::Info);
            return vec![];
        }
        self.quit_armed = false;
        match key.code {
            KeyCode::Char('d') if ctrl && self.input.is_empty() => self.quit(),
            KeyCode::Char(c) if !ctrl => {
                self.input.push(c);
                vec![]
            }
            KeyCode::Backspace => {
                self.input.pop();
                vec![]
            }
            // Alt-Enter everywhere, Shift-Enter where the terminal reports it: a new line.
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
                self.input.push('\n');
                vec![]
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Esc => self.cancel_all(),
            KeyCode::Tab => {
                if let Some(first) = self.completions().into_iter().next() {
                    let keep = self.input.rfind(' ').map_or(0, |i| i + 1);
                    self.input.truncate(keep);
                    self.input.push_str(&first);
                    self.input.push(' ');
                }
                vec![]
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(10);
                vec![]
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(10);
                vec![]
            }
            _ => vec![],
        }
    }

    /// From here on nothing new starts: queued and waiting work is dropped while the loop
    /// cancels what runs.
    fn quit(&mut self) -> Vec<Effect> {
        self.quitting = true;
        for seat in &mut self.seats {
            seat.queue.clear();
            seat.held = false;
        }
        vec![Effect::Quit]
    }

    /// What `Tab` would complete the input to (ADR 0008), for the popup.
    pub fn completions(&self) -> Vec<String> {
        let ids: Vec<String> = self
            .registry
            .participants
            .iter()
            .filter(|p| p.enabled)
            .map(|p| p.id.clone())
            .collect();
        crate::commands::complete(&self.input, &ids)
    }

    fn submit(&mut self) -> Vec<Effect> {
        let mut input = std::mem::take(&mut self.input);
        self.scroll = 0;
        if input.trim_start().starts_with('/') {
            return self.command(input.trim());
        }
        if let Some(checkout) = &self.read_only {
            let why = format!(
                "this thread belongs to {}: read-only here (/clear starts a new one)",
                checkout.display()
            );
            self.input = input;
            self.push("roundtable", why, LineKind::Error);
            return vec![];
        }
        // `\/` sends a literal slash instead of running a command.
        if let Some(rest) = input.trim_start().strip_prefix("\\/") {
            input = format!("/{rest}");
        }
        let (targets, prompt, broadcast) = match self.route(&input) {
            Ok(routed) => routed,
            Err(why) => {
                self.input = input; // keep what was typed, so it can be fixed
                self.push("roundtable", why, LineKind::Error);
                return vec![];
            }
        };
        // Only a message to the writer alone may edit: `@all` and several mentions are
        // read-only for everyone (ADR 0004).
        let to_writer =
            !broadcast && targets.len() == 1 && self.registry.writer() == Some(targets[0].as_str());
        let writable = to_writer && !self.locked();
        let edits = if writable { " (may edit)" } else { "" };
        self.push(
            &format!("you → {}{edits}", targets.join(", ")),
            prompt.clone(),
            LineKind::User,
        );
        if to_writer && !writable {
            self.push(
                "roundtable",
                format!("read-only: edits are locked {}", self.unlock_hint()),
                LineKind::Info,
            );
        }
        let to = targets.iter().map(|id| self.party(id)).collect();
        let mut effects = vec![record(
            user(),
            to,
            transcript::Kind::Msg,
            json!({ "text": prompt }),
        )];
        let origin = self.next_origin;
        self.next_origin += 1;
        for id in targets {
            effects.extend(self.deliver(&id, prompt.clone(), writable, origin));
        }
        effects
    }

    // shortcut: only the registry commands of ADR 0011; ADR 0008 brings the full command table.
    fn command(&mut self, line: &str) -> Vec<Effect> {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["/help"] => {
                self.push("roundtable", crate::commands::help(), LineKind::Info);
                vec![]
            }
            ["/exit"] => self.quit(),
            ["/clear"] => self.clear(),
            ["/resume"] => vec![Effect::Resume(None)],
            ["/resume", which] => {
                if self.seats.iter().any(|s| s.busy) {
                    self.push(
                        "roundtable",
                        "replies are still running; Esc cancels them first".into(),
                        LineKind::Error,
                    );
                    return vec![];
                }
                vec![Effect::Resume(Some(which.to_string()))]
            }
            ["/status"] => {
                let text = self.status_text();
                self.push("roundtable", text, LineKind::Info);
                vec![]
            }
            ["/usage"] => vec![Effect::Usage("thread".into())],
            ["/usage", scope @ ("thread" | "today" | "all")] => {
                vec![Effect::Usage(scope.to_string())]
            }
            ["/context"] => self.context(None),
            ["/context", id] => self.context(Some(id)),
            ["/model", "test", id] => {
                let Some(p) = self.registry.get(id).cloned() else {
                    self.push("roundtable", format!("no model `{id}`"), LineKind::Error);
                    return vec![];
                };
                let target = match p.adapter {
                    Adapter::Cli(kind) => TestTarget::Cli {
                        kind,
                        cmd: p.cmd.clone().unwrap_or_else(|| kind.cmd().into()),
                    },
                    Adapter::OpenAi => TestTarget::Http {
                        base_url: p.base_url.clone().unwrap_or_default(),
                        model: p.model.clone().unwrap_or_default(),
                        key_ref: p.key.clone(),
                    },
                };
                self.push("roundtable", format!("testing {id}…"), LineKind::Info);
                vec![Effect::TestModel {
                    id: id.to_string(),
                    target,
                }]
            }
            ["/talk", "show"] => {
                let text = self.talk_text();
                self.push("roundtable", text, LineKind::Info);
                vec![]
            }
            ["/talk", verb @ ("allow" | "deny"), pair] => self.set_allow(pair, *verb == "allow"),
            ["/budget", n] => match n.parse::<u64>() {
                Ok(n) => {
                    self.registry.talk.budget_tokens = n;
                    self.talk_changed(
                        format!("token budget per message is now {n}"),
                        FileEdit::Budget(n),
                    )
                }
                Err(_) => self.usage_error("/budget <tokens>"),
            },
            ["/turns", n] => match n.parse::<u32>() {
                Ok(n) => {
                    self.registry.talk.max_turns = n;
                    self.talk_changed(
                        format!("model-to-model turns per message: {n}"),
                        FileEdit::Turns(n),
                    )
                }
                Err(_) => self.usage_error("/turns <n>"),
            },
            ["/models"] => {
                let text = self.models_table();
                self.push("roundtable", text, LineKind::Info);
                vec![]
            }
            ["/model", "enable", id] => self.set_enabled(id, true),
            ["/model", "disable", id] => self.set_enabled(id, false),
            ["/model", "use", id] => self.use_default(id),
            ["/talk", state @ ("on" | "off")] => {
                let on = *state == "on";
                self.talk_on = on;
                self.registry.talk.enabled = on;
                let note = format!("talk between models is {state}");
                self.push("roundtable", note.clone(), LineKind::Info);
                vec![status(note), Effect::EditRegistry(FileEdit::Talk(on))]
            }
            ["/quiet"] => match &self.edit_lock {
                // Lifted only when `QuietDone` confirms the marker is gone from disk.
                Some(EditLock::Crashed { key, .. }) => vec![Effect::ConfirmQuiet(key.clone())],
                Some(_) => {
                    let hint =
                        format!("/quiet can't help: edits are locked {}", self.unlock_hint());
                    self.push("roundtable", hint, LineKind::Error);
                    vec![]
                }
                None => {
                    self.push("roundtable", "edits are not locked".into(), LineKind::Info);
                    vec![]
                }
            },
            _ => {
                let name = words.first().copied().unwrap_or("");
                let hint = match crate::commands::COMMANDS.iter().find(|c| c.name == name) {
                    Some(c) => format!("usage: {} {}", c.name, c.args),
                    None => format!("no command {name}; /help lists them"),
                };
                self.push("roundtable", hint, LineKind::Error);
                vec![]
            }
        }
    }

    /// Rebuilds a past thread from its transcript (`/resume`): the chat, each CLI's native
    /// session and each OpenAI model's history, but only for participants still pointing
    /// where they did then (identity), so nothing reaches a new destination.
    fn resume(
        &mut self,
        title: String,
        events: &[transcript::EventLine],
        sessions: &BTreeMap<String, String>,
        elsewhere: Option<std::path::PathBuf>,
    ) -> Vec<Effect> {
        self.lines.clear();
        self.origins.clear();
        self.scroll = 0;
        for seat in &mut self.seats {
            seat.session = None;
            seat.history = Default::default();
            seat.queue.clear();
            seat.thread_tokens = (0, 0);
        }
        let current: BTreeMap<String, String> = self
            .registry
            .participants
            .iter()
            .map(|p| (p.id.clone(), p.identity.clone()))
            .collect();
        let same = |party: &transcript::Party| {
            party.identity.is_some() && current.get(&party.id) == party.identity.as_ref()
        };
        // Per participant: the prompt of its latest delivery, waiting for its reply.
        let mut pending: BTreeMap<String, String> = BTreeMap::new();
        for e in events {
            let text = |key: &str| e.payload[key].as_str().unwrap_or_default().to_string();
            match e.kind {
                transcript::Kind::Msg if e.from.id == "user" => {
                    let to: Vec<&str> = e.to.iter().map(|p| p.id.as_str()).collect();
                    self.push(
                        &format!("you → {}", to.join(", ")),
                        text("text"),
                        LineKind::User,
                    );
                }
                transcript::Kind::Msg => {
                    let outcome = match e.payload["status"].as_str() {
                        Some("interrupted") => Outcome::Interrupted,
                        Some("failed") => Outcome::Failed,
                        _ => Outcome::Complete,
                    };
                    let who = match e.to.first().map(|p| p.id.as_str()) {
                        Some(to) if to != "user" => format!("{} → {to}", e.from.id),
                        _ => e.from.id.clone(),
                    };
                    self.push(&who, text("text"), LineKind::Reply(outcome));
                    if let Some(seat) = self.seat_mut(&e.from.id) {
                        seat.thread_tokens.0 += e.tokens_in.unwrap_or(0);
                        seat.thread_tokens.1 += e.tokens_out.unwrap_or(0);
                    }
                    let prompt = pending.remove(&e.from.id);
                    if outcome == Outcome::Complete
                        && same(&e.from)
                        && let Some(prompt) = prompt
                        && let Some(seat) = self.seat_mut(&e.from.id)
                    {
                        std::sync::Arc::make_mut(&mut seat.history).push((prompt, text("text")));
                    }
                }
                transcript::Kind::Delivery if e.payload["status"] == "sent" => {
                    for to in e.to.iter().filter(|p| same(p)) {
                        pending.insert(to.id.clone(), text("text"));
                    }
                }
                transcript::Kind::Handoff => {
                    let to = e.to.first().map(|p| p.id.as_str()).unwrap_or("?");
                    let who = format!("{} → {to}", e.from.id);
                    if e.payload["status"] == "blocked" {
                        self.push(&who, format!("blocked: {}", text("reason")), LineKind::Info);
                    } else {
                        self.push(&who, text("prompt"), LineKind::Handoff);
                    }
                }
                transcript::Kind::ToolCall => {
                    self.push(&e.from.id, text("summary"), LineKind::Tool);
                }
                transcript::Kind::Error => {
                    self.push(&e.from.id, text("text"), LineKind::Error);
                }
                _ => {}
            }
        }
        // Native CLI sessions, keyed by the identity they were made with.
        for p in self.registry.participants.clone() {
            if let Some(session) = sessions.get(&format!("{}/{}", p.id, p.identity))
                && let Some(seat) = self.seat_mut(&p.id)
            {
                seat.session = Some(session.clone());
            }
        }
        let note = match &elsewhere {
            Some(checkout) => format!(
                "resumed \"{title}\" read-only: it belongs to {}; /clear starts a new thread here",
                checkout.display()
            ),
            None => format!("resumed \"{title}\""),
        };
        self.read_only = elsewhere;
        self.push("roundtable", note, LineKind::Info);
        vec![]
    }

    fn usage_error(&mut self, usage: &str) -> Vec<Effect> {
        self.push("roundtable", format!("usage: {usage}"), LineKind::Error);
        vec![]
    }

    fn talk_changed(&mut self, note: String, edit: FileEdit) -> Vec<Effect> {
        self.push("roundtable", note.clone(), LineKind::Info);
        vec![status(note), Effect::EditRegistry(edit)]
    }

    /// `/clear`: a fresh thread, sessions and model histories included.
    fn clear(&mut self) -> Vec<Effect> {
        if self.seats.iter().any(|s| s.busy) {
            self.push(
                "roundtable",
                "replies are still running; Esc cancels them first".into(),
                LineKind::Error,
            );
            return vec![];
        }
        self.lines.clear();
        self.origins.clear();
        self.scroll = 0;
        self.read_only = None;
        for seat in &mut self.seats {
            seat.session = None;
            seat.history = Default::default();
            seat.queue.clear();
            seat.thread_tokens = (0, 0);
        }
        self.push("roundtable", "new thread".into(), LineKind::Info);
        vec![Effect::NewThread]
    }

    fn status_text(&self) -> String {
        let mut rows = vec![];
        for p in &self.registry.participants {
            let state = match (p.enabled, self.availability(p), self.checks.get(&p.id)) {
                (false, ..) => "disabled".to_string(),
                (true, Err(why), _) => format!("unavailable: {why}"),
                (true, Ok(()), Some(Ok(version))) => format!("ready ({version})"),
                (true, Ok(()), _) => "ready".to_string(),
            };
            let busy = if self.seats.iter().any(|s| s.id == p.id && s.busy) {
                ", answering"
            } else {
                ""
            };
            rows.push(format!("{}: {state}{busy}", p.id));
        }
        rows.push(format!(
            "talk: {}; writer: {}; edits: {}",
            if self.talk_on { "on" } else { "off" },
            self.registry.writer().unwrap_or("none"),
            if self.locked() {
                format!("locked {}", self.unlock_hint())
            } else {
                "unlocked".to_string()
            }
        ));
        rows.join("\n")
    }

    /// `/context [id]` (ADR 0008): for a CLI agent only what roundtable sent and the usage it
    /// reported (its own compaction is invisible); for an OpenAI model the estimated fill.
    fn context(&mut self, only: Option<&str>) -> Vec<Effect> {
        if let Some(id) = only
            && self.registry.get(id).is_none()
        {
            self.push("roundtable", format!("no model `{id}`"), LineKind::Error);
            return vec![];
        }
        let mut rows = vec![];
        for p in &self.registry.participants {
            if only.is_some_and(|id| id != p.id) {
                continue;
            }
            let Some(seat) = self.seats.iter().find(|s| s.id == p.id) else {
                continue;
            };
            let (sent, received) = seat.thread_tokens;
            rows.push(match p.adapter {
                Adapter::Cli(_) => format!(
                    "{}: {sent} tokens in, {received} out this thread; window fill unknown (the CLI keeps and compacts its own context)",
                    p.id
                ),
                Adapter::OpenAi => {
                    let window = p.context_window.unwrap_or(crate::openai::DEFAULT_CONTEXT_WINDOW);
                    let used: u64 = seat
                        .history
                        .iter()
                        .map(|(q, a)| crate::openai::estimate(q) + crate::openai::estimate(a))
                        .sum();
                    format!(
                        "{}: ~{used} of {window} tokens of history (estimate, {} exchanges); {sent} in, {received} out this thread",
                        p.id,
                        seat.history.len()
                    )
                }
            });
        }
        self.push("roundtable", rows.join("\n"), LineKind::Info);
        vec![]
    }

    fn talk_text(&self) -> String {
        let talk = &self.registry.talk;
        let mut rows = vec![format!(
            "talk is {}; per message: {} turns, {} tokens",
            if self.talk_on { "on" } else { "off" },
            talk.max_turns,
            talk.budget_tokens
        )];
        for p in &self.registry.participants {
            let targets = talk.allow.get(&p.id).filter(|t| !t.is_empty());
            rows.push(match targets {
                Some(t) => format!("{} may ask {}", p.id, t.join(", ")),
                None => format!("{} may ask no one", p.id),
            });
        }
        rows.join("\n")
    }

    /// `/talk allow a->b` / `/talk deny a->b`.
    fn set_allow(&mut self, pair: &str, allow: bool) -> Vec<Effect> {
        let Some((from, to)) = pair.split_once("->") else {
            return self.usage_error("/talk allow|deny <a>-><b>");
        };
        for id in [from, to] {
            if self.registry.get(id).is_none() {
                self.push("roundtable", format!("no model `{id}`"), LineKind::Error);
                return vec![];
            }
        }
        let targets = self
            .registry
            .talk
            .allow
            .entry(from.to_string())
            .or_default();
        targets.retain(|t| t != to);
        if allow {
            targets.push(to.to_string());
        }
        let targets = targets.clone();
        let note = format!(
            "{from} {} ask {to}",
            if allow { "may now" } else { "may no longer" }
        );
        self.talk_changed(
            note,
            FileEdit::Allow {
                from: from.to_string(),
                targets,
            },
        )
    }

    fn models_table(&self) -> String {
        let mut rows = vec![];
        for p in &self.registry.participants {
            let mut tags = vec![];
            if self.registry.default() == Some(p.id.as_str()) {
                tags.push("default");
            }
            if self.registry.writer() == Some(p.id.as_str()) {
                tags.push(if !self.locked() {
                    "writer"
                } else {
                    "writer, locked"
                });
            }
            let state = match (p.enabled, self.availability(p)) {
                (false, _) => "disabled".to_string(),
                (true, Ok(())) => "ready".to_string(),
                (true, Err(why)) => format!("unavailable: {why}"),
            };
            let kind = match p.adapter {
                Adapter::Cli(kind) => kind.id(),
                Adapter::OpenAi => "openai",
            };
            let model = p
                .model
                .as_deref()
                .map(|m| format!(", {m}"))
                .unwrap_or_default();
            let tags = if tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", tags.join(", "))
            };
            rows.push(format!("{} ({kind}{model}) {state}{tags}", p.id));
        }
        if rows.is_empty() {
            "no models: add one to models.toml".into()
        } else {
            rows.join("\n")
        }
    }

    fn set_enabled(&mut self, id: &str, enabled: bool) -> Vec<Effect> {
        if self.registry.get(id).is_none() {
            self.push("roundtable", format!("no model `{id}`"), LineKind::Error);
            return vec![];
        }
        if !enabled && self.registry.default() == Some(id) {
            self.push(
                "roundtable",
                format!("`{id}` is the default: /model use another first"),
                LineKind::Error,
            );
            return vec![];
        }
        let mut next = self.registry.clone();
        for p in &mut next.participants {
            if p.id == id {
                p.enabled = enabled;
            }
        }
        let checks = self.checks.clone();
        let mut effects = self.apply_registry(next, checks, true);
        effects.push(Effect::EditRegistry(FileEdit::Enabled {
            id: id.into(),
            enabled,
        }));
        effects
    }

    fn use_default(&mut self, id: &str) -> Vec<Effect> {
        if !self.registry.get(id).is_some_and(|p| p.enabled) {
            self.push(
                "roundtable",
                format!("`{id}` is not an enabled model"),
                LineKind::Error,
            );
            return vec![];
        }
        self.registry.default = Some(id.into());
        let note = format!("default is now {id}");
        self.push("roundtable", note.clone(), LineKind::Info);
        vec![
            status(note),
            Effect::EditRegistry(FileEdit::Default(id.into())),
        ]
    }

    /// Applies a new registry as a diff against the live one (ADR 0011).
    fn apply_registry(&mut self, next: Registry, checks: Checks, announce: bool) -> Vec<Effect> {
        let mut effects = vec![];
        let mut notes = vec![];
        for p in &next.participants {
            let before = self.registry.get(&p.id).cloned();
            match &before {
                None => notes.push(format!("{} joined", p.id)),
                Some(old) if old.enabled && !p.enabled => notes.push(format!("{} disabled", p.id)),
                Some(old) if !old.enabled && p.enabled => notes.push(format!("{} enabled", p.id)),
                _ => {}
            }
            let identity_changed = before.is_some_and(|old| old.identity != p.identity);
            if identity_changed {
                notes.push(format!(
                    "{} now points to a new destination; its history is not carried over",
                    p.id
                ));
            }
            if !p.enabled || identity_changed {
                effects.extend(self.unseat(&p.id));
            }
            if identity_changed && let Some(seat) = self.seat_mut(&p.id) {
                seat.session = None;
                seat.history = Default::default();
            }
        }
        let gone: Vec<String> = self
            .registry
            .participants
            .iter()
            .filter(|old| next.get(&old.id).is_none())
            .map(|old| old.id.clone())
            .collect();
        for id in gone {
            notes.push(format!("{id} left"));
            effects.extend(self.unseat(&id));
            // Coming back later is a fresh start, whatever it points to then.
            if let Some(seat) = self.seat_mut(&id) {
                seat.session = None;
                seat.history = Default::default();
            }
        }
        for p in &next.participants {
            if self.seat_mut(&p.id).is_none() {
                self.seats.push(Seat {
                    id: p.id.clone(),
                    busy: false,
                    session: None,
                    queue: VecDeque::new(),
                    origin: 0,
                    answering: None,
                    asked_by: None,
                    writable: false,
                    held: false,
                    reply: None,
                    usage: None,
                    history: Default::default(),
                    prompt: None,
                    turn_identity: None,
                    thread_tokens: (0, 0),
                });
            }
        }
        let warnings = next.warnings.clone();
        // The file is the truth for the talk switch too.
        self.talk_on = next.talk.enabled;
        self.registry = next;
        self.checks = checks;
        if announce {
            for note in notes {
                self.push("roundtable", note.clone(), LineKind::Info);
                effects.push(status(note));
            }
        }
        for warning in warnings {
            self.push("roundtable", warning, LineKind::Error);
        }
        effects
    }

    /// Stops what a participant is doing: its running turn and its queue.
    fn unseat(&mut self, id: &str) -> Vec<Effect> {
        let Some(seat) = self.seat_mut(id) else {
            return vec![];
        };
        seat.queue.clear();
        seat.held = false;
        if seat.busy {
            vec![Effect::CancelTurn(id.into())]
        } else {
            vec![]
        }
    }

    fn availability(&self, p: &Participant) -> Result<(), String> {
        // No preflight for HTTP models: a dead endpoint shows up as the turn's error.
        if p.adapter == Adapter::OpenAi {
            return Ok(());
        }
        match self.checks.get(&p.id) {
            Some(Ok(_)) => Ok(()),
            Some(Err(why)) => Err(why.clone()),
            None => Err("not checked yet".into()),
        }
    }

    /// `@id` and `@all` at the start pick recipients; none means the default. The flag says
    /// it was a broadcast (`@all` or several mentions), whatever it expanded to: broadcasts
    /// are read-only even if only the writer is left (ADR 0004).
    fn route(&self, input: &str) -> Result<(Vec<String>, String, bool), String> {
        let mut targets: Vec<String> = vec![];
        let mut mentions = 0;
        let mut rest = input.trim_start();
        while let Some(mention) = rest.strip_prefix('@') {
            let end = mention.find(char::is_whitespace).unwrap_or(mention.len());
            let name = &mention[..end];
            mentions += if name == "all" { 2 } else { 1 };
            if name == "all" {
                targets.extend(
                    self.registry
                        .participants
                        .iter()
                        .filter(|p| p.enabled && self.availability(p).is_ok())
                        .map(|p| p.id.clone()),
                );
            } else if self.registry.get(name).is_some() {
                targets.push(name.into());
            } else {
                return Err(format!("no one called @{name}"));
            }
            rest = mention[end..].trim_start();
        }
        let prompt = rest.trim();
        if prompt.is_empty() {
            return Err("nothing to send".into());
        }
        if targets.is_empty() {
            match self.registry.default() {
                Some(id) => targets.push(id.into()),
                None => return Err("no default model: start with @<id> (see /models)".into()),
            }
        }
        let mut seen = vec![];
        targets.retain(|id| {
            let new = !seen.contains(id);
            seen.push(id.clone());
            new
        });
        Ok((targets, prompt.into(), mentions > 1))
    }

    fn deliver(&mut self, id: &str, prompt: String, writable: bool, origin: u64) -> Vec<Effect> {
        let participant = self.registry.get(id).expect("routed ids exist").clone();
        if !participant.enabled {
            self.push("roundtable", format!("`{id}` is disabled"), LineKind::Error);
            return vec![];
        }
        if let Err(why) = self.availability(&participant) {
            self.push(
                "roundtable",
                format!("{id} is unavailable: {why}"),
                LineKind::Error,
            );
            return vec![];
        }
        let seat = self.seat_mut(id).expect("every participant has a seat");
        if seat.busy || seat.held {
            seat.queue.push_back((prompt, writable, origin));
            self.push("roundtable", format!("queued for {id}"), LineKind::Info);
            return vec![];
        }
        self.start(&participant, prompt, writable, origin)
    }

    fn start(
        &mut self,
        p: &Participant,
        prompt: String,
        writable: bool,
        origin: u64,
    ) -> Vec<Effect> {
        // Only a cli writer edits; the registry never makes an openai model the writer.
        let writable = writable && matches!(p.adapter, Adapter::Cli(_));
        // One writable turn per checkout (ADR 0004): if the writer changed while the old one
        // is still editing, the new turn waits until it is done.
        if self.quitting {
            return vec![];
        }
        if writable && self.seats.iter().any(|s| s.busy && s.writable) {
            let seat = self.seat_mut(&p.id).expect("seat");
            seat.queue.push_front((prompt, true, origin));
            seat.held = true;
            self.push(
                "roundtable",
                format!("{}: waiting, another writer is still editing", p.id),
                LineKind::Info,
            );
            return vec![];
        }
        let index = self.lines.len();
        self.lines.push(ChatLine {
            who: p.id.clone(),
            text: String::new(),
            kind: LineKind::Streaming,
        });
        let seat = self.seat_mut(&p.id).expect("seat");
        seat.busy = true;
        seat.writable = writable;
        seat.turn_identity = Some(p.identity.clone());
        seat.origin = origin;
        seat.answering = None;
        seat.asked_by = None;
        seat.reply = Some(index);
        seat.usage = None;
        seat.prompt = Some(prompt.clone());
        let session = seat.session.clone();
        let history = seat.history.clone();
        let delivery = record(
            router(),
            vec![self.party(&p.id)],
            transcript::Kind::Delivery,
            // A writable delivery is the writer marker: synced before the CLI starts.
            json!({ "status": "sent", "text": prompt, "writable": writable }),
        )
        .durable();
        let Adapter::Cli(kind) = p.adapter else {
            let role = p
                .role
                .as_deref()
                .map(|r| format!(" Your role: {r}."))
                .unwrap_or_default();
            return vec![
                delivery,
                Effect::StartChat {
                    id: p.id.clone(),
                    base_url: p.base_url.clone().unwrap_or_default(),
                    model: p.model.clone().unwrap_or_default(),
                    key_ref: p.key.clone(),
                    system: format!(
                        "You are `{}`, one participant in a group chat with a human and other AI \
                         models, run by roundtable.{role} Answer the human's message.",
                        p.id
                    ),
                    history,
                    prompt,
                    context_window: p
                        .context_window
                        .unwrap_or(crate::openai::DEFAULT_CONTEXT_WINDOW),
                    max_output: p
                        .max_output_tokens
                        .unwrap_or(crate::openai::DEFAULT_MAX_OUTPUT),
                    connect_timeout_s: p
                        .connect_timeout_s
                        .unwrap_or(crate::openai::DEFAULT_CONNECT_TIMEOUT_S),
                    idle_timeout_s: p
                        .idle_timeout_s
                        .unwrap_or(crate::openai::DEFAULT_IDLE_TIMEOUT_S),
                },
            ];
        };
        vec![
            delivery,
            Effect::StartTurn {
                id: p.id.clone(),
                kind,
                cmd: p.cmd.clone().unwrap_or_else(|| kind.cmd().into()),
                model: p.model.clone(),
                prompt,
                session,
                writable,
                permission_mode: p.permission_mode.clone(),
                ask_tool: self.may_ask(&p.id, kind),
            },
        ]
    }

    fn cancel_all(&mut self) -> Vec<Effect> {
        let mut any = false;
        for seat in &mut self.seats {
            seat.queue.clear();
            seat.held = false;
            any |= seat.busy;
        }
        if any {
            vec![Effect::CancelTurns]
        } else {
            vec![]
        }
    }

    fn agent_event(&mut self, id: &str, event: PEvent) -> Vec<Effect> {
        let current = self.registry.get(id).map(|p| p.identity.clone());
        let party = self.turn_party(id);
        let Some(seat) = self.seat_mut(id) else {
            return vec![];
        };
        let reply = seat.reply;
        match event {
            PEvent::Session(session) => {
                // Saved under the identity the turn went to; it becomes the seat's session
                // only if that is still the participant's destination.
                let Some(identity) = seat.turn_identity.clone() else {
                    return vec![];
                };
                if current.as_ref() == Some(&identity) {
                    if seat.session.as_deref() == Some(session.as_str()) {
                        return vec![];
                    }
                    seat.session = Some(session.clone());
                }
                vec![Effect::SaveSession {
                    id: id.into(),
                    identity,
                    session,
                }]
            }
            PEvent::TextDelta(text) => {
                if let Some(i) = reply {
                    self.lines[i].text.push_str(&text);
                }
                vec![]
            }
            PEvent::Message(text) => {
                if let Some(i) = reply {
                    let line = &mut self.lines[i].text;
                    if !line.is_empty() {
                        line.push_str("\n\n");
                    }
                    line.push_str(&text);
                }
                vec![]
            }
            PEvent::Tool(summary) => {
                // Shown above the reply it belongs to, so the reply stays one block.
                let line = ChatLine {
                    who: id.into(),
                    text: summary.clone(),
                    kind: LineKind::Tool,
                };
                match reply {
                    Some(i) => {
                        self.lines.insert(i, line);
                        for seat in &mut self.seats {
                            if let Some(r) = seat.reply.as_mut()
                                && *r >= i
                            {
                                *r += 1;
                            }
                        }
                    }
                    None => self.lines.push(line),
                }
                vec![record(
                    party,
                    vec![],
                    transcript::Kind::ToolCall,
                    json!({ "summary": summary }),
                )]
            }
            PEvent::Usage {
                input,
                output,
                cost_usd,
                estimated,
            } => {
                seat.usage = Some((input, output, cost_usd, estimated));
                vec![]
            }
            PEvent::Error(text) => {
                self.push(id, text.clone(), LineKind::Error);
                // Kept in the transcript too: a failed reply should say why.
                vec![record(
                    party,
                    vec![],
                    transcript::Kind::Error,
                    json!({ "text": text }),
                )]
            }
            PEvent::Done(outcome) => self.finish(id, outcome),
        }
    }

    fn finish(&mut self, id: &str, outcome: Outcome) -> Vec<Effect> {
        let party = self.turn_party(id);
        let seat = self.seat_mut(id).expect("seat");
        seat.busy = false;
        let usage = seat.usage;
        if let Some((tokens_in, tokens_out, ..)) = usage {
            seat.thread_tokens.0 += tokens_in;
            seat.thread_tokens.1 += tokens_out;
        }
        let writable = std::mem::take(&mut seat.writable);
        let prompt = seat.prompt.take();
        let text = match seat.reply.take() {
            Some(i) => {
                self.lines[i].kind = LineKind::Reply(outcome);
                self.lines[i].text.clone()
            }
            None => String::new(),
        };
        // A complete exchange becomes the model's own history for its next turn, but only
        // if the model still points where the turn went (ADR 0005): a reload in between
        // must not carry it to a new destination.
        let turn_identity = self
            .seats
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.turn_identity.clone());
        if outcome == Outcome::Complete
            && self.registry.get(id).is_some_and(|p| {
                p.adapter == Adapter::OpenAi && Some(&p.identity) == turn_identity.as_ref()
            })
            && let Some(prompt) = prompt
        {
            let seat = self.seat_mut(id).expect("seat");
            // Copies only if a turn still shares it; normally the runner is done with it.
            std::sync::Arc::make_mut(&mut seat.history).push((prompt, text.clone()));
        }
        let asker = self
            .seats
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.asked_by.clone());
        let mut reply = record(
            party,
            vec![asker.map_or_else(user, |a| self.party(&a))],
            transcript::Kind::Msg,
            // `writable` here closes the writer marker of the delivery (ADR 0010).
            json!({
                "text": text,
                "status": outcome.as_str(),
                "cost_usd": usage.and_then(|u| u.2),
                // `null`: the turn never reported usage (unknown, not zero).
                "usage_estimated": usage.map(|u| u.3),
                "writable": writable,
            }),
        )
        .durable();
        if let (Effect::Record { event, .. }, Some((tokens_in, tokens_out, ..))) =
            (&mut reply, usage)
        {
            event.tokens_in = Some(tokens_in);
            event.tokens_out = Some(tokens_out);
        }
        let mut effects = vec![reply];
        effects.extend(self.end_ask(id, outcome, &text, usage.map(|u| u.0 + u.1)));
        let next = self.seat_mut(id).and_then(|s| s.queue.pop_front());
        if let Some((prompt, writable, origin)) = next
            && let Some(p) = self.registry.get(id).cloned()
        {
            // Rechecked: the writer or the lock may have changed while it waited.
            let writable = writable && self.registry.writer() == Some(id) && !self.locked();
            effects.extend(self.start(&p, prompt, writable, origin));
        }
        if writable {
            effects.extend(self.release_held());
        }
        effects
    }

    /// A writer finished: every held seat gets its turn again. The first one that is still
    /// the writer starts editing; any other one is held again by `start`'s own check.
    fn release_held(&mut self) -> Vec<Effect> {
        let held: Vec<String> = self
            .seats
            .iter()
            .filter(|s| s.held && !s.busy)
            .map(|s| s.id.clone())
            .collect();
        let mut effects = vec![];
        for id in held {
            let seat = self.seat_mut(&id).expect("seat");
            seat.held = false;
            let Some((prompt, writable, origin)) = seat.queue.pop_front() else {
                continue;
            };
            let Some(p) = self.registry.get(&id).cloned() else {
                continue;
            };
            let writable =
                writable && self.registry.writer() == Some(id.as_str()) && !self.locked();
            effects.extend(self.start(&p, prompt, writable, origin));
        }
        effects
    }

    /// Whether `id` gets the `ask` tool for its next turn: talk is on, the policy lets it
    /// ask someone, and its CLI supports it (Claude; Codex can be asked, not ask, until its
    /// MCP approval settings are verified).
    fn may_ask(&self, id: &str, kind: agent::Kind) -> bool {
        self.talk_on
            && kind == agent::Kind::Claude
            && self
                .registry
                .talk
                .allow
                .get(id)
                .is_some_and(|targets| !targets.is_empty())
    }

    /// A participant asked another (ADR 0007). The checks run in the ADR's order; any
    /// refusal is logged and answered at once, so the caller can carry on.
    fn asked(&mut self, ask: u64, caller: String, to: String, prompt: String) -> Vec<Effect> {
        let origin = self
            .seats
            .iter()
            .find(|s| s.id == caller && s.busy)
            .map(|s| s.origin);
        let talk = self.registry.talk.clone();
        let target = self.registry.get(&to).cloned();
        let refusal = if self.quitting {
            Some("roundtable is quitting".to_string())
        } else if !self.talk_on {
            Some("talk between models is off".into())
        } else if origin.is_none() {
            Some(format!("{caller} has no running turn"))
        } else if to == caller {
            Some("a model can't ask itself".into())
        } else if let Some(why) = match &target {
            None => Some(format!("no one called {to}")),
            Some(p) if !p.enabled => Some(format!("{to} is disabled")),
            Some(p) => self
                .availability(p)
                .err()
                .map(|w| format!("{to} is unavailable: {w}")),
        } {
            Some(why)
        } else if !talk.allow.get(&caller).is_some_and(|t| t.contains(&to)) {
            Some(format!("the talk policy doesn't let {caller} ask {to}"))
        } else if self.seats.iter().any(|s| s.id == to && (s.busy || s.held)) {
            // One active turn per session: waiting would deadlock A -> B -> A.
            Some(format!("busy: {to} is answering something else"))
        } else {
            let (turns, tokens) = *self.origins.get(&origin.unwrap_or(0)).unwrap_or(&(0, 0));
            if turns >= talk.max_turns {
                Some(format!(
                    "turn limit reached ({} model-to-model turns per message)",
                    talk.max_turns
                ))
            } else if tokens >= talk.budget_tokens {
                Some(format!(
                    "token budget used up ({} per message)",
                    talk.budget_tokens
                ))
            } else {
                None
            }
        };
        let (from_party, to_party) = (self.party(&caller), self.party(&to));
        let handoff = |status: &str, reason: Option<&str>| {
            record(
                from_party.clone(),
                vec![to_party.clone()],
                transcript::Kind::Handoff,
                json!({ "ask": ask, "prompt": prompt, "status": status, "reason": reason, "origin": origin }),
            )
            .durable()
        };
        if let Some(why) = refusal {
            let effects = vec![
                handoff("blocked", Some(&why)),
                Effect::Answer {
                    ask,
                    text: format!("blocked: {why}"),
                    refused: true,
                },
            ];
            self.push(
                &format!("{caller} → {to}"),
                format!("blocked: {why}"),
                LineKind::Info,
            );
            return effects;
        }
        let origin = origin.unwrap_or(0);
        self.origins.entry(origin).or_default().0 += 1;
        let mut effects = vec![handoff("sent", None)];
        self.push(
            &format!("{caller} → {to}"),
            prompt.clone(),
            LineKind::Handoff,
        );
        let target = target.expect("checked above");
        // An asked turn never writes, whoever it is (ADR 0004).
        effects.extend(self.start(&target, prompt, false, origin));
        if let Some(seat) = self.seat_mut(&to) {
            seat.answering = Some(ask);
            seat.asked_by = Some(caller.clone());
        }
        if let Some(i) = self.seats.iter().find(|s| s.id == to).and_then(|s| s.reply) {
            self.lines[i].who = format!("{to} → {caller}");
        }
        effects
    }

    /// A turn ended: if it answered an `ask`, send the answer; cancel what it asked that is
    /// still running (its chain dies with it); count its tokens against its message.
    fn end_ask(
        &mut self,
        id: &str,
        outcome: Outcome,
        text: &str,
        tokens: Option<u64>,
    ) -> Vec<Effect> {
        let mut effects = vec![];
        let (origin, answering) = match self.seat_mut(id) {
            Some(seat) => (seat.origin, seat.answering.take()),
            None => return effects,
        };
        if let Some(tokens) = tokens {
            self.origins.entry(origin).or_default().1 += tokens;
        }
        if let Some(ask) = answering {
            let (text, refused) = match outcome {
                Outcome::Complete => (text.to_string(), false),
                Outcome::Interrupted => (format!("interrupted; partial answer: {text}"), true),
                Outcome::Failed => (format!("failed; partial answer: {text}"), true),
            };
            effects.push(Effect::Answer { ask, text, refused });
        }
        for seat in &mut self.seats {
            if seat.busy && seat.asked_by.as_deref() == Some(id) {
                effects.push(Effect::CancelTurn(seat.id.clone()));
            }
        }
        effects
    }

    fn seat_mut(&mut self, id: &str) -> Option<&mut Seat> {
        self.seats.iter_mut().find(|s| s.id == id)
    }

    /// The participant as the running turn knew it (see `Seat::turn_identity`).
    fn turn_party(&self, id: &str) -> Party {
        let identity = self
            .seats
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.turn_identity.clone())
            .or_else(|| self.registry.get(id).map(|p| p.identity.clone()));
        Party {
            id: id.into(),
            identity,
        }
    }

    fn party(&self, id: &str) -> Party {
        Party {
            id: id.into(),
            identity: self.registry.get(id).map(|p| p.identity.clone()),
        }
    }

    fn push(&mut self, who: &str, text: String, kind: LineKind) {
        self.lines.push(ChatLine {
            who: who.into(),
            text,
            kind,
        });
    }
}

fn user() -> Party {
    Party {
        id: "user".into(),
        identity: None,
    }
}

fn router() -> Party {
    Party {
        id: "router".into(),
        identity: None,
    }
}

fn record(
    from: Party,
    to: Vec<Party>,
    kind: transcript::Kind,
    payload: serde_json::Value,
) -> Effect {
    Effect::Record {
        event: NewEvent {
            turn: None,
            parent: None,
            from,
            to,
            kind,
            payload,
            tokens_in: None,
            tokens_out: None,
        },
        durable: false,
    }
}

/// A registry change, logged as a `status` event (ADR 0011).
fn status(text: String) -> Effect {
    record(
        router(),
        vec![],
        transcript::Kind::Status,
        json!({ "registry": text }),
    )
}

impl Effect {
    fn durable(self) -> Self {
        match self {
            Effect::Record { event, .. } => Effect::Record {
                event,
                durable: true,
            },
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::DEFAULT_FILE;

    fn key(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    fn type_line(app: &mut App, text: &str) -> Vec<Effect> {
        for c in text.chars() {
            app.update(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.update(key(KeyCode::Enter, KeyModifiers::NONE))
    }

    fn started(effects: &[Effect]) -> Vec<String> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::StartTurn { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    fn last_text(app: &App) -> &str {
        &app.lines.last().unwrap().text
    }

    fn agent(id: &str, event: PEvent) -> Event {
        Event::Agent(id.into(), event)
    }

    fn reload(app: &mut App, text: &str) -> Vec<Effect> {
        let registry = Registry::parse(text).unwrap();
        let checks = registry
            .participants
            .iter()
            .map(|p| (p.id.clone(), Ok("v".into())))
            .collect();
        app.update(Event::Registry(Ok((registry, checks))))
    }

    #[test]
    fn ctrl_c_twice_or_ctrl_d_on_empty_input_quits() {
        let ctrl_c = || key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let mut app = App::default();
        assert!(app.update(ctrl_c()).is_empty(), "first Ctrl-C only arms");
        assert_eq!(app.update(ctrl_c()), [Effect::Quit]);

        let mut app = App::default();
        app.update(ctrl_c());
        app.update(key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.update(ctrl_c()).is_empty(), "any other key disarms");

        let ctrl_d = || key(KeyCode::Char('d'), KeyModifiers::CONTROL);
        let mut app = App::default();
        assert_eq!(app.update(ctrl_d()), [Effect::Quit]);
        let mut app = App::default();
        app.update(key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(
            app.update(ctrl_d()).is_empty(),
            "Ctrl-D never discards typed text"
        );
    }

    #[test]
    fn draws_once_per_change_at_most_once_per_tick() {
        let mut app = App::default();
        assert_eq!(
            app.update(Event::Tick),
            [Effect::Redraw],
            "first tick draws"
        );
        assert!(app.update(Event::Tick).is_empty(), "nothing changed");
        app.update(Event::Resize);
        app.update(Event::Resize);
        assert_eq!(
            app.update(Event::Tick),
            [Effect::Redraw],
            "two changes, one draw"
        );
        assert!(app.update(Event::Tick).is_empty());
    }

    #[test]
    fn quits_when_input_closes() {
        let mut app = App::default();
        assert_eq!(app.update(Event::InputClosed("eof".into())), [Effect::Quit]);
    }

    #[test]
    fn routes_by_mention_with_the_registry_default() {
        assert_eq!(
            started(&type_line(&mut App::default(), "hello")),
            ["claude"]
        );
        assert_eq!(
            started(&type_line(&mut App::default(), "@codex review")),
            ["codex"]
        );
        assert_eq!(
            started(&type_line(&mut App::default(), "@all hi")),
            ["claude", "codex"]
        );
        assert_eq!(
            started(&type_line(&mut App::default(), "@codex @claude @codex hi")),
            ["codex", "claude"],
            "each recipient once, in mention order"
        );
    }

    #[test]
    fn bad_input_is_kept_and_explained() {
        let mut app = App::default();
        assert!(type_line(&mut app, "@gemini hi").is_empty());
        assert_eq!(app.input, "@gemini hi");
        assert_eq!(last_text(&app), "no one called @gemini");
        app.input.clear();
        assert!(type_line(&mut app, "@codex   ").is_empty());
        assert_eq!(last_text(&app), "nothing to send");
    }

    #[test]
    fn unavailable_agents_get_no_turn() {
        let registry = Registry::parse(DEFAULT_FILE).unwrap();
        let checks = Checks::from([
            ("claude".into(), Ok("v".into())),
            ("codex".into(), Err("not inside a git repo".into())),
        ]);
        let mut app = App::new(registry, checks);
        assert!(started(&type_line(&mut app, "@codex hi")).is_empty());
        assert_eq!(
            last_text(&app),
            "codex is unavailable: not inside a git repo"
        );
        assert_eq!(
            started(&type_line(&mut app, "@all hi")),
            ["claude"],
            "@all means everyone available"
        );
    }

    #[test]
    fn a_turn_streams_records_and_resumes_its_session() {
        let mut app = App::default();
        type_line(&mut app, "ping");
        let identity = app.registry.get("claude").unwrap().identity.clone();
        assert_eq!(
            app.update(agent("claude", PEvent::Session("S1".into()))),
            [Effect::SaveSession {
                id: "claude".into(),
                identity: identity.clone(),
                session: "S1".into()
            }]
        );
        app.update(agent("claude", PEvent::TextDelta("po".into())));
        app.update(agent("claude", PEvent::TextDelta("ng".into())));
        app.update(agent(
            "claude",
            PEvent::Usage {
                input: 10,
                output: 2,
                cost_usd: Some(0.01),
                estimated: false,
            },
        ));
        let effects = app.update(agent("claude", PEvent::Done(Outcome::Complete)));
        let reply = app.lines.last().unwrap();
        assert_eq!(
            (reply.text.as_str(), reply.kind),
            ("pong", LineKind::Reply(Outcome::Complete))
        );
        match &effects[..] {
            [
                Effect::Record {
                    event,
                    durable: true,
                },
            ] => {
                assert_eq!(event.payload["text"], "pong");
                assert_eq!((event.tokens_in, event.tokens_out), (Some(10), Some(2)));
                assert_eq!(event.from.identity.as_deref(), Some(identity.as_str()));
            }
            other => panic!("{other:?}"),
        }
        let effects = type_line(&mut app, "again");
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::StartTurn { session: Some(s), .. } if s == "S1"))
        );
    }

    #[test]
    fn a_busy_agent_queues_and_esc_cancels_everything() {
        let mut app = App::default();
        type_line(&mut app, "first");
        assert!(started(&type_line(&mut app, "second")).is_empty());
        assert_eq!(last_text(&app), "queued for claude");
        let effects = app.update(agent("claude", PEvent::Done(Outcome::Complete)));
        assert_eq!(
            started(&effects),
            ["claude"],
            "the queue drains when the turn ends"
        );

        type_line(&mut app, "third");
        assert_eq!(
            app.update(key(KeyCode::Esc, KeyModifiers::NONE)),
            [Effect::CancelTurns]
        );
        let effects = app.update(agent("claude", PEvent::Done(Outcome::Interrupted)));
        assert!(started(&effects).is_empty(), "Esc also clears the queue");
    }

    #[test]
    fn tool_lines_go_above_the_reply_they_belong_to() {
        let mut app = App::default();
        type_line(&mut app, "@all go");
        app.update(agent("claude", PEvent::Tool("Read".into())));
        app.update(agent("codex", PEvent::Tool("ls".into())));
        app.update(agent("codex", PEvent::Message("done".into())));
        app.update(agent("claude", PEvent::TextDelta("hi".into())));
        let shown: Vec<(&str, &str)> = app
            .lines
            .iter()
            .map(|l| (l.who.as_str(), l.text.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                ("you → claude, codex", "go"),
                ("claude", "Read"),
                ("claude", "hi"),
                ("codex", "ls"),
                ("codex", "done")
            ]
        );
    }

    #[test]
    fn disabling_cancels_the_running_turn_and_the_queue() {
        let mut app = App::default();
        type_line(&mut app, "@codex one");
        type_line(&mut app, "@codex two");
        let effects = type_line(&mut app, "/model disable codex");
        assert!(effects.contains(&Effect::CancelTurn("codex".into())));
        assert!(effects.contains(&Effect::EditRegistry(FileEdit::Enabled {
            id: "codex".into(),
            enabled: false
        })));
        let effects = app.update(agent("codex", PEvent::Done(Outcome::Interrupted)));
        assert!(started(&effects).is_empty(), "the queued message is gone");
        assert!(started(&type_line(&mut app, "@codex hi")).is_empty());
        assert_eq!(last_text(&app), "`codex` is disabled");
        assert_eq!(started(&type_line(&mut app, "@all hi")), ["claude"]);

        type_line(&mut app, "/model enable codex");
        assert_eq!(started(&type_line(&mut app, "@codex back")), ["codex"]);
    }

    #[test]
    fn the_default_cannot_be_disabled_until_another_is_used() {
        let mut app = App::default();
        assert!(type_line(&mut app, "/model disable claude").is_empty());
        assert_eq!(
            last_text(&app),
            "`claude` is the default: /model use another first"
        );
        let effects = type_line(&mut app, "/model use codex");
        assert!(effects.contains(&Effect::EditRegistry(FileEdit::Default("codex".into()))));
        assert_eq!(started(&type_line(&mut app, "no mention")), ["codex"]);
        assert!(!type_line(&mut app, "/model disable claude").is_empty());
    }

    #[test]
    fn file_edits_apply_as_a_diff() {
        let mut app = App::default();
        type_line(&mut app, "@claude long task");
        // claude switches model (new identity), codex is renamed codex2.
        let text = DEFAULT_FILE
            .replace("# model = \"claude-opus-5-5\"", "model = \"other\"")
            .replace("id = \"codex\"", "id = \"codex2\"")
            .replace("claude = [\"codex\"], codex = []", "claude = [\"codex2\"]");
        let effects = reload(&mut app, &text);
        assert!(
            effects.contains(&Effect::CancelTurn("claude".into())),
            "{effects:?}"
        );
        let notes: Vec<&str> = app.lines.iter().map(|l| l.text.as_str()).collect();
        assert!(notes.contains(&"codex2 joined"), "{notes:?}");
        assert!(notes.contains(&"codex left"));
        assert!(
            notes
                .iter()
                .any(|n| n.starts_with("claude now points to a new destination"))
        );
        assert!(started(&type_line(&mut app, "@codex hi")).is_empty());
        assert_eq!(last_text(&app), "no one called @codex");
        app.input.clear(); // a refused line stays in the input for fixing
        assert_eq!(started(&type_line(&mut app, "@codex2 hi")), ["codex2"]);
        app.update(agent("claude", PEvent::Done(Outcome::Interrupted)));
        let effects = type_line(&mut app, "@claude again");
        assert!(
            effects.iter().any(|e| matches!(e,
                Effect::StartTurn { session: None, model: Some(m), .. } if m == "other")),
            "a new identity starts a new session with the new model"
        );
    }

    fn writable(effects: &[Effect]) -> Vec<(String, bool)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::StartTurn { id, writable, .. } => Some((id.clone(), *writable)),
                _ => None,
            })
            .collect()
    }

    fn payload_writable(effects: &[Effect], kind: transcript::Kind) -> Option<bool> {
        effects.iter().find_map(|e| match e {
            Effect::Record { event, .. } if event.kind == kind => {
                event.payload["writable"].as_bool()
            }
            _ => None,
        })
    }

    #[test]
    fn only_a_message_to_the_writer_alone_may_edit() {
        let mut app = App::default();
        let effects = type_line(&mut app, "fix the bug");
        assert_eq!(writable(&effects), [("claude".into(), true)]);
        assert_eq!(
            payload_writable(&effects, transcript::Kind::Delivery),
            Some(true),
            "the delivery is the writer marker"
        );
        assert!(app.lines.iter().any(|l| l.who == "you → claude (may edit)"));
        let effects = app.update(agent("claude", PEvent::Done(Outcome::Complete)));
        assert_eq!(
            payload_writable(&effects, transcript::Kind::Msg),
            Some(true),
            "the reply closes the marker"
        );

        for line in ["@all fix", "@claude @codex fix", "@codex fix"] {
            let mut app = App::default();
            let effects = type_line(&mut app, line);
            assert!(writable(&effects).iter().all(|(_, w)| !w), "{line}");
        }
    }

    #[test]
    fn locked_edits_run_read_only_until_quiet() {
        let mut app = App::default();
        app.lock_edits(EditLock::Crashed {
            text: "{marker}".into(),
            key: "k1".into(),
        });
        assert!(last_text(&app).starts_with("edits are locked"));
        let effects = type_line(&mut app, "fix");
        assert_eq!(writable(&effects), [("claude".into(), false)]);
        assert!(
            app.lines
                .iter()
                .any(|l| l.text == "read-only: edits are locked until /quiet")
        );
        app.update(agent("claude", PEvent::Done(Outcome::Complete)));

        assert_eq!(
            type_line(&mut app, "/quiet"),
            [Effect::ConfirmQuiet("k1".into())]
        );
        // Not unlocked until the removal is confirmed on disk.
        assert_eq!(
            writable(&type_line(&mut app, "fix")),
            [("claude".into(), false)]
        );
        app.update(agent("claude", PEvent::Done(Outcome::Complete)));
        app.update(Event::QuietDone(Err("disk full".into())));
        assert_eq!(last_text(&app), "edits are still locked: disk full");
        // The marker changed meanwhile: the new one must be confirmed instead.
        app.update(Event::LockEdits(EditLock::Crashed {
            text: "{other}".into(),
            key: "k2".into(),
        }));
        assert_eq!(
            type_line(&mut app, "/quiet"),
            [Effect::ConfirmQuiet("k2".into())]
        );
        app.update(Event::QuietDone(Ok(())));
        assert_eq!(last_text(&app), "edits unlocked");
        assert_eq!(
            writable(&type_line(&mut app, "fix")),
            [("claude".into(), true)]
        );
        assert!(
            type_line(&mut app, "/quiet").is_empty(),
            "nothing left to unlock"
        );
    }

    #[test]
    fn two_writable_turns_never_overlap() {
        let mut app = App::default();
        type_line(&mut app, "claude edits");
        // The writer becomes codex while claude is still editing.
        reload(
            &mut app,
            &DEFAULT_FILE.replace("writer = \"claude\"", "writer = \"codex\""),
        );
        let effects = type_line(&mut app, "@codex edit too");
        assert!(
            writable(&effects).is_empty(),
            "codex waits instead of overlapping"
        );
        assert_eq!(
            last_text(&app),
            "codex: waiting, another writer is still editing"
        );
        let effects = app.update(agent("claude", PEvent::Done(Outcome::Complete)));
        assert_eq!(
            writable(&effects),
            [("codex".into(), true)],
            "the waiting writer starts, still writable"
        );
    }

    /// Claude is editing; then the writer becomes `new` and `@new edit` waits.
    fn waiting_edit(new: &str, extra: &str) -> App {
        let file = format!("{DEFAULT_FILE}{extra}");
        let mut app = App::default();
        reload(&mut app, &file);
        type_line(&mut app, "claude edits");
        reload(
            &mut app,
            &file.replace("writer = \"claude\"", &format!("writer = \"{new}\"")),
        );
        assert!(started(&type_line(&mut app, &format!("@{new} edit"))).is_empty());
        app
    }

    #[test]
    fn esc_and_quit_discard_a_waiting_edit() {
        let mut app = waiting_edit("codex", "");
        assert_eq!(
            app.update(key(KeyCode::Esc, KeyModifiers::NONE)),
            [Effect::CancelTurns]
        );
        let effects = app.update(agent("claude", PEvent::Done(Outcome::Interrupted)));
        assert!(started(&effects).is_empty(), "Esc dropped the waiting edit");

        let mut app = waiting_edit("codex", "");
        app.update(key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        let effects = app.update(agent("claude", PEvent::Done(Outcome::Interrupted)));
        assert!(
            started(&effects).is_empty(),
            "nothing starts while quitting"
        );
    }

    #[test]
    fn every_waiting_seat_gets_its_turn() {
        let third = "\n[[model]]\nid = \"claude2\"\nkind = \"cli\"\ncmd = \"claude\"\n";
        let mut app = waiting_edit("codex", third);
        // The writer moves on again to claude2, which also asks to edit.
        let file =
            format!("{DEFAULT_FILE}{third}").replace("writer = \"claude\"", "writer = \"claude2\"");
        reload(&mut app, &file);
        assert!(started(&type_line(&mut app, "@claude2 edit")).is_empty());
        let effects = app.update(agent("claude", PEvent::Done(Outcome::Complete)));
        assert_eq!(
            writable(&effects),
            [("codex".into(), false), ("claude2".into(), true)],
            "codex is no longer the writer; claude2 edits"
        );
    }

    #[test]
    fn a_waiting_edit_keeps_its_place_in_line() {
        let mut app = waiting_edit("codex", "");
        let effects = type_line(&mut app, "@all status?");
        assert!(
            started(&effects).is_empty(),
            "both seats are occupied: claude busy, codex held"
        );
        let effects = app.update(agent("claude", PEvent::Done(Outcome::Complete)));
        let order: Vec<_> = writable(&effects);
        assert_eq!(
            order,
            [("claude".into(), false), ("codex".into(), true)],
            "codex runs its edit first; its @all reply waits behind it"
        );
    }

    #[test]
    fn a_broadcast_stays_read_only_even_when_only_the_writer_is_left() {
        let registry = Registry::parse(DEFAULT_FILE).unwrap();
        let checks = Checks::from([
            ("claude".into(), Ok("v".into())),
            ("codex".into(), Err("unavailable".into())),
        ]);
        let mut app = App::new(registry, checks);
        assert_eq!(
            writable(&type_line(&mut app, "@all fix this")),
            [("claude".into(), false)]
        );
        let mut app = App::default();
        assert_eq!(
            writable(&type_line(&mut app, "@claude @claude fix")),
            [("claude".into(), false)],
            "two mentions are a broadcast even if they name the same one"
        );
    }

    #[test]
    fn git_problems_and_outside_git_lock_without_quiet() {
        for lock in [
            EditLock::Unknown("GIT_DIR is set".into()),
            EditLock::OutsideGit,
        ] {
            let mut app = App::default();
            app.lock_edits(lock.clone());
            assert_eq!(
                writable(&type_line(&mut app, "fix")),
                [("claude".into(), false)],
                "{lock:?}"
            );
            app.update(agent("claude", PEvent::Done(Outcome::Complete)));
            assert!(type_line(&mut app, "/quiet").is_empty(), "{lock:?}");
            assert!(last_text(&app).starts_with("/quiet can't help"), "{lock:?}");
        }
    }

    #[test]
    fn a_queued_edit_is_rechecked_when_it_starts() {
        let mut app = App::default();
        type_line(&mut app, "first");
        type_line(&mut app, "second, queued");
        reload(&mut app, &DEFAULT_FILE.replace("writer = \"claude\"", ""));
        let effects = app.update(agent("claude", PEvent::Done(Outcome::Complete)));
        assert_eq!(
            writable(&effects),
            [("claude".into(), false)],
            "the writer was removed while it waited"
        );
    }

    #[test]
    fn a_late_session_stays_with_the_destination_it_came_from() {
        let mut app = App::default();
        type_line(&mut app, "@claude long task");
        let old = app.registry.get("claude").unwrap().identity.clone();
        reload(
            &mut app,
            &DEFAULT_FILE.replace("# model = \"claude-opus-5-5\"", "model = \"other\""),
        );
        // The old turn reports its session after the switch.
        let effects = app.update(agent("claude", PEvent::Session("OLD".into())));
        assert_eq!(
            effects,
            [Effect::SaveSession {
                id: "claude".into(),
                identity: old,
                session: "OLD".into()
            }]
        );
        app.update(agent("claude", PEvent::Done(Outcome::Interrupted)));
        let effects = type_line(&mut app, "@claude again");
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::StartTurn { session: None, .. })),
            "the new destination does not resume the old session"
        );
    }

    #[test]
    fn a_participant_that_leaves_and_returns_starts_fresh() {
        let mut app = App::default();
        type_line(&mut app, "@codex hi");
        app.update(agent("codex", PEvent::Session("S".into())));
        app.update(agent("codex", PEvent::Done(Outcome::Complete)));
        let without = DEFAULT_FILE
            .replace("id = \"codex\"", "id = \"gone\"")
            .replace("codex = []", "gone = []")
            .replace("claude = [\"codex\"]", "claude = [\"gone\"]");
        reload(&mut app, &without);
        reload(&mut app, DEFAULT_FILE);
        let effects = type_line(&mut app, "@codex back");
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::StartTurn { session: None, .. }))
        );
    }

    #[test]
    fn an_invalid_file_changes_nothing() {
        let mut app = App::default();
        app.update(Event::Registry(Err("line 3: oops".into())));
        assert!(last_text(&app).starts_with("models.toml not applied"));
        assert_eq!(started(&type_line(&mut app, "@codex hi")), ["codex"]);
    }

    const LLAMA: &str = "\n[[model]]\nid = \"llama\"\nkind = \"openai\"\nbase_url = \"http://localhost:11434/v1\"\nmodel = \"llama3.3\"\nkey = \"env:LLAMA_KEY\"\n";

    /// A chat start: participant id, history sent, prompt.
    type ChatStart = (String, Vec<(String, String)>, String);

    fn chats(effects: &[Effect]) -> Vec<ChatStart> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::StartChat {
                    id,
                    history,
                    prompt,
                    ..
                } => Some((id.clone(), history.to_vec(), prompt.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn openai_models_chat_with_their_own_history() {
        let mut app = App::default();
        reload(&mut app, &format!("{DEFAULT_FILE}{LLAMA}"));
        type_line(&mut app, "/models");
        assert!(last_text(&app).contains("llama (openai, llama3.3) ready"));

        let effects = type_line(&mut app, "@llama hi");
        match &effects[..] {
            [
                Effect::Record { .. },
                Effect::Record { .. },
                Effect::StartChat {
                    key_ref,
                    system,
                    context_window,
                    max_output,
                    ..
                },
            ] => {
                assert_eq!(
                    key_ref.as_deref(),
                    Some("env:LLAMA_KEY"),
                    "a reference, resolved by the loop"
                );
                assert!(system.contains("`llama`"));
                assert_eq!((*context_window, *max_output), (8_192, 4_096), "defaults");
            }
            other => panic!("{other:?}"),
        }
        app.update(agent("llama", PEvent::TextDelta("hello".into())));
        app.update(agent("llama", PEvent::Done(Outcome::Complete)));
        let effects = type_line(&mut app, "@llama again");
        assert_eq!(
            chats(&effects),
            [(
                "llama".into(),
                vec![("hi".into(), "hello".into())],
                "again".into()
            )]
        );

        // `@all` reaches it too; a failed turn adds nothing to its history.
        let effects = app.update(agent("llama", PEvent::Done(Outcome::Failed)));
        assert!(chats(&effects).is_empty());
        let effects = type_line(&mut app, "@all status");
        assert_eq!(started(&effects), ["claude", "codex"]);
        assert_eq!(chats(&effects).len(), 1);
        assert_eq!(chats(&effects)[0].1.len(), 1, "only the completed exchange");
    }

    #[test]
    fn a_new_endpoint_starts_with_empty_history() {
        let mut app = App::default();
        reload(&mut app, &format!("{DEFAULT_FILE}{LLAMA}"));
        type_line(&mut app, "@llama hi");
        app.update(agent("llama", PEvent::Done(Outcome::Complete)));
        let hosted = LLAMA.replace("http://localhost:11434/v1", "https://openrouter.ai/api/v1");
        reload(&mut app, &format!("{DEFAULT_FILE}{hosted}"));
        assert!(
            app.lines
                .iter()
                .any(|l| l.text.starts_with("llama now points to a new destination"))
        );
        let effects = type_line(&mut app, "@llama hi again");
        assert!(
            chats(&effects)[0].1.is_empty(),
            "history not carried to a hosted API"
        );
    }

    #[test]
    fn a_reply_finishing_after_a_new_endpoint_is_not_kept() {
        let mut app = App::default();
        reload(&mut app, &format!("{DEFAULT_FILE}{LLAMA}"));
        type_line(&mut app, "@llama private");
        let hosted = LLAMA.replace("http://localhost:11434/v1", "https://openrouter.ai/api/v1");
        reload(&mut app, &format!("{DEFAULT_FILE}{hosted}"));
        // The old turn's Done arrives after the switch.
        app.update(agent("llama", PEvent::Done(Outcome::Complete)));
        let effects = type_line(&mut app, "@llama hi");
        assert!(
            chats(&effects)[0].1.is_empty(),
            "the local exchange stays local"
        );
    }

    #[test]
    fn usage_estimates_are_marked_in_the_transcript() {
        let mut app = App::default();
        reload(&mut app, &format!("{DEFAULT_FILE}{LLAMA}"));
        type_line(&mut app, "@llama hi");
        app.update(agent(
            "llama",
            PEvent::Usage {
                input: 5,
                output: 1,
                cost_usd: None,
                estimated: true,
            },
        ));
        let effects = app.update(agent("llama", PEvent::Done(Outcome::Complete)));
        let Effect::Record { event, .. } = &effects[0] else {
            panic!()
        };
        assert_eq!(event.payload["usage_estimated"], true);
    }

    fn ask(app: &mut App, ask: u64, caller: &str, to: &str) -> Vec<Effect> {
        app.update(Event::Asked {
            ask,
            caller: caller.into(),
            to: to.into(),
            prompt: format!("question {ask}"),
        })
    }

    fn answer(effects: &[Effect]) -> Option<(u64, String, bool)> {
        effects.iter().find_map(|e| match e {
            Effect::Answer { ask, text, refused } => Some((*ask, text.clone(), *refused)),
            _ => None,
        })
    }

    fn ask_tool(effects: &[Effect]) -> Option<bool> {
        effects.iter().find_map(|e| match e {
            Effect::StartTurn { ask_tool, .. } => Some(*ask_tool),
            _ => None,
        })
    }

    #[test]
    fn claude_asks_codex_and_gets_its_answer() {
        let mut app = App::default();
        let effects = type_line(&mut app, "@claude review with codex");
        assert_eq!(
            ask_tool(&effects),
            Some(true),
            "the default policy lets claude ask codex"
        );
        let effects = ask(&mut app, 1, "claude", "codex");
        assert_eq!(
            writable(&effects),
            [("codex".into(), false)],
            "an asked turn never writes"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Record { event, .. }
            if event.kind == transcript::Kind::Handoff && event.payload["status"] == "sent"))
        );
        assert!(
            app.lines
                .iter()
                .any(|l| l.who == "claude → codex" && l.kind == LineKind::Handoff)
        );
        app.update(agent("codex", PEvent::Message("looks fine".into())));
        let effects = app.update(agent("codex", PEvent::Done(Outcome::Complete)));
        assert_eq!(answer(&effects), Some((1, "looks fine".into(), false)));
        let Effect::Record { event, .. } = &effects[0] else {
            panic!()
        };
        assert_eq!(event.to[0].id, "claude", "the reply goes to the asker");
        assert!(app.lines.iter().any(|l| l.who == "codex → claude"));
    }

    #[test]
    fn every_refusal_is_answered_and_logged() {
        let refused = |app: &mut App, caller: &str, to: &str| {
            let effects = ask(app, 9, caller, to);
            assert!(
                started(&effects).is_empty(),
                "{caller} -> {to} started a turn"
            );
            let (_, text, refused) = answer(&effects).expect("answered at once");
            assert!(refused);
            assert!(effects.iter().any(|e| matches!(e, Effect::Record { event, .. }
                if event.kind == transcript::Kind::Handoff && event.payload["status"] == "blocked")));
            text
        };
        let mut app = App::default();
        type_line(&mut app, "@claude go");
        assert!(refused(&mut app, "codex", "claude").contains("no running turn"));
        type_line(&mut app, "@codex go");
        assert!(refused(&mut app, "codex", "claude").contains("talk policy"));
        assert!(refused(&mut app, "claude", "claude").contains("itself"));
        assert!(refused(&mut app, "claude", "ghost").contains("no one called ghost"));
        assert!(
            refused(&mut app, "claude", "codex").contains("busy"),
            "codex is mid-turn"
        );

        let mut app = App::default();
        type_line(&mut app, "@claude go");
        type_line(&mut app, "/model disable codex");
        assert!(refused(&mut app, "claude", "codex").contains("disabled"));

        let mut app = App::default();
        type_line(&mut app, "@claude go");
        type_line(&mut app, "/talk off");
        assert!(refused(&mut app, "claude", "codex").contains("off"));
    }

    #[test]
    fn limits_count_per_user_message() {
        let file = DEFAULT_FILE.replace("max_turns = 6", "max_turns = 2");
        let mut app = App::default();
        reload(&mut app, &file);
        type_line(&mut app, "@claude go");
        for n in 1..=2 {
            assert_eq!(started(&ask(&mut app, n, "claude", "codex")), ["codex"]);
            app.update(agent("codex", PEvent::Done(Outcome::Complete)));
        }
        let (_, text, _) = answer(&ask(&mut app, 3, "claude", "codex")).unwrap();
        assert!(text.contains("turn limit"), "{text}");
        // A new message starts a fresh allowance.
        app.update(agent("claude", PEvent::Done(Outcome::Complete)));
        type_line(&mut app, "@claude again");
        assert_eq!(started(&ask(&mut app, 4, "claude", "codex")), ["codex"]);

        let file = DEFAULT_FILE.replace("budget_tokens = 50000", "budget_tokens = 100");
        let mut app = App::default();
        reload(&mut app, &file);
        type_line(&mut app, "@claude go");
        ask(&mut app, 1, "claude", "codex");
        app.update(agent(
            "codex",
            PEvent::Usage {
                input: 90,
                output: 20,
                cost_usd: None,
                estimated: false,
            },
        ));
        app.update(agent("codex", PEvent::Done(Outcome::Complete)));
        let (_, text, _) = answer(&ask(&mut app, 2, "claude", "codex")).unwrap();
        assert!(text.contains("budget"), "{text}");
    }

    #[test]
    fn esc_and_a_finished_caller_end_the_chain() {
        let mut app = App::default();
        type_line(&mut app, "@claude go");
        ask(&mut app, 1, "claude", "codex");
        app.update(key(KeyCode::Esc, KeyModifiers::NONE));
        let (_, text, refused) =
            answer(&app.update(agent("codex", PEvent::Done(Outcome::Interrupted)))).unwrap();
        assert!(refused && text.starts_with("interrupted"));

        let mut app = App::default();
        type_line(&mut app, "@claude go");
        ask(&mut app, 1, "claude", "codex");
        let effects = app.update(agent("claude", PEvent::Done(Outcome::Complete)));
        assert!(
            effects.contains(&Effect::CancelTurn("codex".into())),
            "{effects:?}"
        );
    }

    #[test]
    fn who_gets_the_ask_tool() {
        let mut app = App::default();
        assert_eq!(
            ask_tool(&type_line(&mut app, "@codex hi")),
            Some(false),
            "codex may ask no one"
        );
        let effects = type_line(&mut app, "/talk off");
        assert!(effects.contains(&Effect::EditRegistry(FileEdit::Talk(false))));
        assert_eq!(
            ask_tool(&type_line(&mut app, "@claude hi")),
            Some(false),
            "talk is off"
        );
        type_line(&mut app, "/talk on");
        app.update(agent("claude", PEvent::Done(Outcome::Complete)));
        assert_eq!(ask_tool(&type_line(&mut app, "@claude hi")), Some(true));
    }

    #[test]
    fn help_status_context_and_usage_commands() {
        let mut app = App::default();
        type_line(&mut app, "/help");
        assert!(last_text(&app).contains("/talk on|off|show"));
        type_line(&mut app, "/status");
        assert!(last_text(&app).contains("claude: ready (test)"));
        assert!(last_text(&app).contains("talk: on; writer: claude; edits: unlocked"));
        type_line(&mut app, "/context codex");
        assert!(last_text(&app).starts_with("codex: 0 tokens in"));
        assert!(last_text(&app).contains("window fill unknown"));
        assert_eq!(
            type_line(&mut app, "/usage"),
            [Effect::Usage("thread".into())]
        );
        assert_eq!(
            type_line(&mut app, "/usage all"),
            [Effect::Usage("all".into())]
        );
        assert!(type_line(&mut app, "/usage forever").is_empty());
        assert_eq!(last_text(&app), "usage: /usage [thread|today|all]");
        assert!(type_line(&mut app, "/nope").is_empty());
        assert_eq!(last_text(&app), "no command /nope; /help lists them");
    }

    #[test]
    fn clear_starts_a_fresh_thread_but_not_mid_reply() {
        let mut app = App::default();
        type_line(&mut app, "hi");
        assert!(
            type_line(&mut app, "/clear").is_empty(),
            "claude is still replying"
        );
        app.update(agent("claude", PEvent::Session("S1".into())));
        app.update(agent("claude", PEvent::Done(Outcome::Complete)));
        assert_eq!(type_line(&mut app, "/clear"), [Effect::NewThread]);
        assert_eq!(app.lines.len(), 1, "only the 'new thread' note");
        let effects = type_line(&mut app, "hi again");
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::StartTurn { session: None, .. }))
        );
    }

    #[test]
    fn talk_policy_commands_edit_memory_and_file() {
        let mut app = App::default();
        let effects = type_line(&mut app, "/talk allow codex->claude");
        assert!(effects.contains(&Effect::EditRegistry(FileEdit::Allow {
            from: "codex".into(),
            targets: vec!["claude".into()]
        })));
        type_line(&mut app, "/talk show");
        assert!(last_text(&app).contains("codex may ask claude"));
        type_line(&mut app, "/talk deny codex->claude");
        type_line(&mut app, "/talk show");
        assert!(last_text(&app).contains("codex may ask no one"));
        assert!(type_line(&mut app, "/talk allow codex->ghost").is_empty());
        assert!(
            type_line(&mut app, "/budget 1000")
                .contains(&Effect::EditRegistry(FileEdit::Budget(1000)))
        );
        assert!(
            type_line(&mut app, "/turns 2").contains(&Effect::EditRegistry(FileEdit::Turns(2)))
        );
        type_line(&mut app, "/talk show");
        assert!(last_text(&app).contains("per message: 2 turns, 1000 tokens"));
        assert!(type_line(&mut app, "/turns lots").is_empty());
    }

    #[test]
    fn model_test_pings_the_right_thing() {
        let mut app = App::default();
        let effects = type_line(&mut app, "/model test codex");
        assert!(
            matches!(&effects[..], [Effect::TestModel { id, target: TestTarget::Cli { .. } }] if id == "codex")
        );
        reload(&mut app, &format!("{DEFAULT_FILE}{LLAMA}"));
        let effects = type_line(&mut app, "/model test llama");
        assert!(matches!(
            &effects[..],
            [Effect::TestModel {
                target: TestTarget::Http {
                    key_ref: Some(_),
                    ..
                },
                ..
            }]
        ));
    }

    #[test]
    fn keys_complete_scroll_add_lines_and_escape_slashes() {
        let mut app = App::default();
        for c in "/mod".chars() {
            app.update(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(app.completions(), ["/models", "/model"]);
        app.update(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input, "/models ");
        app.input.clear();
        app.update(key(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.scroll, 10);
        app.update(key(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(app.scroll, 0);
        for c in "line one".chars() {
            app.update(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.update(key(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(
            app.input, "line one\n",
            "Alt-Enter adds a line, doesn't send"
        );
        app.input.clear();
        let effects = type_line(&mut app, "\\/usage is a word here");
        assert!(effects.iter().any(
            |e| matches!(e, Effect::StartTurn { prompt, .. } if prompt == "/usage is a word here")
        ));
    }

    /// A past thread's transcript, as the loop would hand it over.
    fn past_thread(app: &App) -> Vec<transcript::EventLine> {
        let id = |p: &str| app.registry.get(p).unwrap().identity.clone();
        let party = |p: &str, identity: Option<String>| transcript::Party {
            id: p.into(),
            identity,
        };
        let line = |seq, from, to: Vec<transcript::Party>, kind, payload: serde_json::Value| {
            transcript::EventLine {
                seq,
                ts: 0,
                turn: None,
                parent: None,
                from,
                to,
                kind,
                payload,
                tokens_in: Some(3),
                tokens_out: Some(1),
            }
        };
        use transcript::Kind::*;
        vec![
            line(
                0,
                party("user", None),
                vec![party("llama", Some(id("llama")))],
                Msg,
                json!({"text": "q1"}),
            ),
            line(
                1,
                party("router", None),
                vec![party("llama", Some(id("llama")))],
                Delivery,
                json!({"status": "sent", "text": "q1"}),
            ),
            line(
                2,
                party("llama", Some(id("llama"))),
                vec![party("user", None)],
                Msg,
                json!({"text": "a1", "status": "complete"}),
            ),
            line(
                3,
                party("claude", Some(id("claude"))),
                vec![party("codex", Some(id("codex")))],
                Handoff,
                json!({"status": "sent", "prompt": "check this"}),
            ),
            line(
                4,
                party("router", None),
                vec![party("llama", Some("old-endpoint".into()))],
                Delivery,
                json!({"status": "sent", "text": "q0"}),
            ),
            line(
                5,
                party("llama", Some("old-endpoint".into())),
                vec![party("user", None)],
                Msg,
                json!({"text": "from the old endpoint", "status": "complete"}),
            ),
        ]
    }

    #[test]
    fn resume_rebuilds_the_chat_sessions_and_model_history() {
        let mut app = App::default();
        reload(&mut app, &format!("{DEFAULT_FILE}{LLAMA}"));
        let events = past_thread(&app);
        let claude = app.registry.get("claude").unwrap().identity.clone();
        let sessions = BTreeMap::from([
            (format!("claude/{claude}"), "S-claude".to_string()),
            ("codex/stale-identity".to_string(), "S-old".to_string()),
        ]);
        assert!(
            app.update(Event::Resumed {
                title: "old".into(),
                events,
                sessions,
                elsewhere: None,
            })
            .is_empty()
        );
        let shown: Vec<(&str, &str)> = app
            .lines
            .iter()
            .map(|l| (l.who.as_str(), l.text.as_str()))
            .collect();
        assert!(shown.contains(&("you → llama", "q1")));
        assert!(shown.contains(&("llama", "a1")));
        assert!(shown.contains(&("claude → codex", "check this")));
        assert_eq!(app.lines.last().unwrap().text, "resumed \"old\"");

        let effects = type_line(&mut app, "@llama q2");
        assert_eq!(
            chats(&effects)[0].1,
            [("q1".to_string(), "a1".to_string())],
            "only exchanges with the current endpoint"
        );
        let effects = type_line(&mut app, "@claude hi");
        assert!(
            effects.iter().any(
                |e| matches!(e, Effect::StartTurn { session: Some(s), .. } if s == "S-claude")
            )
        );
        let effects = type_line(&mut app, "@codex hi");
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::StartTurn { session: None, .. })),
            "a session made for another identity is not resumed"
        );
    }

    #[test]
    fn a_thread_from_another_checkout_is_read_only() {
        let mut app = App::default();
        reload(&mut app, &format!("{DEFAULT_FILE}{LLAMA}"));
        let events = past_thread(&app);
        app.update(Event::Resumed {
            title: "elsewhere".into(),
            events,
            sessions: BTreeMap::new(),
            elsewhere: Some("/other/repo".into()),
        });
        assert!(last_text(&app).contains("read-only"));
        assert!(type_line(&mut app, "hello").is_empty());
        assert!(last_text(&app).contains("belongs to /other/repo"));
        app.input.clear();
        assert_eq!(type_line(&mut app, "/clear"), [Effect::NewThread]);
        assert!(
            !type_line(&mut app, "hello").is_empty(),
            "a new thread can be written"
        );
    }

    #[test]
    fn resume_waits_for_running_replies() {
        let mut app = App::default();
        assert_eq!(type_line(&mut app, "/resume"), [Effect::Resume(None)]);
        type_line(&mut app, "hi");
        assert!(type_line(&mut app, "/resume 1").is_empty());
        app.update(agent("claude", PEvent::Done(Outcome::Complete)));
        assert_eq!(
            type_line(&mut app, "/resume 1"),
            [Effect::Resume(Some("1".into()))]
        );
    }
}
