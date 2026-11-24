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
    /// The watcher saw `models.toml` change. The loop reads it (I/O) and turns this into
    /// `Registry`, so `update` never sees it.
    RegistryFileChanged,
    /// `models.toml` changed on disk: the new registry, or why it was not applied.
    Registry(Result<(Registry, Checks), String>),
    /// Something the loop failed to do, shown in the chat.
    Notice(String),
}

/// A change to `models.toml` asked for by a command; the loop writes it.
#[derive(Debug, Clone, PartialEq)]
pub enum FileEdit {
    Enabled { id: String, enabled: bool },
    Default(String),
}

/// What `update` asks the loop to do. The loop executes these; results come back as `Event`s.
#[derive(Debug, PartialEq)]
pub enum Effect {
    Redraw,
    Quit,
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
    /// The user confirmed the checkout is quiet: close these threads' unfinished writer
    /// turns with a `settle`, which unlocks edits (ADR 0010).
    ConfirmQuiet(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
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
    queue: VecDeque<(String, bool)>,
    /// The running turn may edit; its reply then ends the writer marker.
    writable: bool,
    /// Index of the reply being streamed in `lines`.
    reply: Option<usize>,
    usage: (u64, u64, Option<f64>),
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
    /// Threads whose writer turn never finished (a crash): edits stay locked until `/quiet`.
    edits_locked: Vec<String>,
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
            edits_locked: vec![],
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
    pub fn lock_edits(&mut self, threads: Vec<String>, checkout: &str) {
        if threads.is_empty() {
            return;
        }
        self.push(
            "roundtable",
            format!(
                "edits are locked: a writer from an earlier run never finished in {checkout} and \
                 may still be running. Check nothing is editing there, then type /quiet. \
                 Until then every turn is read-only."
            ),
            LineKind::Error,
        );
        self.edits_locked = threads;
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
            Event::Key(_) | Event::Resize | Event::Tick | Event::RegistryFileChanged => vec![],
            Event::InputClosed(reason) => {
                tracing::error!(%reason, "terminal input closed");
                vec![Effect::Quit]
            }
            Event::Agent(id, event) => self.agent_event(&id, event),
            Event::Registry(Ok((registry, checks))) => self.apply_registry(registry, checks, true),
            Event::Registry(Err(why)) => {
                self.push(
                    "roundtable",
                    format!("models.toml not applied, the previous registry stays: {why}"),
                    LineKind::Error,
                );
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
                return vec![Effect::Quit];
            }
            self.quit_armed = true;
            self.push("roundtable", "Ctrl-C again to quit".into(), LineKind::Info);
            return vec![];
        }
        self.quit_armed = false;
        match key.code {
            KeyCode::Char('d') if ctrl && self.input.is_empty() => vec![Effect::Quit],
            KeyCode::Char(c) if !ctrl => {
                self.input.push(c);
                vec![]
            }
            KeyCode::Backspace => {
                self.input.pop();
                vec![]
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Esc => self.cancel_all(),
            _ => vec![],
        }
    }

    fn submit(&mut self) -> Vec<Effect> {
        let input = std::mem::take(&mut self.input);
        if input.trim_start().starts_with('/') {
            return self.command(input.trim());
        }
        let (targets, prompt) = match self.route(&input) {
            Ok(routed) => routed,
            Err(why) => {
                self.input = input; // keep what was typed, so it can be fixed
                self.push("roundtable", why, LineKind::Error);
                return vec![];
            }
        };
        // Only a message to the writer alone may edit: `@all` and several mentions are
        // read-only for everyone (ADR 0004).
        let to_writer = targets.len() == 1 && self.registry.writer() == Some(targets[0].as_str());
        let writable = to_writer && self.edits_locked.is_empty();
        let edits = if writable { " (may edit)" } else { "" };
        self.push(
            &format!("you → {}{edits}", targets.join(", ")),
            prompt.clone(),
            LineKind::User,
        );
        if to_writer && !writable {
            self.push(
                "roundtable",
                "read-only: edits are locked until /quiet".into(),
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
        for id in targets {
            effects.extend(self.deliver(&id, prompt.clone(), writable));
        }
        effects
    }

    // shortcut: only the registry commands of ADR 0011; ADR 0008 brings the full command table.
    fn command(&mut self, line: &str) -> Vec<Effect> {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["/models"] => {
                let text = self.models_table();
                self.push("roundtable", text, LineKind::Info);
                vec![]
            }
            ["/model", "enable", id] => self.set_enabled(id, true),
            ["/model", "disable", id] => self.set_enabled(id, false),
            ["/model", "use", id] => self.use_default(id),
            ["/quiet"] if !self.edits_locked.is_empty() => {
                self.push("roundtable", "edits unlocked".into(), LineKind::Info);
                vec![Effect::ConfirmQuiet(std::mem::take(&mut self.edits_locked))]
            }
            ["/quiet"] => {
                self.push("roundtable", "edits are not locked".into(), LineKind::Info);
                vec![]
            }
            _ => {
                self.push(
                    "roundtable",
                    "commands so far: /models, /model enable|disable|use <id>, /quiet".into(),
                    LineKind::Error,
                );
                vec![]
            }
        }
    }

    fn models_table(&self) -> String {
        let mut rows = vec![];
        for p in &self.registry.participants {
            let mut tags = vec![];
            if self.registry.default() == Some(p.id.as_str()) {
                tags.push("default");
            }
            if self.registry.writer() == Some(p.id.as_str()) {
                tags.push(if self.edits_locked.is_empty() {
                    "writer"
                } else {
                    "writer, locked until /quiet"
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
        }
        for p in &next.participants {
            if self.seat_mut(&p.id).is_none() {
                self.seats.push(Seat {
                    id: p.id.clone(),
                    busy: false,
                    session: None,
                    queue: VecDeque::new(),
                    writable: false,
                    reply: None,
                    usage: (0, 0, None),
                });
            }
        }
        let warnings = next.warnings.clone();
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
        if seat.busy {
            vec![Effect::CancelTurn(id.into())]
        } else {
            vec![]
        }
    }

    fn availability(&self, p: &Participant) -> Result<(), String> {
        if p.adapter == Adapter::OpenAi {
            return Err("OpenAI-compatible models answer from ADR 0005 on".into());
        }
        match self.checks.get(&p.id) {
            Some(Ok(_)) => Ok(()),
            Some(Err(why)) => Err(why.clone()),
            None => Err("not checked yet".into()),
        }
    }

    /// `@id` and `@all` at the start pick recipients; none means the default.
    fn route(&self, input: &str) -> Result<(Vec<String>, String), String> {
        let mut targets: Vec<String> = vec![];
        let mut rest = input.trim_start();
        while let Some(mention) = rest.strip_prefix('@') {
            let end = mention.find(char::is_whitespace).unwrap_or(mention.len());
            let name = &mention[..end];
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
        Ok((targets, prompt.into()))
    }

    fn deliver(&mut self, id: &str, prompt: String, writable: bool) -> Vec<Effect> {
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
        if seat.busy {
            seat.queue.push_back((prompt, writable));
            self.push("roundtable", format!("queued for {id}"), LineKind::Info);
            return vec![];
        }
        self.start(&participant, prompt, writable)
    }

    fn start(&mut self, p: &Participant, prompt: String, writable: bool) -> Vec<Effect> {
        let Adapter::Cli(kind) = p.adapter else {
            unreachable!("availability() refuses non-cli participants for now");
        };
        // One writable turn per checkout (ADR 0004): if the writer changed while the old one
        // is still editing, the new turn waits its turn as read-only instead of overlapping.
        let overlap = self.seats.iter().any(|s| s.busy && s.writable);
        let writable = if writable && overlap {
            self.push(
                "roundtable",
                format!("{}: read-only, another writer is still editing", p.id),
                LineKind::Info,
            );
            false
        } else {
            writable
        };
        let index = self.lines.len();
        self.lines.push(ChatLine {
            who: p.id.clone(),
            text: String::new(),
            kind: LineKind::Streaming,
        });
        let seat = self.seat_mut(&p.id).expect("seat");
        seat.busy = true;
        seat.writable = writable;
        seat.reply = Some(index);
        seat.usage = (0, 0, None);
        let session = seat.session.clone();
        vec![
            record(
                router(),
                vec![self.party(&p.id)],
                transcript::Kind::Delivery,
                // A writable delivery is the writer marker: synced before the CLI starts.
                json!({ "status": "sent", "text": prompt, "writable": writable }),
            )
            .durable(),
            Effect::StartTurn {
                id: p.id.clone(),
                kind,
                cmd: p.cmd.clone().unwrap_or_else(|| kind.cmd().into()),
                model: p.model.clone(),
                prompt,
                session,
                writable,
                permission_mode: p.permission_mode.clone(),
            },
        ]
    }

    fn cancel_all(&mut self) -> Vec<Effect> {
        let mut any = false;
        for seat in &mut self.seats {
            seat.queue.clear();
            any |= seat.busy;
        }
        if any {
            vec![Effect::CancelTurns]
        } else {
            vec![]
        }
    }

    fn agent_event(&mut self, id: &str, event: PEvent) -> Vec<Effect> {
        let identity = self
            .registry
            .get(id)
            .map(|p| p.identity.clone())
            .unwrap_or_default();
        let party = self.party(id);
        let Some(seat) = self.seat_mut(id) else {
            return vec![];
        };
        let reply = seat.reply;
        match event {
            PEvent::Session(session) => {
                if seat.session.as_deref() == Some(session.as_str()) {
                    return vec![];
                }
                seat.session = Some(session.clone());
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
            } => {
                seat.usage = (input, output, cost_usd);
                vec![]
            }
            PEvent::Error(text) => {
                self.push(id, text, LineKind::Error);
                vec![]
            }
            PEvent::Done(outcome) => self.finish(id, outcome),
        }
    }

    fn finish(&mut self, id: &str, outcome: Outcome) -> Vec<Effect> {
        let party = self.party(id);
        let seat = self.seat_mut(id).expect("seat");
        seat.busy = false;
        let (tokens_in, tokens_out, cost) = seat.usage;
        let writable = std::mem::take(&mut seat.writable);
        let text = match seat.reply.take() {
            Some(i) => {
                self.lines[i].kind = LineKind::Reply(outcome);
                self.lines[i].text.clone()
            }
            None => String::new(),
        };
        let mut reply = record(
            party,
            vec![user()],
            transcript::Kind::Msg,
            // `writable` here closes the writer marker of the delivery (ADR 0010).
            json!({ "text": text, "status": outcome.as_str(), "cost_usd": cost, "writable": writable }),
        )
        .durable();
        if let Effect::Record { event, .. } = &mut reply {
            event.tokens_in = Some(tokens_in);
            event.tokens_out = Some(tokens_out);
        }
        let mut effects = vec![reply];
        let next = self.seat_mut(id).and_then(|s| s.queue.pop_front());
        if let Some((prompt, writable)) = next
            && let Some(p) = self.registry.get(id).cloned()
        {
            // Rechecked: the writer or the lock may have changed while it waited.
            let writable =
                writable && self.registry.writer() == Some(id) && self.edits_locked.is_empty();
            effects.extend(self.start(&p, prompt, writable));
        }
        effects
    }

    fn seat_mut(&mut self, id: &str) -> Option<&mut Seat> {
        self.seats.iter_mut().find(|s| s.id == id)
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
        app.lock_edits(vec!["T1".into()], "/repo");
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
            [Effect::ConfirmQuiet(vec!["T1".into()])]
        );
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
        assert_eq!(writable(&effects), [("codex".into(), false)]);
        assert!(
            app.lines
                .iter()
                .any(|l| l.text == "codex: read-only, another writer is still editing")
        );
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
    fn an_invalid_file_changes_nothing() {
        let mut app = App::default();
        app.update(Event::Registry(Err("line 3: oops".into())));
        assert!(last_text(&app).starts_with("models.toml not applied"));
        assert_eq!(started(&type_line(&mut app, "@codex hi")), ["codex"]);
    }

    #[test]
    fn openai_models_are_listed_but_not_yet_served() {
        let text = format!(
            "{DEFAULT_FILE}\n[[model]]\nid = \"llama\"\nkind = \"openai\"\nbase_url = \"http://localhost:11434/v1\"\nmodel = \"llama3.3\"\n"
        );
        let mut app = App::default();
        reload(&mut app, &text);
        type_line(&mut app, "/models");
        assert!(last_text(&app).contains(
            "llama (openai, llama3.3) unavailable: OpenAI-compatible models answer from ADR 0005 on"
        ));
        assert_eq!(
            started(&type_line(&mut app, "@all hi")),
            ["claude", "codex"]
        );
    }
}
