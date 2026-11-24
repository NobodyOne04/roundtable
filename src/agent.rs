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
    },
    Error(String),
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
}

/// The command for a turn, plus what goes on its stdin. The prompt always travels on stdin:
/// no quoting issues, no length limit, not visible in `ps`.
// shortcut: every turn is read-only; ADR 0011 brings the `writer` that may edit.
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
                // Read-only, and without the user's MCP servers, hooks and plugins, which
                // could write despite plan mode.
                "--permission-mode",
                "plan",
                "--strict-mcp-config",
                "--mcp-config",
                r#"{"mcpServers":{}}"#,
                "--setting-sources",
                "",
            ]);
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
            // `--ignore-user-config` drops the user's MCP servers (auth still works); the
            // sandbox is set through `-c` because `exec resume` has no `-s`.
            cmd.args([
                "--json",
                "--ignore-user-config",
                "-c",
                r#"sandbox_mode="read-only""#,
            ]);
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
        tracing::debug!(agent = kind.id(), line, "not JSON, skipped");
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
        (Some("result"), _) => {
            let mut out = vec![PEvent::Usage {
                input: u64_at(v, &["usage", "input_tokens"])
                    + u64_at(v, &["usage", "cache_read_input_tokens"]),
                output: u64_at(v, &["usage", "output_tokens"]),
                cost_usd: v.get("total_cost_usd").and_then(Value::as_f64),
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
    if let Some(mut stdin) = child.stdin.take() {
        // Dropping stdin afterwards closes it, which tells the CLI the prompt is complete.
        if let Err(err) = stdin.write_all(&input).await {
            tracing::warn!(agent = kind.id(), %err, "writing the prompt failed");
        }
    }
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let stderr_task = tokio::spawn(async move {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        text
    });
    let mut lines = BufReader::new(child.stdout.take().expect("stdout is piped")).lines();

    let mut outcome = loop {
        tokio::select! {
            _ = cancel.cancelled() => break Outcome::Interrupted,
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    for event in parse(kind, &line) {
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

    if outcome == Outcome::Interrupted {
        signal_group(pgid, libc::SIGTERM);
        if tokio::time::timeout(KILL_GRACE, child.wait())
            .await
            .is_err()
        {
            signal_group(pgid, libc::SIGKILL);
        }
    }
    let status = child.wait().await;
    // Tools the CLI started share its group; none may outlive the turn.
    signal_group(pgid, libc::SIGKILL);
    if outcome == Outcome::Complete && !status.as_ref().is_ok_and(|s| s.success()) {
        outcome = Outcome::Failed;
    }
    if outcome == Outcome::Failed {
        let stderr = stderr_task.await.unwrap_or_default();
        let tail: Vec<&str> = stderr.lines().rev().take(3).collect();
        if !tail.is_empty() {
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            emit(PEvent::Error(tail.join("\n"))).await;
        }
    }
    emit(PEvent::Done(outcome)).await;
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
                cost_usd: None
            })
        );
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
