//! `/log` (ADR 0024): the whole thread as Markdown, built from the transcript, never from
//! the screen, so it copies cleanly in an editor.

use crate::transcript::{EventLine, Kind, day_of};

/// One rendered piece of the thread: a message, a tool line, an error. `/handoff`
/// (ADR 0020) checks each against who it came from and went to.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub text: String,
    pub from: String,
    pub to: Vec<String>,
    /// A message (yours or a model's), as opposed to a tool line or an error.
    pub message: bool,
}

/// The thread as Markdown. `offset_at` gives the local UTC offset in seconds at a Unix second,
/// so each time is shifted by its own offset (the loop passes the real one; tests a constant).
pub fn render(title: &str, events: &[EventLine], offset_at: impl Fn(i64) -> i64) -> String {
    let local = |ts: u64| ts.saturating_add_signed(offset_at((ts / 1000) as i64) * 1000);
    let mut out = format!(
        "# {title} · {}\n",
        events
            .first()
            .map_or_else(String::new, |e| day_of(local(e.ts)))
    );
    for entry in entries(events, &offset_at) {
        out += &entry.text;
    }
    out
}

/// The thread's entries in order, as `render` writes them (without its title line).
pub fn entries(events: &[EventLine], offset_at: impl Fn(i64) -> i64) -> Vec<Entry> {
    let local = |ts: u64| ts.saturating_add_signed(offset_at((ts / 1000) as i64) * 1000);
    let mut list = vec![];
    for e in events {
        let mut out = String::new();
        let mut message = true;
        let text = |key: &str| e.payload[key].as_str().unwrap_or("").trim_end().to_string();
        let to: Vec<&str> = e.to.iter().map(|p| p.id.as_str()).collect();
        let when = clock(local(e.ts));
        match e.kind {
            Kind::Msg if e.from.id == "user" => {
                out += &format!("\n## you · {when}\n\n{}\n", text("text"));
            }
            // A reply to another model: the answer to its `ask`.
            Kind::Msg if !to.is_empty() && to != ["user"] => {
                out += &format!(
                    "\n> {} → {} · {when}\n>\n{}",
                    e.from.id,
                    to.join(", "),
                    quote(&text("text"))
                );
            }
            Kind::Msg => {
                let status = match e.payload["status"].as_str() {
                    Some("complete") | None => String::new(),
                    Some(other) => format!(" ({other})"),
                };
                out += &format!("\n## {}{status} · {when}\n\n{}\n", e.from.id, text("text"));
            }
            Kind::Handoff => {
                let blocked = match e.payload["reason"].as_str() {
                    Some(why) => format!(" · blocked: {why}"),
                    None => String::new(),
                };
                out += &format!(
                    "\n> {} → {} (ask){blocked} · {when}\n>\n{}",
                    e.from.id,
                    to.join(", "),
                    quote(&text("prompt"))
                );
            }
            Kind::ToolCall => {
                message = false;
                out += &format!("\n`[{}: {}]`\n", e.from.id, text("summary"));
            }
            Kind::Error => {
                message = false;
                out += &format!("\n> ! {}: {}\n", e.from.id, text("text"));
            }
            // Bookkeeping lines: deliveries, settles, approvals, registry notes.
            _ => continue,
        }
        list.push(Entry {
            text: out,
            from: e.from.id.clone(),
            to: to.iter().map(|t| (*t).to_string()).collect(),
            message,
        });
    }
    list
}

/// `HH:MM` of a Unix-milliseconds time.
fn clock(ms: u64) -> String {
    let minutes = ms / 60_000;
    format!("{:02}:{:02}", minutes / 60 % 24, minutes % 60)
}

/// A Markdown quote of `text`, every line prefixed, ending in a newline.
fn quote(text: &str) -> String {
    text.lines()
        .map(|l| format!("> {l}\n").replace("> \n", ">\n"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Party;
    use serde_json::json;

    fn event(
        seq: u64,
        from: &str,
        to: &[&str],
        kind: Kind,
        payload: serde_json::Value,
    ) -> EventLine {
        EventLine {
            seq,
            // 2026-11-25 19:41 UTC plus a minute per line.
            ts: 1_795_635_660_000 + seq * 60_000,
            turn: None,
            parent: None,
            from: Party {
                id: from.into(),
                identity: None,
            },
            to: to
                .iter()
                .map(|id| Party {
                    id: (*id).into(),
                    identity: None,
                })
                .collect(),
            kind,
            payload,
            tokens_in: None,
            tokens_out: None,
        }
    }

    #[test]
    fn a_thread_renders_as_markdown() {
        let events = [
            event(
                0,
                "user",
                &["claude"],
                Kind::Msg,
                json!({"text": "@claude review src/app.rs"}),
            ),
            event(
                1,
                "router",
                &["claude"],
                Kind::Delivery,
                json!({"status": "sent"}),
            ),
            event(
                2,
                "claude",
                &[],
                Kind::ToolCall,
                json!({"summary": "Read src/app.rs"}),
            ),
            event(
                3,
                "claude",
                &["codex"],
                Kind::Handoff,
                json!({"prompt": "Is `submit` safe?\n\nCheck the lock.", "status": "sent"}),
            ),
            event(
                4,
                "codex",
                &["claude"],
                Kind::Msg,
                json!({"text": "Yes.\nThe lock covers it.", "status": "complete"}),
            ),
            event(5, "codex", &[], Kind::Error, json!({"text": "usage limit"})),
            event(
                6,
                "claude",
                &["user"],
                Kind::Msg,
                json!({"text": "Looks fine:\n\n```rust\nfn submit() {}\n```\n", "status": "complete"}),
            ),
            event(
                7,
                "codex",
                &["user"],
                Kind::Msg,
                json!({"text": "", "status": "failed"}),
            ),
            event(
                8,
                "router",
                &[],
                Kind::Status,
                json!({"registry": "default: all"}),
            ),
        ];
        insta::assert_snapshot!(render("review", &events, |_| 0));
    }

    #[test]
    fn times_follow_the_offset() {
        let events = [event(0, "user", &[], Kind::Msg, json!({"text": "hi"}))];
        assert!(render("t", &events, |_| 7 * 3600).contains("## you · 02:41"));
        assert!(render("t", &events, |_| 7 * 3600).starts_with("# t · 2026-11-26"));
    }

    #[test]
    fn an_empty_thread_has_only_its_title() {
        assert_eq!(render("t", &[], |_| 0), "# t · \n");
    }
}
