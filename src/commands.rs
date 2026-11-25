//! The slash commands (ADR 0008): one static table, so `/help` and completion can't drift
//! from what the app accepts.

pub struct Command {
    pub name: &'static str,
    pub args: &'static str,
    pub help: &'static str,
}

pub const COMMANDS: &[Command] = &[
    Command {
        name: "/help",
        args: "",
        help: "list the commands",
    },
    Command {
        name: "/clear",
        args: "",
        help: "start a new thread",
    },
    Command {
        name: "/resume",
        args: "[n|id]",
        help: "list past threads, or reopen one (by number or id)",
    },
    Command {
        name: "/exit",
        args: "",
        help: "quit (also Ctrl-D on an empty line, or Ctrl-C twice)",
    },
    Command {
        name: "/status",
        args: "",
        help: "each participant: ready or why not; talk and edit state",
    },
    Command {
        name: "/usage",
        args: "[thread|today|all]",
        help: "plan limits of claude and codex (5h, week), then tokens and cost per model",
    },
    Command {
        name: "/worktrees",
        args: "[clean [id]]",
        help: "the read-only agents' mirrors of this checkout, with sizes; or remove them",
    },
    Command {
        name: "/log",
        args: "",
        help: "open the whole thread as Markdown in the configured `editor`",
    },
    Command {
        name: "/context",
        args: "[id]",
        help: "what each participant has seen and how full its window is",
    },
    Command {
        name: "/models",
        args: "",
        help: "list the models",
    },
    Command {
        name: "/model",
        args: "enable|disable|use|test <id>",
        help: "switch a model on or off, make it the default, or ping it",
    },
    Command {
        name: "/talk",
        args: "on|off|show|allow a->b|deny a->b",
        help: "model-to-model talk: the switch and who may ask whom",
    },
    Command {
        name: "/budget",
        args: "<tokens>",
        help: "model-to-model token budget per message",
    },
    Command {
        name: "/turns",
        args: "<n>",
        help: "model-to-model turns per message",
    },
    Command {
        name: "/quiet",
        args: "",
        help: "after a crash: confirm no writer is still running, unlock edits",
    },
];

/// The generated `/help` text.
pub fn help() -> String {
    let width = COMMANDS
        .iter()
        .map(|c| c.name.len() + c.args.len() + 1)
        .max()
        .unwrap_or(0);
    let mut lines: Vec<String> = COMMANDS
        .iter()
        .map(|c| {
            let usage = format!("{} {}", c.name, c.args);
            format!("{:width$}  {}", usage.trim_end(), c.help)
        })
        .collect();
    lines.push(String::new());
    lines.push(
        "@id or @all at the start of a message picks who gets it; \\/ sends a literal /.".into(),
    );
    lines.push("Enter sends, Alt-Enter (or Shift-Enter) adds a line, Tab completes, Esc cancels running replies, PgUp/PgDn scroll.".into());
    lines.join("\n")
}

/// Completions for what is typed: a command name at the start, a participant id after a
/// command that takes one, or a mention (`@`) among the leading mentions.
pub fn complete(input: &str, ids: &[String]) -> Vec<String> {
    let words: Vec<&str> = input.split(' ').collect();
    let last = *words.last().unwrap_or(&"");
    if words.len() == 1 && last.starts_with('/') {
        return COMMANDS
            .iter()
            .map(|c| c.name.to_string())
            .filter(|name| name.starts_with(last) && name != last)
            .collect();
    }
    let takes_id = matches!(
        words.as_slice(),
        ["/context", _] | ["/model", "enable" | "disable" | "use" | "test", _]
    );
    if takes_id {
        return ids
            .iter()
            .filter(|id| id.starts_with(last) && id.as_str() != last)
            .cloned()
            .collect();
    }
    let all_mentions = words[..words.len() - 1].iter().all(|w| w.starts_with('@'));
    if all_mentions && let Some(partial) = last.strip_prefix('@') {
        return ids
            .iter()
            .map(String::as_str)
            // `@all?`: everyone, nobody edits (ADR 0017).
            .chain(["all", "all?"])
            .filter(|id| id.starts_with(partial) && *id != partial)
            .map(|id| format!("@{id}"))
            .collect();
    }
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> Vec<String> {
        vec!["claude".into(), "codex".into(), "llama".into()]
    }

    #[test]
    fn help_lists_every_command() {
        let help = help();
        for c in COMMANDS {
            assert!(help.contains(c.name), "{}", c.name);
        }
    }

    #[test]
    fn completes_commands_ids_and_mentions() {
        assert_eq!(complete("/mo", &ids()), ["/models", "/model"]);
        assert_eq!(complete("/model use c", &ids()), ["claude", "codex"]);
        assert_eq!(complete("/context l", &ids()), ["llama"]);
        assert_eq!(complete("@c", &ids()), ["@claude", "@codex"]);
        assert_eq!(complete("@codex @a", &ids()), ["@all", "@all?"]);
        assert_eq!(complete("@all", &ids()), ["@all?"]);
        assert!(
            complete("hello @c", &ids()).is_empty(),
            "only leading mentions route"
        );
        assert!(complete("/models", &ids()).is_empty(), "already complete");
    }
}
