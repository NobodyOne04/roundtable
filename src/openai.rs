//! Free and side models over the OpenAI-compatible chat API (ADR 0005): one adapter for
//! Ollama, LM Studio, OpenRouter, Groq, Gemini and anything else that speaks it. These
//! participants only chat: no tools, no files.

use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::{Outcome, PEvent};
use crate::app::Event;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// No byte from the provider for this long ends the turn.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const RETRY_BACKOFF: Duration = Duration::from_secs(2);
/// Defaults when `models.toml` doesn't say.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 8_192;
pub const DEFAULT_MAX_OUTPUT: u64 = 4_096;

/// One request: the model's own history (oldest first, as prompt/reply pairs) and the new
/// prompt. The runner fits them to the window.
#[derive(Debug, Clone, PartialEq)]
pub struct Chat {
    pub base_url: String,
    pub model: String,
    /// The resolved key (never a reference, never logged).
    pub key: Option<String>,
    pub system: String,
    pub history: Vec<(String, String)>,
    pub prompt: String,
    pub context_window: u64,
    pub max_output: u64,
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
/// `Err` when even the prompt alone doesn't fit, with the sizes.
pub fn fit(chat: &Chat, history: &[(String, String)]) -> Result<Vec<Value>, String> {
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
    let mut messages = vec![json!({"role": "system", "content": chat.system})];
    for (prompt, reply) in kept.into_iter().rev() {
        messages.push(json!({"role": "user", "content": prompt}));
        messages.push(json!({"role": "assistant", "content": reply}));
    }
    messages.push(json!({"role": "user", "content": chat.prompt}));
    Ok(messages)
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
    let client = match reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            emit(PEvent::Error(format!("HTTP client: {err}"))).await;
            emit(PEvent::Done(Outcome::Failed)).await;
            return;
        }
    };
    let mut history = chat.history.clone();
    let mut trimmed = false;
    let mut retried_5xx = false;
    let outcome = loop {
        let messages = match fit(&chat, &history) {
            Ok(messages) => messages,
            Err(why) => {
                emit(PEvent::Error(why)).await;
                break Outcome::Failed;
            }
        };
        let attempt = tokio::select! {
            _ = cancel.cancelled() => Attempt::Done(Outcome::Interrupted),
            attempt = request(&client, &chat, messages, &emit) => attempt,
        };
        match attempt {
            Attempt::Done(outcome) => break outcome,
            Attempt::TooLong(why) if !trimmed && !history.is_empty() => {
                // Drop the oldest quarter (at least one turn) and try once more.
                let drop = history.len().div_ceil(4);
                history.drain(..drop);
                trimmed = true;
                tracing::info!(id, %why, dropped = drop, "context too long, retrying");
            }
            Attempt::TooLong(why) => {
                emit(PEvent::Error(format!(
                    "the model refused the request as too long: {why}"
                )))
                .await;
                break Outcome::Failed;
            }
            Attempt::ServerError(why) if !retried_5xx => {
                retried_5xx = true;
                tracing::info!(id, %why, "server error, retrying once");
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
    messages: Vec<Value>,
    emit: &F,
) -> Attempt
where
    F: Fn(PEvent) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let url = format!("{}/chat/completions", chat.base_url.trim_end_matches('/'));
    let body = json!({
        "model": chat.model,
        "messages": messages,
        "stream": true,
        "max_tokens": chat.max_output,
        "stream_options": {"include_usage": true},
    });
    let mut builder = client.post(&url).json(&body);
    if let Some(key) = &chat.key {
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
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        let message = provider_message(&text).unwrap_or_else(|| text.chars().take(300).collect());
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
    let mut finished = false;
    while !finished {
        let chunk = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
            Err(_) => {
                emit(PEvent::Error(format!(
                    "{}: no data for {}s",
                    chat.base_url,
                    IDLE_TIMEOUT.as_secs()
                )))
                .await;
                return Attempt::Done(Outcome::Failed);
            }
            Ok(None) => break,
            Ok(Some(Err(err))) => {
                emit(PEvent::Error(format!(
                    "{}: {}",
                    chat.base_url,
                    without_url(&err)
                )))
                .await;
                return Attempt::Done(Outcome::Failed);
            }
            Ok(Some(Ok(bytes))) => bytes,
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
                emit(PEvent::TextDelta(text.into())).await;
            }
            if let Some(u) = event.get("usage").filter(|u| !u.is_null()) {
                usage = Some((
                    u["prompt_tokens"].as_u64().unwrap_or(0),
                    u["completion_tokens"].as_u64().unwrap_or(0),
                ));
            }
            if let Some(message) = event.pointer("/error/message").and_then(Value::as_str) {
                emit(PEvent::Error(format!("{}: {message}", chat.base_url))).await;
                return Attempt::Done(Outcome::Failed);
            }
        }
    }
    let (input, output, estimated) = match usage {
        Some((input, output)) => (input, output, false),
        // The provider didn't say: estimate, and say so (ADR 0005).
        None => (
            estimate(&chat.system) + estimate(&chat.prompt),
            estimate(&reply_chars),
            true,
        ),
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
/// once. The error names the source, never a value.
pub fn resolve_key(reference: &str) -> Result<String, String> {
    if let Some(var) = reference.strip_prefix("env:") {
        return std::env::var(var).map_err(|_| format!("environment variable {var} is not set"));
    }
    if let Some(name) = reference.strip_prefix("keyring:") {
        return keyring::Entry::new("roundtable", name)
            .and_then(|entry| entry.get_password())
            .map_err(|e| format!("keychain entry roundtable/{name}: {e}"));
    }
    Err(format!("`{reference}` is not a key reference"))
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
            key: None,
            system: "sys".into(),
            history: history
                .into_iter()
                .map(|(a, b)| (a.into(), b.into()))
                .collect(),
            prompt: "now".into(),
            context_window: window,
            max_output: 10,
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
        let messages = fit(&c, &c.history).unwrap();
        let texts: Vec<&str> = messages
            .iter()
            .map(|m| m["content"].as_str().unwrap())
            .collect();
        assert_eq!(
            texts,
            ["sys", "new q", "new a", "now"],
            "only the newest turn fits"
        );
        let roomy = chat(vec![("old q", "old a"), ("new q", "new a")], 1000);
        assert_eq!(fit(&roomy, &roomy.history).unwrap().len(), 6);
        let tiny = chat(vec![], 12);
        assert!(fit(&tiny, &[]).unwrap_err().contains("only 2 fit"));
    }

    #[test]
    fn keys_resolve_from_env_and_never_from_text() {
        // SAFETY: a variable only this test reads.
        unsafe { std::env::set_var("ROUNDTABLE_TEST_KEY", "sekret") };
        assert_eq!(resolve_key("env:ROUNDTABLE_TEST_KEY").unwrap(), "sekret");
        assert!(
            resolve_key("env:ROUNDTABLE_NO_SUCH_VAR")
                .unwrap_err()
                .contains("not set")
        );
        assert!(resolve_key("sk-literal").is_err());
    }

    /// A one-shot fake provider: answers each connection with the next canned response and
    /// hands back the request bodies it got.
    async fn fake(responses: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<Value>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let mut bodies = vec![];
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![];
                let mut buf = [0u8; 65536];
                // Read headers, then exactly content-length bytes of body.
                let body = loop {
                    let n = socket.read(&mut buf).await.unwrap();
                    request.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&request).to_string();
                    if let Some(split) = text.find("\r\n\r\n") {
                        let length: usize = text
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        if request.len() >= split + 4 + length {
                            break serde_json::from_slice(&request[split + 4..split + 4 + length])
                                .unwrap();
                        }
                    }
                };
                bodies.push(body);
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
            bodies
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
    async fn streams_deltas_and_reported_usage() {
        let (url, server) = fake(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"po"}}]}"#,
            r#"{"choices":[{"delta":{"content":"ng"}}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":2}}"#,
            "[DONE]",
        ])])
        .await;
        let mut c = chat(vec![("q", "a")], 1000);
        c.base_url = url;
        c.key = Some("sekret".into());
        let events = run_chat(c).await;
        assert_eq!(
            events,
            [
                PEvent::TextDelta("po".into()),
                PEvent::TextDelta("ng".into()),
                PEvent::Usage {
                    input: 12,
                    output: 2,
                    cost_usd: None,
                    estimated: false
                },
                PEvent::Done(Outcome::Complete)
            ]
        );
        let body = &server.await.unwrap()[0];
        assert_eq!(body["stream"], true);
        assert_eq!(
            body["messages"].as_array().unwrap().len(),
            4,
            "system, q, a, prompt"
        );
    }

    #[tokio::test]
    async fn a_429_is_shown_and_not_retried() {
        let (url, server) = fake(vec![status(429, "rate limited, try later")]).await;
        let mut c = chat(vec![], 1000);
        c.base_url = url.clone();
        let events = run_chat(c).await;
        assert!(
            matches!(&events[0], PEvent::Error(e) if e.contains("429") && e.contains("rate limited"))
        );
        assert_eq!(events.last(), Some(&PEvent::Done(Outcome::Failed)));
        assert_eq!(server.await.unwrap().len(), 1, "one request only");
    }

    #[tokio::test]
    async fn a_5xx_is_retried_once() {
        let (url, server) = fake(vec![
            status(503, "overloaded"),
            sse(&[r#"{"choices":[{"delta":{"content":"ok"}}]}"#, "[DONE]"]),
        ])
        .await;
        let mut c = chat(vec![], 1000);
        c.base_url = url;
        let events = run_chat(c).await;
        assert_eq!(events[0], PEvent::TextDelta("ok".into()));
        assert!(
            matches!(
                events[1],
                PEvent::Usage {
                    estimated: true,
                    ..
                }
            ),
            "no usage reported: estimated"
        );
        assert_eq!(server.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn too_long_retries_once_with_a_quarter_less_history() {
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
        let bodies = server.await.unwrap();
        let count = |b: &Value| b["messages"].as_array().unwrap().len();
        assert_eq!(
            (count(&bodies[0]), count(&bodies[1])),
            (2 + 16, 2 + 12),
            "8 turns, then 6"
        );
    }
}
