//! Free and side models over the OpenAI-compatible chat API (ADR 0005): one adapter for
//! Ollama, LM Studio, OpenRouter, Groq, Gemini and anything else that speaks it. These
//! participants only chat: no tools, no files.

use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::{Outcome, PEvent};
use crate::app::{Event, Secret};

const RETRY_BACKOFF: Duration = Duration::from_secs(2);
/// Defaults when `models.toml` doesn't say (ADR 0005).
pub const DEFAULT_CONTEXT_WINDOW: u64 = 8_192;
pub const DEFAULT_MAX_OUTPUT: u64 = 4_096;
pub const DEFAULT_CONNECT_TIMEOUT_S: u64 = 10;
pub const DEFAULT_IDLE_TIMEOUT_S: u64 = 120;
/// Shorter keys are refused: every key must be redactable without hitting ordinary words.
const MIN_KEY_LEN: usize = 8;

/// One request: the model's own history (oldest first, as prompt/reply pairs) and the new
/// prompt. The runner resolves the key and fits the messages to the window.
#[derive(Debug, Clone, PartialEq)]
pub struct Chat {
    pub base_url: String,
    pub model: String,
    /// A reference (`env:`/`keyring:`), resolved by the runner off the UI loop.
    pub key_ref: Option<String>,
    pub system: String,
    pub history: Vec<(String, String)>,
    pub prompt: String,
    pub context_window: u64,
    pub max_output: u64,
    pub connect_timeout: Duration,
    /// No byte from the provider for this long (headers, body or stream) ends the turn.
    pub idle_timeout: Duration,
}

/// Tokens of `text`, estimated: chars/3 plus one per non-ASCII char, so code, CJK and
/// emoji are not undercounted (ADR 0005). Deliberately pessimistic.
pub fn estimate(text: &str) -> u64 {
    let chars = text.chars().count() as u64;
    let non_ascii = text.chars().filter(|c| !c.is_ascii()).count() as u64;
    chars.div_ceil(3) + non_ascii
}

/// The messages to send, fitted to the window (ADR 0005): output space reserved first, the
/// system hint and the new prompt always kept, whole older turns dropped oldest first.
/// Returns the messages and how many history turns made it in. `Err` when even the prompt
/// alone doesn't fit, with the sizes.
pub fn fit(chat: &Chat, history: &[(String, String)]) -> Result<(Vec<Value>, usize), String> {
    // A few tokens per message for the role and separators.
    let cost = |text: &str| estimate(text) + 4;
    let budget = chat.context_window.saturating_sub(chat.max_output);
    let fixed = cost(&chat.system) + cost(&chat.prompt);
    if fixed > budget {
        return Err(format!(
            "the prompt needs ~{fixed} tokens but only {budget} fit (window {}, {} kept for the reply)",
            chat.context_window, chat.max_output
        ));
    }
    let mut room = budget - fixed;
    let mut kept = vec![];
    for (prompt, reply) in history.iter().rev() {
        let turn = cost(prompt) + cost(reply);
        if turn > room {
            break; // whole turns only, oldest dropped first
        }
        room -= turn;
        kept.push((prompt, reply));
    }
    let turns = kept.len();
    let mut messages = vec![json!({"role": "system", "content": chat.system})];
    for (prompt, reply) in kept.into_iter().rev() {
        messages.push(json!({"role": "user", "content": prompt}));
        messages.push(json!({"role": "assistant", "content": reply}));
    }
    messages.push(json!({"role": "user", "content": chat.prompt}));
    Ok((messages, turns))
}

/// Hides a key in streamed text before it is shown anywhere. Text is held back by up to
/// `key.len() - 1` bytes, so a key split across chunks is never shown in pieces.
struct Redactor {
    key: Option<String>,
    pending: String,
}

impl Redactor {
    fn new(key: Option<String>) -> Self {
        Self {
            key: key.filter(|k| !k.is_empty()),
            pending: String::new(),
        }
    }

    /// Text safe to show now.
    fn push(&mut self, text: &str) -> String {
        let Some(key) = &self.key else {
            return text.to_string();
        };
        self.pending.push_str(text);
        self.pending = self.pending.replace(key.as_str(), "[redacted]");
        // Keep back what could still turn out to be the start of the key.
        let keep = key.len().saturating_sub(1).min(self.pending.len());
        let mut cut = self.pending.len() - keep;
        while !self.pending.is_char_boundary(cut) {
            cut -= 1;
        }
        self.pending.drain(..cut).collect()
    }

    fn finish(&mut self) -> String {
        std::mem::take(&mut self.pending)
    }

    fn clean(&self, text: &str) -> String {
        match &self.key {
            Some(key) => text.replace(key.as_str(), "[redacted]"),
            None => text.to_string(),
        }
    }
}

/// What one HTTP attempt ended with.
enum Attempt {
    Done(Outcome),
    /// The provider said the request is too long: retry once with less history.
    TooLong(String),
    /// A 5xx: retry once after a pause.
    ServerError(String),
}

/// Runs one chat turn for participant `id` and reports through `tx` under that id. Ends
/// with exactly one `Done`.
pub async fn run(id: String, chat: Chat, tx: mpsc::Sender<Event>, cancel: CancellationToken) {
    let emit = |event| {
        let tx = tx.clone();
        let id = id.clone();
        async move {
            let _ = tx.send(Event::Agent(id, event)).await;
        }
    };
    // The OS keychain may block (an unlock prompt): never on the UI loop.
    let key = match chat.key_ref.clone() {
        None => None,
        Some(reference) => {
            let resolved = tokio::task::spawn_blocking({
                let reference = reference.clone();
                move || resolve_key(&reference)
            })
            .await
            .unwrap_or_else(|e| Err(format!("key lookup: {e}")));
            match resolved {
                Ok(key) => {
                    // To the loop first, so the transcript redacts it before any reply.
                    let _ = tx
                        .send(Event::KeyResolved {
                            reference,
                            value: Secret(key.clone()),
                        })
                        .await;
                    Some(key)
                }
                Err(why) => {
                    emit(PEvent::Error(why)).await;
                    emit(PEvent::Done(Outcome::Failed)).await;
                    return;
                }
            }
        }
    };
    let client = match reqwest::Client::builder()
        .connect_timeout(chat.connect_timeout)
        // Covers waiting for headers and every read of the body, errors included.
        .read_timeout(chat.idle_timeout)
        // A redirect would carry the prompt and history to another origin.
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            emit(PEvent::Error(format!("HTTP client: {err}"))).await;
            emit(PEvent::Done(Outcome::Failed)).await;
            return;
        }
    };
    let mut redactor = Redactor::new(key.clone());
    let mut history = chat.history.clone();
    let mut trimmed = false;
    let mut retried_5xx = false;
    let outcome = loop {
        let (messages, turns) = match fit(&chat, &history) {
            Ok(fitted) => fitted,
            Err(why) => {
                emit(PEvent::Error(why)).await;
                break Outcome::Failed;
            }
        };
        let attempt = tokio::select! {
            _ = cancel.cancelled() => Attempt::Done(Outcome::Interrupted),
            attempt = request(&client, &chat, key.as_deref(), messages, &mut redactor, &emit) => attempt,
        };
        match attempt {
            Attempt::Done(outcome) => break outcome,
            Attempt::TooLong(_) if !trimmed && turns > 0 => {
                // Drop the oldest quarter (at least one) of the turns actually sent.
                let sent_from = history.len() - turns;
                let drop = turns.div_ceil(4);
                history.drain(..sent_from + drop);
                trimmed = true;
                tracing::info!(id, dropped = drop, "context too long, retrying");
            }
            Attempt::TooLong(why) => {
                emit(PEvent::Error(format!(
                    "the model refused the request as too long: {why}"
                )))
                .await;
                break Outcome::Failed;
            }
            Attempt::ServerError(_) if !retried_5xx => {
                retried_5xx = true;
                // Provider text is not logged: it may echo a key.
                tracing::info!(id, "server error, retrying once");
                tokio::select! {
                    _ = cancel.cancelled() => break Outcome::Interrupted,
                    _ = tokio::time::sleep(RETRY_BACKOFF) => {}
                }
            }
            Attempt::ServerError(why) => {
                emit(PEvent::Error(why)).await;
                break Outcome::Failed;
            }
        }
    };
    emit(PEvent::Done(outcome)).await;
}

async fn request<F, Fut>(
    client: &reqwest::Client,
    chat: &Chat,
    key: Option<&str>,
    messages: Vec<Value>,
    redactor: &mut Redactor,
    emit: &F,
) -> Attempt
where
    F: Fn(PEvent) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let url = format!("{}/chat/completions", chat.base_url.trim_end_matches('/'));
    let sent_estimate: u64 = messages
        .iter()
        .filter_map(|m| m["content"].as_str())
        .map(estimate)
        .sum();
    let body = json!({
        "model": chat.model,
        "messages": messages,
        "stream": true,
        "max_tokens": chat.max_output,
        "stream_options": {"include_usage": true},
    });
    let mut builder = client.post(&url).json(&body);
    if let Some(key) = key {
        builder = builder.bearer_auth(key);
    }
    let response = match builder.send().await {
        Ok(response) => response,
        Err(err) => {
            emit(PEvent::Error(format!(
                "{}: {}",
                chat.base_url,
                without_url(&err)
            )))
            .await;
            return Attempt::Done(Outcome::Failed);
        }
    };
    let status = response.status();
    if status.is_redirection() {
        emit(PEvent::Error(format!(
            "{}: redirected ({status}); redirects are not followed, fix base_url",
            chat.base_url
        )))
        .await;
        return Attempt::Done(Outcome::Failed);
    }
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        let message = provider_message(&text).unwrap_or_else(|| text.chars().take(300).collect());
        let message = redactor.clean(&message);
        let why = format!("{} {status}: {message}", chat.base_url);
        if status.is_server_error() {
            return Attempt::ServerError(why);
        }
        let lower = message.to_lowercase();
        if status.as_u16() == 400 && (lower.contains("context") || lower.contains("too long")) {
            return Attempt::TooLong(message);
        }
        // 429 and other 4xx: shown, never retried (free tiers, bad requests).
        emit(PEvent::Error(why)).await;
        return Attempt::Done(Outcome::Failed);
    }

    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = vec![];
    let mut usage = None;
    let mut reply_chars = String::new();
    // A clean end of the connection is not proof the model finished: only `[DONE]` or a
    // `finish_reason` is.
    let mut completed = false;
    let mut finished = false;
    while !finished {
        let chunk = match stream.next().await {
            None => break,
            Some(Err(err)) => {
                emit(PEvent::Error(format!(
                    "{}: {}",
                    chat.base_url,
                    without_url(&err)
                )))
                .await;
                return Attempt::Done(Outcome::Failed);
            }
            Some(Ok(bytes)) => bytes,
        };
        buffer.extend_from_slice(&chunk);
        while let Some(end) = buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue; // comments, `event:` lines, blank separators
            };
            let data = data.trim();
            if data == "[DONE]" {
                completed = true;
                finished = true;
                break;
            }
            let Ok(event) = serde_json::from_str::<Value>(data) else {
                tracing::debug!(bytes = data.len(), "unparsable SSE data, skipped");
                continue;
            };
            if let Some(text) = event
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
                && !text.is_empty()
            {
                reply_chars.push_str(text);
                let shown = redactor.push(text);
                if !shown.is_empty() {
                    emit(PEvent::TextDelta(shown)).await;
                }
            }
            if event
                .pointer("/choices/0/finish_reason")
                .is_some_and(|r| !r.is_null())
            {
                completed = true;
            }
            if let Some(u) = event.get("usage").filter(|u| !u.is_null()) {
                usage = Some((
                    u["prompt_tokens"].as_u64().unwrap_or(0),
                    u["completion_tokens"].as_u64().unwrap_or(0),
                ));
            }
            if let Some(message) = event.pointer("/error/message").and_then(Value::as_str) {
                let message = redactor.clean(message);
                emit(PEvent::Error(format!("{}: {message}", chat.base_url))).await;
                return Attempt::Done(Outcome::Failed);
            }
        }
    }
    let rest = redactor.finish();
    if !rest.is_empty() {
        emit(PEvent::TextDelta(rest)).await;
    }
    if !completed {
        emit(PEvent::Error(format!(
            "{}: the reply stream ended before the model finished",
            chat.base_url
        )))
        .await;
        return Attempt::Done(Outcome::Failed);
    }
    let (input, output, estimated) = match usage {
        Some((input, output)) => (input, output, false),
        // The provider didn't say: estimate over what was really sent, and say so.
        None => (sent_estimate, estimate(&reply_chars), true),
    };
    emit(PEvent::Usage {
        input,
        output,
        cost_usd: None,
        estimated,
    })
    .await;
    Attempt::Done(Outcome::Complete)
}

/// A provider's own error message from an OpenAI-style error body.
fn provider_message(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    v.pointer("/error/message")
        .or_else(|| v.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// reqwest errors print the URL, query included; keep the message, drop the URL.
fn without_url(err: &reqwest::Error) -> String {
    let mut err = err.to_string();
    if let Some(i) = err.find(" for url (") {
        err.truncate(i);
    }
    err
}

/// Resolves a key reference from `models.toml` (ADR 0011): `env:<VAR>` or
/// `keyring:<name>` (service `roundtable`). Done per request, so a changed key is used at
/// once. The error names the source, never a value. Blocking: call off the UI loop.
pub fn resolve_key(reference: &str) -> Result<String, String> {
    let key = if let Some(var) = reference.strip_prefix("env:") {
        std::env::var(var).map_err(|_| format!("environment variable {var} is not set"))?
    } else if let Some(name) = reference.strip_prefix("keyring:") {
        keyring::Entry::new("roundtable", name)
            .and_then(|entry| entry.get_password())
            .map_err(|e| format!("keychain entry roundtable/{name}: {e}"))?
    } else {
        return Err(format!("`{reference}` is not a key reference"));
    };
    if key.len() < MIN_KEY_LEN {
        return Err(format!(
            "the key from {reference} is shorter than {MIN_KEY_LEN} characters; refused so it \
             can always be redacted"
        ));
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn chat(history: Vec<(&str, &str)>, window: u64) -> Chat {
        Chat {
            base_url: String::new(),
            model: "m".into(),
            key_ref: None,
            system: "sys".into(),
            history: history
                .into_iter()
                .map(|(a, b)| (a.into(), b.into()))
                .collect(),
            prompt: "now".into(),
            context_window: window,
            max_output: 10,
            connect_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn estimates_are_pessimistic_for_non_ascii() {
        assert_eq!(estimate("abcdef"), 2);
        assert_eq!(estimate("日本語"), 1 + 3);
        assert_eq!(estimate(""), 0);
    }

    #[test]
    fn fitting_keeps_system_and_prompt_and_drops_oldest_whole_turns() {
        let c = chat(
            vec![("old q", "old a"), ("new q", "new a")],
            10 + 5 + 5 + 7 + 7,
        );
        let (messages, turns) = fit(&c, &c.history).unwrap();
        let texts: Vec<&str> = messages
            .iter()
            .map(|m| m["content"].as_str().unwrap())
            .collect();
        assert_eq!(
            texts,
            ["sys", "new q", "new a", "now"],
            "only the newest turn fits"
        );
        assert_eq!(turns, 1);
        let roomy = chat(vec![("old q", "old a"), ("new q", "new a")], 1000);
        assert_eq!(fit(&roomy, &roomy.history).unwrap().0.len(), 6);
        let tiny = chat(vec![], 12);
        assert!(fit(&tiny, &[]).unwrap_err().contains("only 2 fit"));
    }

    #[test]
    fn keys_resolve_from_env_and_never_from_text() {
        // SAFETY: variables only this test reads.
        unsafe {
            std::env::set_var("ROUNDTABLE_TEST_KEY", "sekret-12345");
            std::env::set_var("ROUNDTABLE_SHORT_KEY", "abc");
        }
        assert_eq!(
            resolve_key("env:ROUNDTABLE_TEST_KEY").unwrap(),
            "sekret-12345"
        );
        assert!(
            resolve_key("env:ROUNDTABLE_NO_SUCH_VAR")
                .unwrap_err()
                .contains("not set")
        );
        assert!(
            resolve_key("env:ROUNDTABLE_SHORT_KEY")
                .unwrap_err()
                .contains("shorter")
        );
        assert!(resolve_key("sk-literal").is_err());
    }

    #[test]
    fn a_key_split_across_chunks_is_never_shown() {
        let mut r = Redactor::new(Some("SECRETKEY".into()));
        let mut shown = String::new();
        for chunk in ["your key is SEC", "RETK", "EY, bye"] {
            shown.push_str(&r.push(chunk));
            assert!(!shown.contains("SEC"), "a piece leaked: {shown}");
        }
        shown.push_str(&r.finish());
        assert_eq!(shown, "your key is [redacted], bye");
        let mut none = Redactor::new(None);
        assert_eq!(none.push("abc"), "abc");
    }

    /// A fake provider: answers each connection with the next canned response and hands
    /// back the request bodies and authorization headers it got.
    async fn fake(
        responses: Vec<String>,
    ) -> (String, tokio::task::JoinHandle<Vec<(Value, String)>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let mut got = vec![];
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![];
                let mut buf = [0u8; 65536];
                let (body, auth) = loop {
                    let n = socket.read(&mut buf).await.unwrap();
                    request.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&request).to_string();
                    if let Some(split) = text.find("\r\n\r\n") {
                        let header = |name: &str| {
                            text.lines().find_map(|l| {
                                let lower = l.to_lowercase();
                                lower
                                    .strip_prefix(&format!("{name}:"))
                                    .map(|v| v.trim().to_string())
                            })
                        };
                        let length: usize = header("content-length")
                            .map(|v| v.parse().unwrap())
                            .unwrap_or(0);
                        if request.len() >= split + 4 + length {
                            let body =
                                serde_json::from_slice(&request[split + 4..split + 4 + length])
                                    .unwrap();
                            break (body, header("authorization").unwrap_or_default());
                        }
                    }
                };
                got.push((body, auth));
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
            got
        });
        (url, handle)
    }

    fn sse(lines: &[&str]) -> String {
        let body: String = lines.iter().map(|l| format!("data: {l}\n\n")).collect();
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
        )
    }

    fn status(code: u16, message: &str) -> String {
        let body = json!({"error": {"message": message}}).to_string();
        format!(
            "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    async fn run_chat(c: Chat) -> Vec<PEvent> {
        let (tx, mut rx) = mpsc::channel(64);
        tokio::spawn(run("llama".into(), c, tx, CancellationToken::new()));
        let mut events = vec![];
        while let Some(event) = rx.recv().await {
            let Event::Agent(_, event) = event else {
                continue; // KeyResolved goes to the loop, not the chat
            };
            let done = matches!(event, PEvent::Done(_));
            events.push(event);
            if done {
                break;
            }
        }
        events
    }

    #[tokio::test]
    async fn streams_deltas_and_reported_usage() {
        let (url, server) = fake(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"po"}}]}"#,
            r#"{"choices":[{"delta":{"content":"ng"},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":2}}"#,
            "[DONE]",
        ])])
        .await;
        // SAFETY: a variable only this test reads.
        unsafe { std::env::set_var("ROUNDTABLE_STREAM_KEY", "sk-stream-0001") };
        let mut c = chat(vec![("q", "a")], 1000);
        c.base_url = url;
        c.key_ref = Some("env:ROUNDTABLE_STREAM_KEY".into());
        let events = run_chat(c).await;
        assert_eq!(
            events,
            [
                // With a key, text is held back until it can't be part of the key.
                PEvent::TextDelta("pong".into()),
                PEvent::Usage {
                    input: 12,
                    output: 2,
                    cost_usd: None,
                    estimated: false
                },
                PEvent::Done(Outcome::Complete)
            ]
        );
        let (body, auth) = &server.await.unwrap()[0];
        assert_eq!(auth, "bearer sk-stream-0001", "lowercased by the fake");
        assert_eq!(
            body["messages"].as_array().unwrap().len(),
            4,
            "system, q, a, prompt"
        );
    }

    #[tokio::test]
    async fn a_stream_cut_short_is_a_failure_not_a_reply() {
        let (url, _server) = fake(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"half"}}]}"#,
        ])])
        .await;
        let mut c = chat(vec![], 1000);
        c.base_url = url;
        let events = run_chat(c).await;
        assert!(matches!(&events[1], PEvent::Error(e) if e.contains("ended before")));
        assert_eq!(events.last(), Some(&PEvent::Done(Outcome::Failed)));
    }

    #[tokio::test]
    async fn a_429_is_shown_and_not_retried() {
        let (url, server) = fake(vec![status(429, "rate limited, try later")]).await;
        let mut c = chat(vec![], 1000);
        c.base_url = url;
        let events = run_chat(c).await;
        assert!(
            matches!(&events[0], PEvent::Error(e) if e.contains("429") && e.contains("rate limited"))
        );
        assert_eq!(events.last(), Some(&PEvent::Done(Outcome::Failed)));
        assert_eq!(server.await.unwrap().len(), 1, "one request only");
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let redirect = "HTTP/1.1 307 Temporary Redirect\r\nlocation: http://127.0.0.1:9/elsewhere\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string();
        let (url, server) = fake(vec![redirect]).await;
        let mut c = chat(vec![("private q", "private a")], 1000);
        c.base_url = url;
        let events = run_chat(c).await;
        assert!(matches!(&events[0], PEvent::Error(e) if e.contains("redirects are not followed")));
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_5xx_is_retried_once_and_usage_is_estimated_over_what_was_sent() {
        let (url, server) = fake(vec![
            status(503, "overloaded"),
            sse(&[r#"{"choices":[{"delta":{"content":"ok"}}]}"#, "[DONE]"]),
        ])
        .await;
        let mut c = chat(
            vec![("a long earlier question here", "and its long answer")],
            1000,
        );
        c.base_url = url;
        let events = run_chat(c).await;
        assert_eq!(events[0], PEvent::TextDelta("ok".into()));
        let PEvent::Usage {
            input, estimated, ..
        } = events[1]
        else {
            panic!("{events:?}")
        };
        assert!(estimated);
        assert!(
            input > estimate("sys") + estimate("now"),
            "history counted: {input}"
        );
        assert_eq!(server.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn too_long_retries_once_with_a_quarter_less_of_what_was_sent() {
        let (url, server) = fake(vec![
            status(400, "This model's maximum context length is 4096 tokens"),
            sse(&[r#"{"choices":[{"delta":{"content":"ok"}}]}"#, "[DONE]"]),
        ])
        .await;
        let history = (0..8)
            .map(|i| (format!("q{i}"), format!("a{i}")))
            .collect::<Vec<_>>();
        let mut c = chat(vec![], 100_000);
        c.history = history;
        c.base_url = url;
        let events = run_chat(c).await;
        assert_eq!(events.last(), Some(&PEvent::Done(Outcome::Complete)));
        let got = server.await.unwrap();
        let count = |b: &Value| b["messages"].as_array().unwrap().len();
        assert_eq!(
            (count(&got[0].0), count(&got[1].0)),
            (2 + 16, 2 + 12),
            "8 turns, then 6"
        );
    }

    #[tokio::test]
    async fn the_retry_shrinks_even_when_fitting_already_dropped_turns() {
        let (url, server) = fake(vec![
            status(400, "context too long"),
            sse(&[r#"{"choices":[{"delta":{"content":"ok"}}]}"#, "[DONE]"]),
        ])
        .await;
        // Ten turns, but the window only fits the newest few.
        let history = (0..10)
            .map(|i| (format!("question {i}"), format!("answer {i}")))
            .collect::<Vec<_>>();
        let mut c = chat(vec![], 10 + 5 + 5 + 4 * 16);
        c.history = history;
        c.base_url = url;
        run_chat(c).await;
        let got = server.await.unwrap();
        let count = |b: &Value| b["messages"].as_array().unwrap().len();
        assert!(count(&got[1].0) < count(&got[0].0), "the retry sends less");
    }

    #[tokio::test]
    async fn an_echoed_key_is_redacted_in_errors() {
        let (url, _server) = fake(vec![status(401, "bad key sk-echo-0000001")]).await;
        // SAFETY: a variable only this test reads.
        unsafe { std::env::set_var("ROUNDTABLE_ECHO_KEY", "sk-echo-0000001") };
        let mut c = chat(vec![], 1000);
        c.base_url = url;
        c.key_ref = Some("env:ROUNDTABLE_ECHO_KEY".into());
        let events = run_chat(c).await;
        let PEvent::Error(e) = &events[0] else {
            panic!("{events:?}")
        };
        assert!(e.contains("[redacted]") && !e.contains("sk-echo"), "{e}");
    }
}
