//! Plan limits of the CLI agents (ADR 0014): how much of each subscription's 5-hour and
//! weekly window is used, from what the CLIs already write. Parsing and the report are
//! pure; the runner reads Codex's file and the loop keeps `limits.json`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::transcript::day_of;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub used_percent: f64,
    /// Unix seconds.
    pub resets_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    pub five_hour: Option<Window>,
    pub weekly: Option<Window>,
    /// The CLI says it is blocked now (not just near the limit).
    pub limited: bool,
    /// Unix milliseconds when the CLI reported these.
    pub seen_at: u64,
}

/// Claude's `rate_limit_event` (stream-json, seen with 2.1.332).
pub fn from_claude(event: &Value, seen_at: u64) -> Option<Limits> {
    let windows = event.pointer("/rate_limit_info/unifiedWindows")?;
    let window = |name: &str| {
        let w = windows.get(name)?;
        Some(Window {
            used_percent: w.get("utilization")?.as_f64()? * 100.0,
            resets_at: w.get("resetsAt")?.as_i64()?,
        })
    };
    let blocked = |name: &str| {
        windows
            .pointer(&format!("/{name}/status"))
            .and_then(Value::as_str)
            .is_some_and(|s| !s.starts_with("allowed"))
    };
    Some(Limits {
        five_hour: window("five_hour"),
        weekly: window("seven_day"),
        limited: blocked("five_hour") || blocked("seven_day"),
        seen_at,
    })
}

/// The last `rate_limits` with data in a Codex session file (`token_count` events, seen
/// with 0.172.0). Windows are told apart by length: 300 minutes and 10080 minutes.
pub fn from_codex_session(text: &str, seen_at: u64) -> Option<Limits> {
    text.lines().rev().find_map(|line| {
        if !line.contains("\"rate_limits\"") {
            return None;
        }
        let v: Value = serde_json::from_str(line).ok()?;
        let limits = v.pointer("/payload/rate_limits")?;
        let mut out = Limits {
            five_hour: None,
            weekly: None,
            limited: !limits["rate_limit_reached_type"].is_null(),
            seen_at,
        };
        for key in ["primary", "secondary"] {
            let w = &limits[key];
            let (Some(used), Some(resets), Some(minutes)) = (
                w["used_percent"].as_f64(),
                w["resets_at"].as_i64(),
                w["window_minutes"].as_u64(),
            ) else {
                continue;
            };
            let window = Some(Window {
                used_percent: used,
                resets_at: resets,
            });
            match minutes {
                300 => out.five_hour = window,
                10080 => out.weekly = window,
                _ => {}
            }
        }
        // Codex leaves `rate_limit_reached_type` empty even at 100% (seen with 0.172.0).
        out.limited |= [out.five_hour, out.weekly]
            .iter()
            .flatten()
            .any(|w| w.used_percent >= 100.0);
        (out.five_hour.is_some() || out.weekly.is_some()).then_some(out)
    })
}

/// The session file of Codex thread `thread_id` under `codex_home`
/// (`sessions/YYYY/MM/DD/rollout-*-<thread_id>.jsonl`), newest day first.
pub fn codex_session_file(codex_home: &Path, thread_id: &str) -> Option<std::path::PathBuf> {
    let suffix = format!("-{thread_id}.jsonl");
    let sorted = |dir: &Path| -> Vec<std::path::PathBuf> {
        let mut v: Vec<_> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        v
    };
    for year in sorted(&codex_home.join("sessions")) {
        for month in sorted(&year) {
            for day in sorted(&month) {
                if let Some(file) = sorted(&day).into_iter().find(|f| {
                    f.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(&suffix))
                }) {
                    return Some(file);
                }
            }
        }
    }
    None
}

/// The `/usage` block: one row per CLI agent in `ids`. `now_ms` and `utc_offset_s` make it
/// a pure function of its inputs.
pub fn report(
    ids: &[&str],
    limits: &BTreeMap<String, Limits>,
    now_ms: u64,
    utc_offset_s: i64,
) -> String {
    let local = |s: i64| (s + utc_offset_s).max(0) as u64 * 1000;
    let width = ids.iter().map(|id| id.len()).max().unwrap_or(0);
    let mut rows = vec!["limits:".to_string()];
    for id in ids {
        let Some(l) = limits.get(*id) else {
            rows.push(format!(
                "{id:width$}  no limit data yet (after its first turn)"
            ));
            continue;
        };
        let window = |name: &str, w: Option<Window>| match w {
            None => None,
            Some(w) if w.resets_at * 1000 <= now_ms as i64 => {
                Some(format!("{name} reset since (was {:.0}%)", w.used_percent))
            }
            Some(w) => Some(format!(
                "{name} {:.0}%  resets {}",
                w.used_percent,
                when(local(w.resets_at), local(now_ms as i64 / 1000))
            )),
        };
        let parts: Vec<String> = [window("5h", l.five_hour), window("week", l.weekly)]
            .into_iter()
            .flatten()
            .collect();
        // Only while it still holds: a full window that hasn't reset, or the CLI's own
        // flag with no window reset since it was seen.
        let active: Vec<Window> = [l.five_hour, l.weekly]
            .into_iter()
            .flatten()
            .filter(|w| w.resets_at * 1000 > now_ms as i64)
            .collect();
        let all_active = active.len() == [l.five_hour, l.weekly].iter().flatten().count();
        let limited = if active.iter().any(|w| w.used_percent >= 100.0) || (l.limited && all_active)
        {
            "  LIMITED"
        } else {
            ""
        };
        rows.push(format!(
            "{id:width$}  {}{limited}  (as of {})",
            parts.join(" · "),
            when(local(l.seen_at as i64 / 1000), local(now_ms as i64 / 1000))
        ));
    }
    rows.join("\n")
}

/// For the status line: windows at 80% or more that have not reset yet, e.g. `codex 5h 86%`.
pub fn warnings(limits: &BTreeMap<String, Limits>, now_ms: u64) -> Vec<String> {
    let mut out = vec![];
    for (id, l) in limits {
        for (name, w) in [("5h", l.five_hour), ("week", l.weekly)] {
            if let Some(w) = w
                && w.used_percent >= 80.0
                && w.resets_at * 1000 > now_ms as i64
            {
                out.push(format!("{id} {name} {:.0}%", w.used_percent));
            }
        }
    }
    out
}

/// `HH:MM` today, else `Mon HH:MM` within a week, else the date; both in local milliseconds.
fn when(at_ms: u64, now_ms: u64) -> String {
    let minutes = at_ms / 60_000;
    let clock = format!("{:02}:{:02}", minutes / 60 % 24, minutes % 60);
    let (day, today) = (at_ms / 86_400_000, now_ms / 86_400_000);
    if day == today {
        clock
    } else if day > today && day - today < 7 {
        // 1970-01-01 was a Thursday.
        let name = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][(day % 7) as usize];
        format!("{name} {clock}")
    } else {
        format!("{} {clock}", day_of(at_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 2026-11-25 20:45 UTC, a Wednesday.
    const NOW: u64 = 1_795_639_500_000;

    #[test]
    fn claude_rate_limit_event() {
        let event = json!({"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning",
            "unifiedWindows":{"five_hour":{"utilization":0.82,"resetsAt":1795644000,"status":"allowed"},
            "seven_day":{"utilization":0.32,"resetsAt":1796137200,"status":"allowed"}}}});
        let l = from_claude(&event, NOW).unwrap();
        assert_eq!(l.five_hour.unwrap().resets_at, 1795644000);
        assert!((l.five_hour.unwrap().used_percent - 82.0).abs() < 1e-9);
        assert!((l.weekly.unwrap().used_percent - 32.0).abs() < 1e-9);
        assert!(!l.limited);
        let blocked = json!({"rate_limit_info":{"unifiedWindows":{"five_hour":{"utilization":1.0,"resetsAt":1,"status":"rejected"}}}});
        assert!(from_claude(&blocked, NOW).unwrap().limited);
        assert_eq!(from_claude(&json!({"type":"rate_limit_event"}), NOW), None);
    }

    #[test]
    fn codex_session_lines() {
        let line = |primary: Value| {
            json!({"type":"event_msg","payload":{"type":"token_count","rate_limits":{
                "primary": primary,
                "secondary":{"used_percent":66.0,"window_minutes":10080,"resets_at":1795965214},
                "rate_limit_reached_type": null}}})
            .to_string()
        };
        let text = [
            line(json!({"used_percent":50.0,"window_minutes":300,"resets_at":1795620349})),
            r#"{"type":"event_msg","payload":{"type":"agent_message"}}"#.into(),
            line(json!({"used_percent":86.0,"window_minutes":300,"resets_at":1795640349})),
            // A later line without data (e.g. after the limit message) is skipped.
            json!({"payload":{"rate_limits":{"primary":null,"secondary":null}}}).to_string(),
        ]
        .join("\n");
        let l = from_codex_session(&text, NOW).unwrap();
        assert_eq!(
            l.five_hour,
            Some(Window {
                used_percent: 86.0,
                resets_at: 1795640349
            })
        );
        assert_eq!(l.weekly.unwrap().used_percent, 66.0);
        assert!(!l.limited);
        let full = line(json!({"used_percent":100.0,"window_minutes":300,"resets_at":1}));
        assert!(
            from_codex_session(&full, NOW).unwrap().limited,
            "100% is limited"
        );
        assert_eq!(from_codex_session("{}\nnot json", NOW), None);
    }

    #[test]
    fn finds_the_codex_session_file_by_thread_id() {
        let home = tempfile::tempdir().unwrap();
        let day = home.path().join("sessions/2026/11/25");
        std::fs::create_dir_all(&day).unwrap();
        let file = day.join("rollout-2026-11-25T19-41-33-01a2-abc.jsonl");
        std::fs::write(&file, "").unwrap();
        std::fs::write(day.join("rollout-2026-11-25T19-00-00-other.jsonl"), "").unwrap();
        assert_eq!(codex_session_file(home.path(), "01a2-abc"), Some(file));
        assert_eq!(codex_session_file(home.path(), "missing"), None);
    }

    #[test]
    fn the_report_and_warnings() {
        let mut limits = BTreeMap::new();
        limits.insert(
            "claude".into(),
            Limits {
                // Resets 22:00 today; 32% of the week, resetting Monday.
                five_hour: Some(Window {
                    used_percent: 82.0,
                    resets_at: 1_795_644_000,
                }),
                weekly: Some(Window {
                    used_percent: 32.0,
                    resets_at: 1_796_029_200,
                }),
                limited: false,
                seen_at: NOW - 60_000,
            },
        );
        limits.insert(
            "codex".into(),
            Limits {
                five_hour: Some(Window {
                    used_percent: 100.0,
                    resets_at: 1_795_630_000, // already past
                }),
                weekly: None,
                limited: true,
                seen_at: NOW - 3_600_000,
            },
        );
        insta::assert_snapshot!(report(&["claude", "codex", "llama"], &limits, NOW, 0), @r"
        limits:
        claude  5h 82%  resets 22:00 · week 32%  resets Mon 09:00  (as of 20:44)
        codex   5h reset since (was 100%)  (as of 19:45)
        llama   no limit data yet (after its first turn)
        ");
        assert_eq!(warnings(&limits, NOW), ["claude 5h 82%"]);
        // A full window that hasn't reset yet is LIMITED, and warned about.
        limits.get_mut("codex").unwrap().five_hour = Some(Window {
            used_percent: 100.0,
            resets_at: 1_795_641_000,
        });
        let report = report(&["codex"], &limits, NOW, 0);
        assert!(
            report.contains("5h 100%  resets 21:10  LIMITED"),
            "{report}"
        );
        assert_eq!(warnings(&limits, NOW), ["claude 5h 82%", "codex 5h 100%"]);
    }
}
