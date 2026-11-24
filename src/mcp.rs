//! `roundtable mcp`: the MCP server an agent CLI launches to get the `ask` tool (ADR 0007).
//! It speaks MCP over stdio to the CLI and forwards each `ask` to the running roundtable
//! over a private Unix socket, which decides (talk policy, limits) and returns the answer.
//!
//! The protocol subset is small (initialize, tools/list, tools/call), so it is written out
//! here instead of pulling in an SDK.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Environment the agent's MCP config passes to `roundtable mcp`.
pub const ENV_SOCKET: &str = "ROUNDTABLE_SOCKET";
pub const ENV_CALLER: &str = "ROUNDTABLE_CALLER";
pub const ENV_TOKEN: &str = "ROUNDTABLE_TOKEN";

/// One line on the socket, from `roundtable mcp` to roundtable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskRequest {
    /// Proves the call comes from the caller's live turn (one-time per turn).
    pub token: String,
    pub caller: String,
    pub to: String,
    pub prompt: String,
}

/// The answer line back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskAnswer {
    pub text: String,
    /// Blocked, busy or failed: shown to the calling model as a tool error it can work around.
    pub refused: bool,
}

const TOOL: &str = "ask";

fn tool_schema() -> Value {
    json!({
        "name": TOOL,
        "description": "Ask another participant at this roundtable (another AI model) a question \
                        and get its answer. Use it for a second opinion or a review. The answer \
                        may be 'blocked' or 'busy'; then carry on without it.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "to": {"type": "string", "description": "The participant's id, e.g. codex"},
                "prompt": {"type": "string", "description": "Your question, self-contained"}
            },
            "required": ["to", "prompt"]
        }
    })
}

/// Handles one JSON-RPC message from the CLI; `None` for notifications (no reply).
/// `ask` does the round trip to roundtable.
pub fn handle(message: &str, ask: &mut dyn FnMut(&str, &str) -> AskAnswer) -> Option<Value> {
    let Ok(request) = serde_json::from_str::<Value>(message) else {
        return Some(error(Value::Null, -32700, "parse error"));
    };
    let id = request.get("id").cloned()?; // no id: a notification
    let method = request["method"].as_str().unwrap_or_default();
    let result = match method {
        "initialize" => json!({
            "protocolVersion": request.pointer("/params/protocolVersion").cloned()
                .unwrap_or_else(|| json!("2025-06-18")),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "roundtable", "version": env!("CARGO_PKG_VERSION")}
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": [tool_schema()]}),
        "tools/call" => {
            if request.pointer("/params/name").and_then(Value::as_str) != Some(TOOL) {
                return Some(error(id, -32602, "unknown tool"));
            }
            let args = request
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_default();
            let (Some(to), Some(prompt)) = (args["to"].as_str(), args["prompt"].as_str()) else {
                return Some(error(id, -32602, "`to` and `prompt` are required"));
            };
            let answer = ask(to, prompt);
            json!({
                "content": [{"type": "text", "text": answer.text}],
                "isError": answer.refused
            })
        }
        _ => return Some(error(id, -32601, "method not found")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The `roundtable mcp` process: MCP over stdin/stdout until the CLI closes it.
pub fn serve() -> std::io::Result<()> {
    let socket = std::env::var(ENV_SOCKET).unwrap_or_default();
    let caller = std::env::var(ENV_CALLER).unwrap_or_default();
    let token = std::env::var(ENV_TOKEN).unwrap_or_default();
    let mut ask = |to: &str, prompt: &str| {
        let request = AskRequest {
            token: token.clone(),
            caller: caller.clone(),
            to: to.into(),
            prompt: prompt.into(),
        };
        round_trip(&socket, &request).unwrap_or_else(|err| AskAnswer {
            text: format!("blocked: roundtable is not reachable ({err})"),
            refused: true,
        })
    };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = handle(&line, &mut ask) {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// One request and its answer over roundtable's socket. Blocks until the asked participant
/// has answered (or the ask was refused).
pub fn round_trip(socket: &str, request: &AskRequest) -> std::io::Result<AskAnswer> {
    let mut stream = UnixStream::connect(socket)?;
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    serde_json::from_str(&reply).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(line: &str) -> Option<Value> {
        handle(line, &mut |to, prompt| AskAnswer {
            text: format!("{to} says: {prompt}?"),
            refused: to == "nobody",
        })
    }

    #[test]
    fn speaks_the_mcp_handshake_and_lists_the_tool() {
        let init = call(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#).unwrap();
        assert_eq!(
            init["result"]["protocolVersion"], "2025-03-26",
            "echoes the client's version"
        );
        assert!(call(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        let list = call(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).unwrap();
        assert_eq!(list["result"]["tools"][0]["name"], "ask");
        assert_eq!(
            call(r#"{"jsonrpc":"2.0","id":3,"method":"nope"}"#).unwrap()["error"]["code"],
            -32601
        );
    }

    #[test]
    fn a_call_returns_the_answer_or_a_tool_error() {
        let ok = call(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"ask","arguments":{"to":"codex","prompt":"why"}}}"#).unwrap();
        assert_eq!(ok["result"]["content"][0]["text"], "codex says: why?");
        assert_eq!(ok["result"]["isError"], false);
        let refused = call(r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"ask","arguments":{"to":"nobody","prompt":"x"}}}"#).unwrap();
        assert_eq!(refused["result"]["isError"], true);
        let missing = call(r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"ask","arguments":{"to":"codex"}}}"#).unwrap();
        assert_eq!(missing["error"]["code"], -32602);
    }

    #[test]
    fn round_trip_over_a_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ask.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: AskRequest = serde_json::from_str(&line).unwrap();
            let answer = AskAnswer {
                text: format!("hi {}", request.caller),
                refused: false,
            };
            let mut stream = stream;
            writeln!(stream, "{}", serde_json::to_string(&answer).unwrap()).unwrap();
        });
        let request = AskRequest {
            token: "t".into(),
            caller: "claude".into(),
            to: "codex".into(),
            prompt: "p".into(),
        };
        let answer = round_trip(path.to_str().unwrap(), &request).unwrap();
        assert_eq!(answer.text, "hi claude");
        server.join().unwrap();
    }
}
