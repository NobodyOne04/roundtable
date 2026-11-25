//! The model registry (ADR 0011): who sits at the table. It lives in memory; `models.toml`
//! is its persisted copy, read whole and applied only when every entry is valid.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::agent;

/// Written on first run. Comments survive later edits (`toml_edit`).
pub const DEFAULT_FILE: &str = r#"# roundtable model registry (ADR 0011).
# Edits apply live while roundtable runs. A file with an error is not applied at all:
# the chat shows the error and the previous registry stays in use.

# Who gets a message without an @mention; "all" sends to everyone. The writer may edit from
# any message that reaches it; start one with @all? when nobody should (ADR 0017).
default = "claude"
# The only participant that may edit files; remove the line and everyone is read-only.
writer = "claude"
# The editor /log opens the thread in (a command line; the file is added last). The file is
# deleted when the editor closes (ADR 0024).
editor = "nvim"

[[model]]
id = "claude"
kind = "cli"
cmd = "claude"
role = "lead"
enabled = true
# model = "claude-opus-5-5"          # passed as --model
# permission_mode = "acceptEdits"    # used only for the writer's own turns

[[model]]
id = "codex"
kind = "cli"
cmd = "codex"
role = "reviewer"
enabled = true

# A free model over an OpenAI-compatible API (answers arrive with ADR 0005):
# [[model]]
# id = "llama"
# kind = "openai"
# base_url = "http://localhost:11434/v1"
# model = "llama3.3"
# context_window = 128000
# max_output_tokens = 4096           # kept free for the reply
# connect_timeout_s = 10
# idle_timeout_s = 120               # no data for this long ends the turn
# key = "keyring:ollama"             # or "env:VAR"; never the key itself
# enabled = true

# Model-to-model talk (ADR 0007).
[talk]
enabled = true
max_turns = 6
budget_tokens = 50000
allow = { claude = ["codex"], codex = [] }
"#;

const RESERVED: [&str; 4] = ["all", "user", "router", "you"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Cli,
    Openai,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    id: String,
    kind: EntryKind,
    cmd: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    context_window: Option<u64>,
    max_output_tokens: Option<u64>,
    connect_timeout_s: Option<u64>,
    idle_timeout_s: Option<u64>,
    key: Option<String>,
    #[serde(default = "yes")]
    enabled: bool,
    role: Option<String>,
    permission_mode: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Talk {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "six")]
    pub max_turns: u32,
    #[serde(default = "fifty_k")]
    pub budget_tokens: u64,
    /// Who may `ask` whom; a caller not listed may ask no one (default deny).
    #[serde(default)]
    pub allow: BTreeMap<String, Vec<String>>,
}

fn six() -> u32 {
    6
}

fn fifty_k() -> u64 {
    50_000
}

impl Default for Talk {
    fn default() -> Self {
        Self {
            enabled: true,
            max_turns: six(),
            budget_tokens: fifty_k(),
            allow: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    default: Option<String>,
    writer: Option<String>,
    editor: Option<String>,
    #[serde(default, rename = "model")]
    models: Vec<RawEntry>,
    #[serde(default)]
    talk: Talk,
}

/// Which code drives a participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adapter {
    Cli(agent::Kind),
    /// OpenAI-compatible HTTP (ADR 0005).
    OpenAi,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Participant {
    pub id: String,
    pub adapter: Adapter,
    pub cmd: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub context_window: Option<u64>,
    /// Tokens kept free for the reply when fitting history (ADR 0005).
    pub max_output_tokens: Option<u64>,
    /// HTTP timeouts in seconds (ADR 0005: configurable).
    pub connect_timeout_s: Option<u64>,
    pub idle_timeout_s: Option<u64>,
    /// A reference (`keyring:<name>` or `env:<VAR>`), resolved per request.
    pub key: Option<String>,
    pub enabled: bool,
    pub role: Option<String>,
    pub permission_mode: Option<String>,
    /// Names the exact destination; history and sessions are bound to it, not to `id`.
    pub identity: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Registry {
    pub participants: Vec<Participant>,
    /// As configured; `default()` and `writer()` give what is in effect right now.
    pub default: Option<String>,
    pub writer: Option<String>,
    /// `/log`'s editor command, split into words (ADR 0024); empty when unset.
    pub editor: Vec<String>,
    pub talk: Talk,
    /// Not errors, but the user should know (e.g. a `writer` that was dropped).
    pub warnings: Vec<String>,
}

impl Registry {
    /// Parses and validates a whole file. Any error rejects all of it (ADR 0011).
    pub fn parse(text: &str) -> Result<Registry, String> {
        let raw: RawFile = toml::from_str(text).map_err(|err| err.to_string())?;
        let mut errors = vec![];
        let mut participants: Vec<Participant> = vec![];
        for entry in raw.models {
            match participant(entry) {
                Ok(p) if participants.iter().any(|q| q.id == p.id) => {
                    errors.push(format!("model `{}`: id used twice", p.id));
                }
                Ok(p) => participants.push(p),
                Err(err) => errors.push(err),
            }
        }
        for (caller, targets) in &raw.talk.allow {
            for id in std::iter::once(caller).chain(targets) {
                if !participants.iter().any(|p| &p.id == id) {
                    errors.push(format!("[talk] allow: no model `{id}`"));
                }
            }
        }
        let editor = match raw.editor.as_deref().map(shell_words::split) {
            None => vec![],
            Some(Ok(words)) => words,
            Some(Err(err)) => {
                errors.push(format!("editor: {err}"));
                vec![]
            }
        };
        if !errors.is_empty() {
            return Err(errors.join("\n"));
        }

        let mut reg = Registry {
            participants,
            default: raw.default,
            writer: raw.writer,
            editor,
            talk: raw.talk,
            warnings: vec![],
        };
        if let Some(id) = &reg.default
            && reg.default().is_none()
        {
            let warning =
                format!("default `{id}` is missing or disabled: start messages with @<id>");
            reg.warnings.push(warning);
        }
        if let Some(id) = &reg.writer
            && reg.writer().is_none()
        {
            let warning =
                format!("writer `{id}` is not an enabled cli model: everyone is read-only");
            reg.warnings.push(warning);
        }
        Ok(reg)
    }

    /// Who gets a message without an `@mention`: `None` when unset, unknown or disabled.
    /// `"all"` means every enabled participant (a broadcast, so read-only).
    pub fn default(&self) -> Option<&str> {
        let id = self.default.as_deref()?;
        if id == "all" {
            return self.participants.iter().any(|p| p.enabled).then_some("all");
        }
        self.get(id).filter(|p| p.enabled).map(|p| p.id.as_str())
    }

    /// The one participant allowed to edit: `None` unless it is an enabled cli model
    /// (fail closed).
    pub fn writer(&self) -> Option<&str> {
        let id = self.writer.as_deref()?;
        self.get(id)
            .filter(|p| p.enabled && matches!(p.adapter, Adapter::Cli(_)))
            .map(|p| p.id.as_str())
    }

    pub fn get(&self, id: &str) -> Option<&Participant> {
        self.participants.iter().find(|p| p.id == id)
    }
}

fn participant(e: RawEntry) -> Result<Participant, String> {
    let at = |msg: &str| format!("model `{}`: {msg}", e.id);
    if e.id.is_empty()
        || !e
            .id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(at("id must be lowercase letters, digits and -"));
    }
    if RESERVED.contains(&e.id.as_str()) {
        return Err(at("this id is reserved"));
    }
    if let Some(key) = &e.key
        && !(key.starts_with("keyring:") || key.starts_with("env:"))
    {
        return Err(at(
            "`key` must be a reference (keyring:<name> or env:<VAR>), never the key itself",
        ));
    }
    let adapter = match e.kind {
        EntryKind::Cli => {
            // Fields of the other kind are refused rather than silently ignored.
            if e.base_url.is_some()
                || e.key.is_some()
                || e.context_window.is_some()
                || e.max_output_tokens.is_some()
                || e.connect_timeout_s.is_some()
                || e.idle_timeout_s.is_some()
            {
                return Err(at(
                    "cli models take no `base_url`, `key`, `context_window`, `max_output_tokens` or timeouts",
                ));
            }
            let cmd = e
                .cmd
                .as_deref()
                .ok_or_else(|| at("cli models need `cmd`"))?;
            let name = std::path::Path::new(cmd)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let kind = agent::Kind::from_id(name)
                .ok_or_else(|| at("`cmd` must be the claude or codex CLI"))?;
            if let Some(mode) = &e.permission_mode {
                let ok = match kind {
                    agent::Kind::Claude => [
                        "default",
                        "acceptEdits",
                        "plan",
                        "dontAsk",
                        "auto",
                        "manual",
                    ]
                    .contains(&mode.as_str()),
                    agent::Kind::Codex => ["read-only", "workspace-write"].contains(&mode.as_str()),
                };
                if !ok {
                    return Err(at(&format!(
                        "permission_mode `{mode}` is not allowed (never bypass or full access)"
                    )));
                }
            }
            Adapter::Cli(kind)
        }
        EntryKind::Openai => {
            if e.cmd.is_some() || e.permission_mode.is_some() {
                return Err(at("openai models take no `cmd` or `permission_mode`"));
            }
            let url = e
                .base_url
                .as_deref()
                .ok_or_else(|| at("openai models need `base_url`"))?;
            if e.model.is_none() {
                return Err(at("openai models need `model`"));
            }
            let remote_http = url.starts_with("http://") && !is_loopback(url);
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err(at("`base_url` must be http(s)"));
            }
            if authority(url).contains('@') {
                return Err(at("`base_url` must not contain user info (user@host)"));
            }
            if remote_http && e.key.is_some() {
                return Err(at(
                    "a key over plain http to a non-local host would travel in cleartext; use https",
                ));
            }
            Adapter::OpenAi
        }
    };
    // Each kind's own destination: a cli is its command, an openai model its URL.
    let destination = match e.kind {
        EntryKind::Cli => e.cmd.as_deref(),
        EntryKind::Openai => e.base_url.as_deref(),
    }
    .unwrap_or("");
    let identity = fingerprint(&format!(
        "{:?}|{destination}|{}",
        e.kind,
        e.model.as_deref().unwrap_or("")
    ));
    Ok(Participant {
        id: e.id,
        adapter,
        cmd: e.cmd,
        base_url: e.base_url,
        model: e.model,
        context_window: e.context_window,
        max_output_tokens: e.max_output_tokens,
        connect_timeout_s: e.connect_timeout_s,
        idle_timeout_s: e.idle_timeout_s,
        key: e.key,
        enabled: e.enabled,
        role: e.role,
        permission_mode: e.permission_mode,
        identity,
    })
}

/// The `host[:port]` part of a URL, user info included if present.
fn authority(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().unwrap_or("")
}

fn is_loopback(url: &str) -> bool {
    let rest = authority(url);
    let host = match rest.strip_prefix('[') {
        Some(v6) => v6.split(']').next(),
        None => rest.split(['/', ':']).next(),
    };
    matches!(host, Some("localhost" | "127.0.0.1" | "::1"))
}

/// FNV-1a: stable across Rust versions and runs, unlike `DefaultHasher`, so identities
/// written to the transcript keep matching.
pub fn fingerprint(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Sets `enabled` of model `id` in the file text, keeping comments and layout.
pub fn set_enabled(text: &str, id: &str, enabled: bool) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{e}"))?;
    let models = doc
        .get_mut("model")
        .and_then(|m| m.as_array_of_tables_mut())
        .ok_or("no [[model]] entries")?;
    let table = models
        .iter_mut()
        .find(|t| t.get("id").and_then(|v| v.as_str()) == Some(id))
        .ok_or_else(|| format!("no model `{id}` in the file"))?;
    table["enabled"] = toml_edit::value(enabled);
    Ok(doc.to_string())
}

/// Sets `[talk] enabled`, keeping comments and layout.
pub fn set_talk_enabled(text: &str, enabled: bool) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{e}"))?;
    if doc.get("talk").is_none() {
        doc["talk"] = toml_edit::table();
    }
    doc["talk"]["enabled"] = toml_edit::value(enabled);
    Ok(doc.to_string())
}

/// Sets who `from` may ask (`[talk] allow`), keeping comments and layout.
pub fn set_allow(text: &str, from: &str, targets: &[String]) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{e}"))?;
    if doc.get("talk").is_none() {
        doc["talk"] = toml_edit::table();
    }
    if doc["talk"].get("allow").is_none() {
        doc["talk"]["allow"] = toml_edit::value(toml_edit::InlineTable::new());
    }
    let mut list = toml_edit::Array::new();
    for target in targets {
        list.push(target.as_str());
    }
    doc["talk"]["allow"][from] = toml_edit::value(list);
    Ok(doc.to_string())
}

/// Sets a number in `[talk]` (`max_turns`, `budget_tokens`), keeping comments and layout.
pub fn set_talk_number(text: &str, key: &str, value: i64) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{e}"))?;
    if doc.get("talk").is_none() {
        doc["talk"] = toml_edit::table();
    }
    doc["talk"][key] = toml_edit::value(value);
    Ok(doc.to_string())
}

/// Sets the top-level `default`, keeping comments and layout.
pub fn set_default(text: &str, id: &str) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{e}"))?;
    doc["default"] = toml_edit::value(id);
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_file_is_valid() {
        let reg = Registry::parse(DEFAULT_FILE).unwrap();
        let ids: Vec<_> = reg.participants.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["claude", "codex"]);
        assert_eq!(reg.default(), Some("claude"));
        assert_eq!(reg.writer(), Some("claude"));
        assert!(reg.warnings.is_empty());
    }

    #[test]
    fn the_editor_is_a_command_line_from_the_file() {
        assert_eq!(Registry::parse(DEFAULT_FILE).unwrap().editor, ["nvim"]);
        let set = |line: &str| Registry::parse(&DEFAULT_FILE.replace("editor = \"nvim\"", line));
        assert_eq!(
            set("editor = \"code --wait 'my dir'\"").unwrap().editor,
            ["code", "--wait", "my dir"]
        );
        assert!(
            set("").unwrap().editor.is_empty(),
            "unset: no editor, nothing built in"
        );
        assert!(set("editor = \"\"").unwrap().editor.is_empty());
        let err = set("editor = \"nvim 'unclosed\"").unwrap_err();
        assert!(err.contains("editor"), "{err}");
    }

    #[test]
    fn the_default_may_be_everyone() {
        let reg =
            Registry::parse(&DEFAULT_FILE.replace("default = \"claude\"", "default = \"all\""))
                .unwrap();
        assert_eq!(reg.default(), Some("all"));
        assert!(reg.warnings.is_empty());
        let none = DEFAULT_FILE
            .replace("default = \"claude\"", "default = \"all\"")
            .replace("enabled = true", "enabled = false");
        assert_eq!(Registry::parse(&none).unwrap().default(), None);
    }

    fn entry(extra: &str) -> String {
        format!("[[model]]\nid = \"x\"\n{extra}\n")
    }

    #[test]
    fn bad_entries_reject_the_whole_file() {
        let cases = [
            (entry("kind = \"cli\""), "need `cmd`"),
            (entry("kind = \"cli\"\ncmd = \"gemini\""), "claude or codex"),
            (
                entry("kind = \"openai\"\nbase_url = \"https://a\""),
                "need `model`",
            ),
            (
                entry(
                    "kind = \"openai\"\nbase_url = \"https://a\"\nmodel = \"m\"\nkey = \"sk-123\"",
                ),
                "never the key itself",
            ),
            (
                entry(
                    "kind = \"openai\"\nbase_url = \"http://example.com/v1\"\nmodel = \"m\"\nkey = \"env:K\"",
                ),
                "cleartext",
            ),
            (
                entry("kind = \"cli\"\ncmd = \"claude\"\npermission_mode = \"bypassPermissions\""),
                "never bypass",
            ),
            (
                entry("kind = \"cli\"\ncmd = \"codex\"\npermission_mode = \"danger-full-access\""),
                "never bypass",
            ),
            (
                entry(
                    "kind = \"openai\"\nbase_url = \"https://a\"\nmodel = \"m\"\ncmd = \"claude\"",
                ),
                "take no `cmd`",
            ),
            (
                entry("kind = \"cli\"\ncmd = \"claude\"\nbase_url = \"https://a\""),
                "take no `base_url`",
            ),
            (
                entry("kind = \"cli\"\ncmd = \"claude\"\ncolour = \"red\""),
                "unknown field",
            ),
            (
                "[[model]]\nid = \"all\"\nkind = \"cli\"\ncmd = \"claude\"\n".into(),
                "reserved",
            ),
            (
                "[[model]]\nid = \"Big\"\nkind = \"cli\"\ncmd = \"claude\"\n".into(),
                "lowercase",
            ),
            (
                format!(
                    "{}{}",
                    entry("kind = \"cli\"\ncmd = \"claude\""),
                    entry("kind = \"cli\"\ncmd = \"codex\"")
                ),
                "used twice",
            ),
            (
                format!(
                    "{}[talk]\nallow = {{ x = [\"ghost\"] }}\n",
                    entry("kind = \"cli\"\ncmd = \"claude\"")
                ),
                "no model `ghost`",
            ),
            ("default = \n".into(), "expected"),
        ];
        for (text, expected) in cases {
            let err = Registry::parse(&text).unwrap_err();
            assert!(err.contains(expected), "{text:?}: {err}");
        }
    }

    #[test]
    fn local_http_without_tls_is_fine() {
        for url in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:1234/v1",
            "http://[::1]:8080",
        ] {
            let text = entry(&format!(
                "kind = \"openai\"\nbase_url = \"{url}\"\nmodel = \"m\"\nkey = \"env:K\""
            ));
            assert!(Registry::parse(&text).is_ok(), "{url}");
        }
        for url in [
            "http://[2001:db8::1]/v1",
            "http://localhost.evil.com/v1",
            "http://localhost:1@evil.com/v1",
        ] {
            let text = entry(&format!(
                "kind = \"openai\"\nbase_url = \"{url}\"\nmodel = \"m\"\nkey = \"env:K\""
            ));
            assert!(Registry::parse(&text).is_err(), "{url} is not local");
        }
    }

    #[test]
    fn default_and_writer_fail_closed() {
        let text = "default = \"codex\"\nwriter = \"codex\"\n[[model]]\nid = \"codex\"\nkind = \"cli\"\ncmd = \"codex\"\nenabled = false\n";
        let reg = Registry::parse(text).unwrap();
        assert_eq!((reg.default(), reg.writer()), (None, None));
        assert_eq!(reg.warnings.len(), 2);
    }

    #[test]
    fn identity_follows_destination_not_id() {
        let a = Registry::parse(&entry("kind = \"cli\"\ncmd = \"claude\"")).unwrap();
        let b =
            Registry::parse(&entry("kind = \"cli\"\ncmd = \"claude\"\nrole = \"other\"")).unwrap();
        let c =
            Registry::parse(&entry("kind = \"cli\"\ncmd = \"claude\"\nmodel = \"m2\"")).unwrap();
        assert_eq!(a.participants[0].identity, b.participants[0].identity);
        assert_ne!(a.participants[0].identity, c.participants[0].identity);
        assert_eq!(fingerprint(""), "cbf29ce484222325", "FNV-1a offset basis");
    }

    #[test]
    fn edits_keep_comments() {
        let text = set_enabled(DEFAULT_FILE, "codex", false).unwrap();
        let text = set_default(&text, "codex").unwrap();
        assert!(text.contains("# The only participant that may edit files"));
        let reg = Registry::parse(&text).unwrap();
        assert!(!reg.get("codex").unwrap().enabled);
        assert_eq!(
            reg.default(),
            None,
            "codex is disabled, so it can't be default"
        );
        assert!(set_enabled(DEFAULT_FILE, "ghost", true).is_err());
        let text = set_allow(DEFAULT_FILE, "codex", &["claude".into()]).unwrap();
        let text = set_talk_number(&text, "max_turns", 3).unwrap();
        let text = set_talk_number(&text, "budget_tokens", 999).unwrap();
        let reg = Registry::parse(&text).unwrap();
        assert_eq!(reg.talk.allow["codex"], ["claude"]);
        assert_eq!(reg.talk.allow["claude"], ["codex"], "others untouched");
        assert_eq!((reg.talk.max_turns, reg.talk.budget_tokens), (3, 999));
        assert!(text.contains("# Model-to-model talk"), "comments kept");
    }
}
