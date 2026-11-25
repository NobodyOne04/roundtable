//! CLI agents (ADR 0004): Claude Code and Codex, driven through their own CLIs' JSON
//! streaming modes, never a scraped terminal. Each adapter builds its command and maps its
//! CLI's events to `PEvent`; one runner owns the process from spawn to reap.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::app::Event;

/// Grace period between SIGTERM and SIGKILL when a turn is cancelled.
const KILL_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Claude,
    Codex,
}

impl Kind {
    pub const ALL: [Kind; 2] = [Kind::Claude, Kind::Codex];

    /// The id used in `@mentions` and the transcript.
    pub fn id(self) -> &'static str {
        match self {
            Kind::Claude => "claude",
            Kind::Codex => "codex",
        }
    }

    pub fn from_id(id: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.id() == id)
    }

    /// The CLI binary.
    pub fn cmd(self) -> &'static str {
        self.id()
    }
}

/// What a running turn reports, mapped from each CLI's own events.
#[derive(Debug, Clone, PartialEq)]
pub enum PEvent {
    /// The CLI's native session/thread id, kept so the next turn resumes it.
    Session(String),
    TextDelta(String),
    /// A whole message: Codex sends no deltas.
    Message(String),
    /// A tool the agent ran, as one line.
    Tool(String),
    Usage {
        input: u64,
        output: u64,
        cost_usd: Option<f64>,
        /// Counted by roundtable because the provider didn't report it (ADR 0005).
        estimated: bool,
    },
    Error(String),
    /// The CLI's plan limits (ADR 0014): Claude reports them in its stream, Codex in its
    /// session file, read when the turn ends.
    Limits(crate::limits::Limits),
    /// Always the last event of a turn, sent after the process and its group are gone.
    Done(Outcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Complete,
    Interrupted,
    Failed,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Complete => "complete",
            Outcome::Interrupted => "interrupted",
            Outcome::Failed => "failed",
        }
    }
}

/// One turn for one agent.
#[derive(Debug, Clone)]
pub struct Turn {
    /// The CLI to run, as configured (a name on `PATH` or a full path).
    pub cmd: String,
    pub prompt: String,
    /// Native session to resume; `None` starts a new one.
    pub session: Option<String>,
    pub cwd: PathBuf,
    /// The user confirmed running Codex outside a git repo (`--outside-repo`).
    pub outside_repo: bool,
    /// From the registry; `None` lets the CLI pick.
    pub model: Option<String>,
    /// The writer's own turn: may edit files (ADR 0004). Every other turn is read-only.
    pub writable: bool,
    /// Gives the turn the `ask` tool (ADR 0007): Claude only for now.
    pub ask: Option<AskBridge>,
    /// The writer's configured mode; `None` means a safe explicit default, never whatever
    /// the user's own CLI config says (it could be bypass). The registry refuses bypass and
    /// full-access modes.
    pub permission_mode: Option<String>,
}

/// What a turn needs to reach roundtable's `ask` tool: the CLI launches `exe mcp`, which
/// talks to `socket` as `caller`, proving it with `token` (one per turn).
#[derive(Debug, Clone, PartialEq)]
pub struct AskBridge {
    pub exe: PathBuf,
    pub socket: PathBuf,
    pub caller: String,
    pub token: String,
    /// Where the loop wrote `claude_config()` (0600, owner-only folder): the token never
    /// appears on a command line, where `ps` would show it.
    pub config_file: PathBuf,
}

impl AskBridge {
    /// Claude's `--mcp-config` JSON: the roundtable server, and only it. Written to
    /// `config_file` by the loop; never passed inline.
    pub fn claude_config(&self) -> String {
        json!({"mcpServers": {"roundtable": {
            "command": self.exe,
            "args": ["mcp"],
            "env": {
                crate::mcp::ENV_SOCKET: self.socket,
                crate::mcp::ENV_CALLER: self.caller,
                crate::mcp::ENV_TOKEN: self.token,
            }
        }}})
        .to_string()
    }
}

/// How long an `ask` may take before the CLI gives up on the tool call: the asked model
/// may run a whole turn.
const ASK_TIMEOUT_MS: &str = "900000";

/// The command for a turn, plus what goes on its stdin. The prompt always travels on stdin:
/// no quoting issues, no length limit, not visible in `ps`.
pub fn command(kind: Kind, turn: &Turn) -> (Command, Vec<u8>) {
    let mut cmd = Command::new(&turn.cmd);
    cmd.current_dir(&turn.cwd);
    match kind {
        Kind::Claude => {
            cmd.args([
                "-p",
                "--output-format",
                "stream-json",
                "--input-format",
                "stream-json",
                "--include-partial-messages",
                "--verbose",
            ]);
            let servers = turn.ask.as_ref().map_or_else(
                || r#"{"mcpServers":{}}"#.to_string(),
                |bridge| bridge.config_file.to_string_lossy().into_owned(),
            );
            if turn.writable {
                // The writer runs with the user's own setup, but always with an explicit mode,
                // so a bypass default in the user's settings can't apply.
                let mode = turn.permission_mode.as_deref().unwrap_or("default");
                cmd.args(["--permission-mode", mode]);
                if turn.ask.is_some() {
                    cmd.args(["--mcp-config", &servers]);
                }
            } else {
                // Read-only, and without the user's MCP servers, hooks and plugins, which
                // could write despite plan mode: only roundtable's own server, if any.
                cmd.args([
                    "--permission-mode",
                    "plan",
                    "--strict-mcp-config",
                    "--mcp-config",
                    &servers,
                    "--setting-sources",
                    "",
                ]);
            }
            if turn.ask.is_some() {
                // Registering a server doesn't let a headless turn call it: grant exactly
                // this tool; who may ask whom is the router's decision (ADR 0007).
                cmd.args(["--allowedTools", "mcp__roundtable__ask"]);
                cmd.env("MCP_TOOL_TIMEOUT", ASK_TIMEOUT_MS);
            }
            if let Some(session) = &turn.session {
                cmd.args(["--resume", session]);
            }
            if let Some(model) = &turn.model {
                cmd.args(["--model", model]);
            }
            let line = json!({"type": "user", "message": {"role": "user", "content": turn.prompt}});
            (cmd, format!("{line}\n").into_bytes())
        }
        Kind::Codex => {
            cmd.arg("exec");
            if let Some(session) = &turn.session {
                cmd.args(["resume", session.as_str()]);
            }
            cmd.arg("--json");
            // The sandbox is set through `-c` because `exec resume` has no `-s`.
            if turn.writable {
                // Always explicit, so a full-access sandbox in the user's config can't apply.
                let mode = turn.permission_mode.as_deref().unwrap_or("workspace-write");
                cmd.args(["-c", &format!("sandbox_mode=\"{mode}\"")]);
            } else {
                // `--ignore-user-config` drops the user's MCP servers (auth still works).
                cmd.args(["--ignore-user-config", "-c", r#"sandbox_mode="read-only""#]);
            }
            if turn.outside_repo {
                cmd.arg("--skip-git-repo-check");
            }
            if let Some(model) = &turn.model {
                cmd.args(["-m", model]);
            }
            cmd.arg("-");
            (cmd, turn.prompt.clone().into_bytes())
        }
    }
}

/// Maps one JSON line of a CLI's output. Unknown events map to nothing: CLI formats change
/// between versions, and a new event type must never break a turn.
pub fn parse(kind: Kind, line: &str) -> Vec<PEvent> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        // Never the line itself: it may hold tool output or credentials.
        tracing::debug!(agent = kind.id(), bytes = line.len(), "not JSON, skipped");
        return vec![];
    };
    match kind {
        Kind::Claude => parse_claude(&v),
        Kind::Codex => parse_codex(&v),
    }
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter().try_fold(v, |v, k| v.get(k))?.as_str()
}

fn u64_at(v: &Value, path: &[&str]) -> u64 {
    path.iter()
        .try_fold(v, |v, k| v.get(k))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn parse_claude(v: &Value) -> Vec<PEvent> {
    match (str_at(v, &["type"]), str_at(v, &["subtype"])) {
        (Some("system"), Some("init")) => str_at(v, &["session_id"])
            .map(|id| vec![PEvent::Session(id.into())])
            .unwrap_or_default(),
        (Some("stream_event"), _) => {
            if str_at(v, &["event", "delta", "type"]) == Some("text_delta") {
                str_at(v, &["event", "delta", "text"])
                    .map(|t| vec![PEvent::TextDelta(t.into())])
                    .unwrap_or_default()
            } else {
                vec![]
            }
        }
        (Some("assistant"), _) => v
            .pointer("/message/content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|c| str_at(c, &["type"]) == Some("tool_use"))
            .map(|c| {
                let name = str_at(c, &["name"]).unwrap_or("tool");
                let input = c.get("input").map(Value::to_string).unwrap_or_default();
                PEvent::Tool(format!("{name} {}", truncate(&input, 80)))
            })
            .collect(),
        (Some("rate_limit_event"), _) => crate::limits::from_claude(v, now_ms())
            .map(|l| vec![PEvent::Limits(l)])
            .unwrap_or_default(),
        (Some("result"), _) => {
            let mut out = vec![PEvent::Usage {
                input: u64_at(v, &["usage", "input_tokens"])
                    + u64_at(v, &["usage", "cache_read_input_tokens"]),
                output: u64_at(v, &["usage", "output_tokens"]),
                cost_usd: v.get("total_cost_usd").and_then(Value::as_f64),
                estimated: false,
            }];
            if v.get("is_error").and_then(Value::as_bool) == Some(true) {
                let msg = str_at(v, &["result"]).unwrap_or("Claude reported an error");
                out.push(PEvent::Error(msg.into()));
            }
            out
        }
        _ => vec![],
    }
}

fn parse_codex(v: &Value) -> Vec<PEvent> {
    match str_at(v, &["type"]) {
        Some("thread.started") => str_at(v, &["thread_id"])
            .map(|id| vec![PEvent::Session(id.into())])
            .unwrap_or_default(),
        Some("item.completed") if str_at(v, &["item", "type"]) == Some("agent_message") => {
            str_at(v, &["item", "text"])
                .map(|t| vec![PEvent::Message(t.into())])
                .unwrap_or_default()
        }
        Some("item.started") if str_at(v, &["item", "type"]) == Some("command_execution") => {
            str_at(v, &["item", "command"])
                .map(|c| vec![PEvent::Tool(truncate(c, 100))])
                .unwrap_or_default()
        }
        Some("turn.completed") => vec![PEvent::Usage {
            input: u64_at(v, &["usage", "input_tokens"]),
            output: u64_at(v, &["usage", "output_tokens"]),
            cost_usd: None,
            estimated: false,
        }],
        Some("error") => vec![PEvent::Error(
            str_at(v, &["message"]).unwrap_or("Codex error").into(),
        )],
        Some("turn.failed") => vec![PEvent::Error(
            str_at(v, &["error", "message"])
                .unwrap_or("Codex turn failed")
                .into(),
        )],
        _ => vec![],
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Codex's limits for `thread_id` from its own session file (ADR 0014), or `None` when the
/// file or its shape is not what 0.172 writes: never an error.
fn codex_limits(thread_id: &str) -> Option<crate::limits::Limits> {
    let home = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".codex")))?;
    let file = crate::limits::codex_session_file(&home, thread_id)?;
    let limits = read_tail(&file, crate::limits::from_codex_session);
    if limits.is_none() {
        tracing::debug!(file = %file.display(), "no Codex limits in the session file");
    }
    limits
}

/// Runs `find` over the end of `file`, 64 KiB, then 1 MiB, then 16 MiB, stopping at the
/// first hit: a long session is never read whole for one recent line. The first, likely
/// partial, line of a tail is dropped.
fn read_tail<T>(file: &std::path::Path, find: impl Fn(&str) -> Option<T>) -> Option<T> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(file).ok()?;
    let len = f.metadata().ok()?.len();
    for size in [64 << 10, 1 << 20, 16 << 20] {
        let start = len.saturating_sub(size);
        f.seek(SeekFrom::Start(start)).ok()?;
        let mut bytes = Vec::new();
        f.by_ref().take(size).read_to_end(&mut bytes).ok()?;
        let text = String::from_utf8_lossy(&bytes);
        let text = if start > 0 {
            text.split_once('\n').map_or("", |(_, rest)| rest)
        } else {
            &text
        };
        if let Some(found) = find(text) {
            return Some(found);
        }
        if start == 0 {
            return None;
        }
    }
    None
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Runs one turn of participant `id` (a `kind` CLI) and reports through `tx` under that id.
/// Ends with exactly one `Done`.
pub async fn run(
    id: String,
    kind: Kind,
    turn: Turn,
    tx: mpsc::Sender<Event>,
    cancel: CancellationToken,
) {
    // Checked at every launch, not only at startup: a project config added later would
    // otherwise load into a read-only turn.
    if kind == Kind::Codex
        && let Some(path) = codex_project_config(&turn.cwd)
    {
        let why = format!(
            "{} exists; Codex turns are refused (ADR 0004)",
            path.display()
        );
        let _ = tx.send(Event::Agent(id.clone(), PEvent::Error(why))).await;
        let _ = tx
            .send(Event::Agent(id, PEvent::Done(Outcome::Failed)))
            .await;
        return;
    }
    let (cmd, input) = command(kind, &turn);
    run_command(id, kind, cmd, input, tx, cancel).await;
}

/// The runner, separate from `command` so tests can drive it with a fake CLI.
pub async fn run_command(
    id: String,
    kind: Kind,
    mut cmd: Command,
    input: Vec<u8>,
    tx: mpsc::Sender<Event>,
    cancel: CancellationToken,
) {
    let emit = |event| {
        let tx = tx.clone();
        let id = id.clone();
        async move {
            // The loop is gone only when the app is exiting; nothing left to report to.
            let _ = tx.send(Event::Agent(id, event)).await;
        }
    };
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group, so cancelling reaches the tools the CLI started too.
        .process_group(0)
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            emit(PEvent::Error(format!("cannot start {}: {err}", kind.cmd()))).await;
            emit(PEvent::Done(Outcome::Failed)).await;
            return;
        }
    };
    let pgid = child.id();
    // The prompt is written alongside reading the output, not before it: a CLI that fills
    // its output pipe before reading all of a large prompt would otherwise deadlock.
    if let Some(mut stdin) = child.stdin.take() {
        let agent = kind.id();
        tokio::spawn(async move {
            // Dropping stdin afterwards closes it: the prompt is complete.
            if let Err(err) = stdin.write_all(&input).await {
                tracing::warn!(agent, %err, "writing the prompt failed");
            }
        });
    }
    let stderr = child.stderr.take().expect("stderr is piped");
    let stderr_task = tokio::spawn(stderr_tail(stderr));
    let mut lines = BufReader::new(child.stdout.take().expect("stdout is piped")).lines();

    let mut session: Option<String> = None;
    let mut outcome = loop {
        tokio::select! {
            _ = cancel.cancelled() => break Outcome::Interrupted,
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    for event in parse(kind, &line) {
                        if let PEvent::Session(thread) = &event {
                            session = Some(thread.clone());
                        }
                        emit(event).await;
                    }
                }
                Ok(None) => break Outcome::Complete,
                Err(err) => {
                    emit(PEvent::Error(format!("reading {} output: {err}", kind.cmd()))).await;
                    break Outcome::Failed;
                }
            }
        }
    };

    // Output closed is not the end: the CLI may still run. Cancellation stays honoured
    // while waiting for it to exit.
    let status = if outcome == Outcome::Interrupted {
        None
    } else {
        tokio::select! {
            status = child.wait() => Some(status),
            _ = cancel.cancelled() => {
                outcome = Outcome::Interrupted;
                None
            }
        }
    };
    let status = match status {
        Some(status) => status,
        None => {
            signal_group(pgid, libc::SIGTERM);
            if tokio::time::timeout(KILL_GRACE, child.wait())
                .await
                .is_err()
            {
                signal_group(pgid, libc::SIGKILL);
            }
            child.wait().await
        }
    };
    // Tools the CLI started share its group; none may outlive the turn.
    signal_group(pgid, libc::SIGKILL);
    if outcome == Outcome::Complete && !status.as_ref().is_ok_and(|s| s.success()) {
        outcome = Outcome::Failed;
    }
    if outcome == Outcome::Failed {
        let tail = stderr_task.await.unwrap_or_default();
        if !tail.is_empty() {
            emit(PEvent::Error(tail)).await;
        }
    }
    // Codex writes its limits to its own session file, done by now (ADR 0014); a failed
    // turn (e.g. the usage limit itself) still updated them.
    if kind == Kind::Codex
        && let Some(thread) = session
        && let Ok(Some(limits)) = tokio::task::spawn_blocking(move || codex_limits(&thread)).await
    {
        emit(PEvent::Limits(limits)).await;
    }
    emit(PEvent::Done(outcome)).await;
}

/// The last few lines of stderr, read continuously but kept bounded: a noisy CLI must not
/// grow memory without limit when only the tail is ever shown.
async fn stderr_tail(mut stderr: tokio::process::ChildStderr) -> String {
    const KEEP: usize = 4096;
    let mut tail: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(n) = stderr.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        tail.extend_from_slice(&chunk[..n]);
        if tail.len() > KEEP {
            tail.drain(..tail.len() - KEEP);
        }
    }
    let text = String::from_utf8_lossy(&tail);
    let lines: Vec<&str> = text.lines().rev().take(3).collect();
    lines.into_iter().rev().collect::<Vec<_>>().join("\n")
}

fn signal_group(pgid: Option<u32>, signal: libc::c_int) {
    let Some(pgid) = pgid else { return };
    // SAFETY: killpg only sends a signal; an empty or vanished group returns ESRCH, ignored.
    unsafe {
        libc::killpg(pgid as libc::pid_t, signal);
    }
}

/// Startup check of one agent: its CLI version, or why it can't take turns.
pub fn preflight(
    kind: Kind,
    cmd: &str,
    cwd: &std::path::Path,
    outside_repo: bool,
) -> Result<String, String> {
    let output = std::process::Command::new(cmd)
        .arg("--version")
        .output()
        .map_err(|err| format!("`{cmd}` not found: {err}"))?;
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if kind == Kind::Codex {
        let in_repo = std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(cwd)
            .output()
            .is_ok_and(|o| o.status.success());
        if !in_repo && !outside_repo {
            return Err(
                "not inside a git repo: start roundtable in one, or pass --outside-repo".into(),
            );
        }
        if let Some(path) = codex_project_config(cwd) {
            return Err(format!(
                "{} exists; a project Codex config can start MCP servers or run programs \
                 that read-only turns can't exclude (ADR 0004)",
                path.display()
            ));
        }
    }
    Ok(version)
}

/// Any project-level Codex config in `cwd` or a parent. `--ignore-user-config` skips only the
/// user's own config, and a project config can declare MCP servers, `notify` programs and
/// more, so its mere presence fails closed instead of trusting a scan of its keys.
fn codex_project_config(cwd: &std::path::Path) -> Option<PathBuf> {
    let user_config = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".codex")))
        .map(|d| d.join("config.toml"));
    cwd.ancestors()
        .map(|dir| dir.join(".codex").join("config.toml"))
        .filter(|path| Some(path) != user_config.as_ref())
        .find(|path| path.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<PEvent> {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let kind = if name.starts_with("claude") {
            Kind::Claude
        } else {
            Kind::Codex
        };
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .flat_map(|l| parse(kind, l))
            .collect()
    }

    fn text(events: &[PEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                PEvent::TextDelta(t) | PEvent::Message(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn claude_recorded_two_turns() {
        for name in ["claude-2.1.331-turn1.jsonl", "claude-2.1.331-turn2.jsonl"] {
            let events = fixture(name);
            assert_eq!(
                events[0],
                PEvent::Session("3b0307c1-8595-439b-8445-bb007bb56721".into()),
                "{name}: same session on resume"
            );
            assert_eq!(text(&events), "pong", "{name}");
            assert!(
                matches!(events.last(), Some(PEvent::Usage { output, cost_usd: Some(_), .. }) if *output > 0),
                "{name}: {events:?}"
            );
        }
    }

    #[test]
    fn codex_recorded_turn() {
        let events = fixture("codex-0.171.0-turn1.jsonl");
        assert_eq!(
            events[0],
            PEvent::Session("01a20d64-7861-72b3-a272-d782b7eac201".into())
        );
        assert_eq!(
            events[2],
            PEvent::Tool("/bin/zsh -lc 'cat CLAUDE.md'".into())
        );
        assert_eq!(
            text(&events),
            "I will read the ADRs first.1. **ADR 0004: [P1]** example finding."
        );
        assert_eq!(
            events.last(),
            Some(&PEvent::Usage {
                input: 118547,
                output: 1894,
                cost_usd: None,
                estimated: false
            })
        );
    }

    #[test]
    fn claude_rate_limit_events_become_limits() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{"five_hour":{"utilization":0.5,"resetsAt":1795620000,"status":"allowed"}}}}"#;
        let [PEvent::Limits(l)] = &parse(Kind::Claude, line)[..] else {
            panic!("{:?}", parse(Kind::Claude, line))
        };
        assert_eq!(l.five_hour.unwrap().resets_at, 1795620000);
        // The recorded fixtures carry the event without data: nothing comes of it.
        assert!(parse(Kind::Claude, r#"{"type":"rate_limit_event"}"#).is_empty());
    }

    #[test]
    fn the_tail_is_read_in_growing_steps() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("s.jsonl");
        let filler = "x".repeat(100) + "\n";
        std::fs::write(&file, format!("needle\n{}", filler.repeat(2_000))).unwrap();
        let seen = std::cell::RefCell::new(vec![]);
        let found = read_tail(&file, |text| {
            seen.borrow_mut().push(text.len());
            text.lines().any(|l| l == "needle").then_some(())
        });
        assert_eq!(found, Some(()));
        let seen = seen.into_inner();
        assert!(seen[0] < 70_000 && seen.len() == 2, "{seen:?}");
        assert_eq!(read_tail(&file, |_| None::<()>), None);
    }

    #[test]
    fn unknown_and_broken_lines_are_skipped() {
        assert!(parse(Kind::Claude, r#"{"type":"brand_new_event"}"#).is_empty());
        assert!(parse(Kind::Codex, "not json").is_empty());
    }

    #[test]
    fn any_project_codex_config_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(codex_project_config(&nested), None);
        std::fs::create_dir_all(dir.path().join("a/.codex")).unwrap();
        // No MCP key at all: still refused.
        std::fs::write(
            dir.path().join("a/.codex/config.toml"),
            "notify = [\"x\"]\n",
        )
        .unwrap();
        assert_eq!(
            codex_project_config(&nested),
            Some(dir.path().join("a/.codex/config.toml"))
        );
    }

    #[test]
    fn codex_errors_surface() {
        assert_eq!(
            parse(Kind::Codex, r#"{"type":"error","message":"usage limit"}"#),
            [PEvent::Error("usage limit".into())]
        );
    }

    #[test]
    fn commands_are_read_only_and_resume() {
        let turn = Turn {
            cmd: "claude".into(),
            prompt: "hi".into(),
            session: Some("S1".into()),
            cwd: ".".into(),
            outside_repo: false,
            model: Some("m1".into()),
            writable: false,
            permission_mode: Some("acceptEdits".into()),
            ask: None,
        };
        let args = |kind| {
            let (cmd, _) = command(kind, &turn);
            cmd.as_std()
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let claude = args(Kind::Claude);
        assert!(claude.contains("--permission-mode plan --strict-mcp-config"));
        assert!(claude.ends_with("--resume S1 --model m1"));
        let codex = args(Kind::Codex);
        assert!(codex.starts_with("exec resume S1 --json --ignore-user-config"));
        assert!(codex.contains(r#"sandbox_mode="read-only""#));
        assert!(!codex.contains("--skip-git-repo-check"));
        assert!(codex.ends_with("-m m1 -"));
        assert!(
            !claude.contains("acceptEdits"),
            "a read-only turn ignores the writer's mode"
        );
    }

    #[test]
    fn the_writer_turn_drops_read_only_flags_and_uses_its_mode() {
        let mut turn = Turn {
            cmd: "claude".into(),
            prompt: "hi".into(),
            session: None,
            cwd: ".".into(),
            outside_repo: false,
            model: None,
            writable: true,
            permission_mode: Some("acceptEdits".into()),
            ask: None,
        };
        let args = |kind, turn: &Turn| {
            let (cmd, _) = command(kind, turn);
            cmd.as_std()
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let claude = args(Kind::Claude, &turn);
        assert!(claude.ends_with("--verbose --permission-mode acceptEdits"));
        assert!(!claude.contains("plan") && !claude.contains("--setting-sources"));
        turn.permission_mode = Some("workspace-write".into());
        let codex = args(Kind::Codex, &turn);
        assert_eq!(codex, r#"exec --json -c sandbox_mode="workspace-write" -"#);
        turn.permission_mode = None;
        assert_eq!(
            args(Kind::Codex, &turn),
            r#"exec --json -c sandbox_mode="workspace-write" -"#,
            "unset never falls back to the user's own config"
        );
        assert!(args(Kind::Claude, &turn).ends_with("--permission-mode default"));
    }

    #[test]
    fn a_caller_gets_only_the_roundtable_server_and_only_the_ask_tool() {
        let turn = Turn {
            cmd: "claude".into(),
            prompt: "hi".into(),
            session: None,
            cwd: ".".into(),
            outside_repo: false,
            model: None,
            writable: false,
            permission_mode: None,
            ask: Some(AskBridge {
                exe: "/bin/roundtable".into(),
                socket: "/data/ask.sock".into(),
                caller: "claude".into(),
                token: "T1".into(),
                config_file: "/data/mcp/turn.json".into(),
            }),
        };
        let (cmd, _) = command(Kind::Claude, &turn);
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let config = &args[args.iter().position(|a| a == "--mcp-config").unwrap() + 1];
        assert_eq!(
            config, "/data/mcp/turn.json",
            "a path, never the token itself"
        );
        assert!(
            !args.iter().any(|a| a.contains("T1")),
            "no token on the command line"
        );
        let config: Value =
            serde_json::from_str(&turn.ask.as_ref().unwrap().claude_config()).unwrap();
        let servers = config["mcpServers"].as_object().unwrap();
        assert_eq!(servers.keys().collect::<Vec<_>>(), ["roundtable"]);
        assert_eq!(servers["roundtable"]["args"][0], "mcp");
        assert_eq!(servers["roundtable"]["env"][crate::mcp::ENV_TOKEN], "T1");
        assert!(
            args.contains(&"--strict-mcp-config".into()),
            "still read-only isolation"
        );
        let allowed = &args[args.iter().position(|a| a == "--allowedTools").unwrap() + 1];
        assert_eq!(allowed, "mcp__roundtable__ask");
    }

    async fn run_fake(script: &str, cancel: CancellationToken) -> Vec<PEvent> {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        let (tx, mut rx) = mpsc::channel(64);
        tokio::spawn(run_command(
            "codex".into(),
            Kind::Codex,
            cmd,
            b"prompt".to_vec(),
            tx,
            cancel,
        ));
        let mut events = vec![];
        while let Some(Event::Agent(_, event)) = rx.recv().await {
            let done = matches!(event, PEvent::Done(_));
            events.push(event);
            if done {
                break;
            }
        }
        events
    }

    #[tokio::test]
    async fn runner_reads_the_prompt_from_stdin_and_streams_events() {
        // The fake CLI echoes its stdin back as an agent message.
        let script = r#"read p; printf '{"type":"item.completed","item":{"type":"agent_message","text":"got %s"}}\n' "$p""#;
        let events = run_fake(script, CancellationToken::new()).await;
        assert_eq!(
            events,
            [
                PEvent::Message("got prompt".into()),
                PEvent::Done(Outcome::Complete)
            ]
        );
    }

    #[tokio::test]
    async fn runner_reports_failure_with_stderr() {
        let events = run_fake("echo boom >&2; exit 3", CancellationToken::new()).await;
        assert_eq!(
            events,
            [PEvent::Error("boom".into()), PEvent::Done(Outcome::Failed)]
        );
    }

    #[tokio::test]
    async fn a_large_prompt_does_not_deadlock_a_chatty_cli() {
        // Writes 1 MiB of output before reading any input: with the prompt written first,
        // both sides would block on full pipes.
        let script = r#"head -c 1048576 /dev/zero | tr '\0' 'x'; echo; cat > /dev/null; printf '{"type":"item.completed","item":{"type":"agent_message","text":"ok"}}\n'"#;
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        let (tx, mut rx) = mpsc::channel(64);
        let prompt = vec![b'p'; 4 * 1024 * 1024];
        tokio::spawn(run_command(
            "codex".into(),
            Kind::Codex,
            cmd,
            prompt,
            tx,
            CancellationToken::new(),
        ));
        let events = tokio::time::timeout(Duration::from_secs(20), async {
            let mut events = vec![];
            while let Some(Event::Agent(_, event)) = rx.recv().await {
                let done = matches!(event, PEvent::Done(_));
                events.push(event);
                if done {
                    break;
                }
            }
            events
        })
        .await
        .expect("deadlocked");
        assert_eq!(
            events,
            [
                PEvent::Message("ok".into()),
                PEvent::Done(Outcome::Complete)
            ]
        );
    }

    #[tokio::test]
    async fn only_the_tail_of_noisy_stderr_is_kept() {
        let events = run_fake(
            "i=0; while [ $i -lt 20000 ]; do echo noise $i >&2; i=$((i+1)); done; echo last >&2; exit 1",
            CancellationToken::new(),
        )
        .await;
        assert_eq!(
            events,
            [
                PEvent::Error("noise 19998\nnoise 19999\nlast".into()),
                PEvent::Done(Outcome::Failed)
            ]
        );
    }

    #[tokio::test]
    async fn cancel_kills_the_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("tool.pid");
        // A "tool" in the background, as an agent's shell command would be.
        let script = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            stop.cancel();
        });
        let started = std::time::Instant::now();
        let events = run_fake(&script, cancel).await;
        assert_eq!(events, [PEvent::Done(Outcome::Interrupted)]);
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid: libc::pid_t = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The killed tool may linger briefly as a zombie until its new parent reaps it.
        let gone = (0..100)
            .any(|_| !running(pid) || (std::thread::sleep(Duration::from_millis(20)), false).1);
        assert!(gone, "the background tool outlived the cancelled turn");
    }

    /// Alive and not a zombie (`/proc` on Linux; elsewhere existence only).
    fn running(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 only checks whether the process exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return false;
        }
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => !stat.contains(") Z "),
            Err(_) => true,
        }
    }
}
