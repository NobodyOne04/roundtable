//! App state and its single update function (ADR 0002).
//! Only the main loop owns `App`; every other task talks to it through `Event`s.

use std::collections::VecDeque;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::json;

use crate::agent::{Kind, Outcome, PEvent};
use crate::transcript::{self, NewEvent, Party};

/// Everything that can happen to the app. Every source sends these into one channel.
#[derive(Debug)]
pub enum Event {
    Key(KeyEvent),
    Resize,
    /// Redraw throttle: at most one draw per tick.
    Tick,
    /// The terminal input stream ended or failed; nothing more can be typed.
    InputClosed(String),
    /// Output of a running agent turn.
    Agent(Kind, PEvent),
    /// Something the loop failed to do, shown in the chat.
    Notice(String),
}

/// What `update` asks the loop to do. The loop executes these; results come back as `Event`s.
#[derive(Debug, PartialEq)]
pub enum Effect {
    Redraw,
    Quit,
    StartTurn {
        agent: Kind,
        prompt: String,
        session: Option<String>,
    },
    /// Cancel every running turn (`Esc`).
    CancelTurns,
    /// Append to the transcript; the loop opens the thread on the first one.
    Record {
        event: NewEvent,
        durable: bool,
    },
    /// Persist an agent's native session id for this thread.
    SaveSession {
        agent: Kind,
        session: String,
    },
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

#[derive(Debug)]
struct AgentState {
    kind: Kind,
    /// `Err(why)` when the agent can't take turns (preflight, ADR 0004).
    available: Result<(), String>,
    busy: bool,
    session: Option<String>,
    /// Direct messages waiting while the agent is busy (ADR 0007: queued, not dropped).
    queue: VecDeque<String>,
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
    agents: Vec<AgentState>,
    /// First Ctrl-C arms, the second quits.
    quit_armed: bool,
}

impl Default for App {
    /// All agents available; the binary passes real preflight results to `new`.
    fn default() -> Self {
        Self::new(Kind::ALL.iter().map(|&k| (k, Ok(()))).collect())
    }
}

impl App {
    pub fn new(agents: Vec<(Kind, Result<(), String>)>) -> Self {
        Self {
            // Dirty from the start, so the first tick draws the screen.
            dirty: true,
            lines: vec![],
            input: String::new(),
            agents: agents
                .into_iter()
                .map(|(kind, available)| AgentState {
                    kind,
                    available,
                    busy: false,
                    session: None,
                    queue: VecDeque::new(),
                    reply: None,
                    usage: (0, 0, None),
                })
                .collect(),
            quit_armed: false,
        }
    }

    /// Adds a line before the first draw, e.g. startup notes from preflight.
    pub fn note(&mut self, text: String, is_error: bool) {
        let kind = if is_error {
            LineKind::Error
        } else {
            LineKind::Info
        };
        self.push("roundtable", text, kind);
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
            Event::Key(_) | Event::Resize | Event::Tick => vec![],
            Event::InputClosed(reason) => {
                tracing::error!(%reason, "terminal input closed");
                vec![Effect::Quit]
            }
            Event::Agent(kind, event) => self.agent_event(kind, event),
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
            KeyCode::Esc => self.cancel(),
            _ => vec![],
        }
    }

    fn submit(&mut self) -> Vec<Effect> {
        let input = std::mem::take(&mut self.input);
        let (targets, prompt) = match self.route(&input) {
            Ok(routed) => routed,
            Err(why) => {
                self.input = input; // keep what was typed, so it can be fixed
                self.push("roundtable", why, LineKind::Error);
                return vec![];
            }
        };
        let names: Vec<&str> = targets.iter().map(|k| k.id()).collect();
        self.push(
            &format!("you → {}", names.join(", ")),
            prompt.clone(),
            LineKind::User,
        );
        let mut effects = vec![Effect::Record {
            event: NewEvent {
                turn: None,
                parent: None,
                from: user(),
                to: targets.iter().map(|&k| party(k)).collect(),
                kind: transcript::Kind::Msg,
                payload: json!({ "text": prompt }),
                tokens_in: None,
                tokens_out: None,
            },
            durable: false,
        }];
        for kind in targets {
            effects.extend(self.deliver(kind, prompt.clone()));
        }
        effects
    }

    /// `@claude`, `@codex`, `@all` at the start pick recipients; none means the default.
    fn route(&self, input: &str) -> Result<(Vec<Kind>, String), String> {
        let mut targets = vec![];
        let mut rest = input.trim_start();
        while let Some(mention) = rest.strip_prefix('@') {
            let end = mention.find(char::is_whitespace).unwrap_or(mention.len());
            let name = &mention[..end];
            if name == "all" {
                targets.extend(
                    self.agents
                        .iter()
                        .filter(|a| a.available.is_ok())
                        .map(|a| a.kind),
                );
            } else {
                targets.push(Kind::from_id(name).ok_or_else(|| format!("no one called @{name}"))?);
            }
            rest = mention[end..].trim_start();
        }
        let prompt = rest.trim();
        if prompt.is_empty() {
            return Err("nothing to send".into());
        }
        if targets.is_empty() {
            // shortcut: the default is fixed until ADR 0011's registry names one.
            targets.push(Kind::Claude);
        }
        let mut seen = vec![];
        targets.retain(|k| {
            let new = !seen.contains(k);
            seen.push(*k);
            new
        });
        Ok((targets, prompt.into()))
    }

    fn deliver(&mut self, kind: Kind, prompt: String) -> Vec<Effect> {
        let agent = self.agent_mut(kind);
        if let Err(why) = &agent.available {
            let why = format!("{} is unavailable: {why}", kind.id());
            self.push("roundtable", why, LineKind::Error);
            return vec![];
        }
        if agent.busy {
            agent.queue.push_back(prompt);
            self.push(
                "roundtable",
                format!("queued for {}", kind.id()),
                LineKind::Info,
            );
            return vec![];
        }
        self.start(kind, prompt)
    }

    fn start(&mut self, kind: Kind, prompt: String) -> Vec<Effect> {
        let index = self.lines.len();
        self.lines.push(ChatLine {
            who: kind.id().into(),
            text: String::new(),
            kind: LineKind::Streaming,
        });
        let agent = self.agent_mut(kind);
        agent.busy = true;
        agent.reply = Some(index);
        agent.usage = (0, 0, None);
        let session = agent.session.clone();
        vec![
            Effect::Record {
                event: NewEvent {
                    turn: None,
                    parent: None,
                    from: Party {
                        id: "router".into(),
                        identity: None,
                    },
                    to: vec![party(kind)],
                    kind: transcript::Kind::Delivery,
                    payload: json!({ "status": "sent", "text": prompt }),
                    tokens_in: None,
                    tokens_out: None,
                },
                durable: true,
            },
            Effect::StartTurn {
                agent: kind,
                prompt,
                session,
            },
        ]
    }

    fn cancel(&mut self) -> Vec<Effect> {
        let mut any = false;
        for agent in &mut self.agents {
            agent.queue.clear();
            any |= agent.busy;
        }
        if any {
            vec![Effect::CancelTurns]
        } else {
            vec![]
        }
    }

    fn agent_event(&mut self, kind: Kind, event: PEvent) -> Vec<Effect> {
        let agent = self.agent_mut(kind);
        let reply = agent.reply;
        match event {
            PEvent::Session(id) => {
                if agent.session.as_deref() == Some(id.as_str()) {
                    return vec![];
                }
                agent.session = Some(id.clone());
                vec![Effect::SaveSession {
                    agent: kind,
                    session: id,
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
                    who: kind.id().into(),
                    text: summary.clone(),
                    kind: LineKind::Tool,
                };
                match reply {
                    Some(i) => {
                        self.lines.insert(i, line);
                        for agent in &mut self.agents {
                            if let Some(r) = agent.reply.as_mut()
                                && *r >= i
                            {
                                *r += 1;
                            }
                        }
                    }
                    None => self.lines.push(line),
                }
                vec![Effect::Record {
                    event: NewEvent {
                        turn: None,
                        parent: None,
                        from: party(kind),
                        to: vec![],
                        kind: transcript::Kind::ToolCall,
                        payload: json!({ "summary": summary }),
                        tokens_in: None,
                        tokens_out: None,
                    },
                    durable: false,
                }]
            }
            PEvent::Usage {
                input,
                output,
                cost_usd,
            } => {
                agent.usage = (input, output, cost_usd);
                vec![]
            }
            PEvent::Error(text) => {
                self.push(kind.id(), text, LineKind::Error);
                vec![]
            }
            PEvent::Done(outcome) => self.finish(kind, outcome),
        }
    }

    fn finish(&mut self, kind: Kind, outcome: Outcome) -> Vec<Effect> {
        let agent = self.agent_mut(kind);
        agent.busy = false;
        let (tokens_in, tokens_out, cost) = agent.usage;
        let text = match agent.reply.take() {
            Some(i) => {
                self.lines[i].kind = LineKind::Reply(outcome);
                self.lines[i].text.clone()
            }
            None => String::new(),
        };
        let mut effects = vec![Effect::Record {
            event: NewEvent {
                turn: None,
                parent: None,
                from: party(kind),
                to: vec![user()],
                kind: transcript::Kind::Msg,
                payload: json!({ "text": text, "status": outcome.as_str(), "cost_usd": cost }),
                tokens_in: Some(tokens_in),
                tokens_out: Some(tokens_out),
            },
            durable: true,
        }];
        if let Some(next) = self.agent_mut(kind).queue.pop_front() {
            effects.extend(self.start(kind, next));
        }
        effects
    }

    fn agent_mut(&mut self, kind: Kind) -> &mut AgentState {
        self.agents
            .iter_mut()
            .find(|a| a.kind == kind)
            .expect("every Kind has a state")
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

// shortcut: no identity hash until ADR 0011 defines each participant's destination.
fn party(kind: Kind) -> Party {
    Party {
        id: kind.id().into(),
        identity: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    fn type_line(app: &mut App, text: &str) -> Vec<Effect> {
        for c in text.chars() {
            app.update(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.update(key(KeyCode::Enter, KeyModifiers::NONE))
    }

    fn started(effects: &[Effect]) -> Vec<Kind> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::StartTurn { agent, .. } => Some(*agent),
                _ => None,
            })
            .collect()
    }

    fn last_text(app: &App) -> &str {
        &app.lines.last().unwrap().text
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
    fn typing_q_is_text_not_quit() {
        let mut app = App::default();
        assert!(
            app.update(key(KeyCode::Char('q'), KeyModifiers::NONE))
                .is_empty()
        );
        assert_eq!(app.input, "q");
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
    fn routes_by_mention_with_claude_as_default() {
        assert_eq!(
            started(&type_line(&mut App::default(), "hello")),
            [Kind::Claude]
        );
        assert_eq!(
            started(&type_line(&mut App::default(), "@codex review")),
            [Kind::Codex]
        );
        assert_eq!(
            started(&type_line(&mut App::default(), "@all hi")),
            [Kind::Claude, Kind::Codex]
        );
        assert_eq!(
            started(&type_line(&mut App::default(), "@codex @claude @codex hi")),
            [Kind::Codex, Kind::Claude],
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
        let mut app = App::new(vec![
            (Kind::Claude, Ok(())),
            (Kind::Codex, Err("not inside a git repo".into())),
        ]);
        assert!(started(&type_line(&mut app, "@codex hi")).is_empty());
        assert_eq!(
            last_text(&app),
            "codex is unavailable: not inside a git repo"
        );
        assert_eq!(
            started(&type_line(&mut app, "@all hi")),
            [Kind::Claude],
            "@all means everyone available"
        );
    }

    #[test]
    fn a_turn_streams_records_and_resumes_its_session() {
        let mut app = App::default();
        type_line(&mut app, "ping");
        assert_eq!(
            app.update(Event::Agent(Kind::Claude, PEvent::Session("S1".into()))),
            [Effect::SaveSession {
                agent: Kind::Claude,
                session: "S1".into()
            }]
        );
        app.update(Event::Agent(Kind::Claude, PEvent::TextDelta("po".into())));
        app.update(Event::Agent(Kind::Claude, PEvent::TextDelta("ng".into())));
        app.update(Event::Agent(
            Kind::Claude,
            PEvent::Usage {
                input: 10,
                output: 2,
                cost_usd: Some(0.01),
            },
        ));
        let effects = app.update(Event::Agent(Kind::Claude, PEvent::Done(Outcome::Complete)));
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
            }
            other => panic!("{other:?}"),
        }
        assert!(type_line(&mut app, "again").contains(&Effect::StartTurn {
            agent: Kind::Claude,
            prompt: "again".into(),
            session: Some("S1".into())
        }));
    }

    #[test]
    fn a_busy_agent_queues_and_esc_cancels_everything() {
        let mut app = App::default();
        type_line(&mut app, "first");
        assert!(started(&type_line(&mut app, "second")).is_empty());
        assert_eq!(last_text(&app), "queued for claude");
        let effects = app.update(Event::Agent(Kind::Claude, PEvent::Done(Outcome::Complete)));
        assert_eq!(
            started(&effects),
            [Kind::Claude],
            "the queue drains when the turn ends"
        );

        type_line(&mut app, "third");
        assert_eq!(
            app.update(key(KeyCode::Esc, KeyModifiers::NONE)),
            [Effect::CancelTurns]
        );
        let effects = app.update(Event::Agent(
            Kind::Claude,
            PEvent::Done(Outcome::Interrupted),
        ));
        assert!(started(&effects).is_empty(), "Esc also clears the queue");
    }

    #[test]
    fn tool_lines_go_above_the_reply_they_belong_to() {
        let mut app = App::default();
        type_line(&mut app, "@all go");
        app.update(Event::Agent(Kind::Claude, PEvent::Tool("Read".into())));
        app.update(Event::Agent(Kind::Codex, PEvent::Tool("ls".into())));
        app.update(Event::Agent(Kind::Codex, PEvent::Message("done".into())));
        app.update(Event::Agent(Kind::Claude, PEvent::TextDelta("hi".into())));
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
}
