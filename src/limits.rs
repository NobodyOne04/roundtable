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
    /// The CLI says this window blocks it now, whatever the percentage (Claude's
    /// `status: rejected`). Full windows block too: see `blocked_until`.
    #[serde(default)]
    pub blocked: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    pub five_hour: Option<Window>,
    pub weekly: Option<Window>,
    /// Unix milliseconds when the CLI reported these.
    pub seen_at: u64,
    /// The CLI said it was limited without saying by which window (Codex's
    /// `rate_limit_reached_type` below 100%): limited until the first window resets, since
    /// any of them may be the cause. Unix seconds.
    #[serde(default)]
    pub limited_until: Option<i64>,
    /// The destination that reported them (the participant's identity, ADR 0011): a
    /// reading never carries over to a repointed id.
    #[serde(default)]
    pub identity: Option<String>,
}

/// Claude's `rate_limit_event` (stream-json, seen with 2.1.332).
pub fn from_claude(event: &Value, seen_at: u64) -> Option<Limits> {
    let windows = event.pointer("/rate_limit_info/unifiedWindows")?;
    let window = |name: &str| {
        let w = windows.get(name)?;
        Some(Window {
            used_percent: w.get("utilization")?.as_f64()? * 100.0,
            resets_at: w.get("resetsAt")?.as_i64().filter(|&r| plausible(r))?,
            blocked: w
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.starts_with("allowed")),
        })
    };
    let (five_hour, weekly) = (window("five_hour"), window("seven_day"));
    // An event without usable windows says nothing: keep the last good reading.
    if five_hour.is_none() && weekly.is_none() {
        return None;
    }
    Some(Limits {
        five_hour,
        weekly,
        seen_at,
        limited_until: None,
        identity: None,
    })
}

/// The last `rate_limits` with data in (the end of) a Codex session file (`token_count`
/// events, seen with 0.172.0). Windows are told apart by length: 300 and 10080 minutes.
/// Its age is the line's own `timestamp`, so an old sample is never passed off as new.
pub fn from_codex_session(text: &str) -> Option<Limits> {
    text.lines().rev().find_map(|line| {
        if !line.contains("\"rate_limits\"") {
            return None;
        }
        let v: Value = serde_json::from_str(line).ok()?;
        let limits = v.pointer("/payload/rate_limits")?;
        let seen_at = iso_ms(v.get("timestamp")?.as_str()?)?;
        let mut out = Limits {
            five_hour: None,
            weekly: None,
            seen_at,
            limited_until: None,
            identity: None,
        };
        let reached = !limits["rate_limit_reached_type"].is_null();
        for key in ["primary", "secondary"] {
            let w = &limits[key];
            let (Some(used), Some(resets), Some(minutes)) = (
                w["used_percent"].as_f64(),
                w["resets_at"].as_i64().filter(|&r| plausible(r)),
                w["window_minutes"].as_u64(),
            ) else {
                continue;
            };
            let window = Some(Window {
                used_percent: used,
                resets_at: resets,
                blocked: false,
            });
            match minutes {
                300 => out.five_hour = window,
                10080 => out.weekly = window,
                _ => {}
            }
        }
        // Codex leaves `rate_limit_reached_type` empty even at 100% (0.172.0), so a full
        // window blocks by itself (`blocked_until`). When it is set without a full window,
        // it doesn't say which one: limited until the first of them resets.
        let full = [out.five_hour, out.weekly]
            .iter()
            .flatten()
            .any(|w| w.used_percent >= 100.0);
        if reached && !full {
            out.limited_until = [out.five_hour, out.weekly]
                .iter()
                .flatten()
                .map(|w| w.resets_at)
                .min();
        }
        (out.five_hour.is_some() || out.weekly.is_some()).then_some(out)
    })
}

/// Unix seconds no later than year 9999: anything else is a malformed reading, dropped.
const LAST_SECOND: i64 = 253_402_300_799;

fn plausible(resets_at: i64) -> bool {
    (0..=LAST_SECOND).contains(&resets_at)
}

/// Whole seconds of a Unix-milliseconds time, for comparing with `resets_at` in seconds
/// (never multiplying it: a malformed reading must not overflow).
fn secs(ms: u64) -> i64 {
    (ms / 1000) as i64
}

/// Unix milliseconds of an RFC 3339 UTC time as Codex writes it (`2026-11-25T15:42:05.550Z`).
fn iso_ms(text: &str) -> Option<u64> {
    let b = text.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[10] != b'T' || !text.ends_with('Z') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| text.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, min, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let valid = (1..=12).contains(&m)
        && (1..=month_days[(m - 1).clamp(0, 11) as usize]).contains(&d)
        && (0..24).contains(&h)
        && (0..60).contains(&min)
        && (0..=60).contains(&s)
        && b[7] == b'-'
        && b[13] == b':'
        && b[16] == b':';
    if !valid {
        return None;
    }
    let millis = match text.get(19..text.len() - 1) {
        Some("") => 0,
        Some(frac) => {
            let digits = frac.strip_prefix('.')?;
            format!("{digits:0<3}").get(..3)?.parse::<i64>().ok()?
        }
        None => return None,
    };
    // Days from civil (Howard Hinnant), the inverse of `day_of`.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(((days * 24 + h) * 60 + min) * 60_000 + s * 1000 + millis).ok()
}

/// Until when (Unix seconds) a full window blocks this CLI, if one still does at `now_ms`.
pub fn blocked_until(l: &Limits, now_ms: u64) -> Option<i64> {
    [l.five_hour, l.weekly]
        .into_iter()
        .flatten()
        .filter(|w| (w.blocked || w.used_percent >= 100.0) && w.resets_at > secs(now_ms))
        .map(|w| w.resets_at)
        .chain(l.limited_until.filter(|&t| t > secs(now_ms)))
        .max()
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

/// The `/usage` block: one row per CLI agent in `ids`. `now_ms` and `offset_at` (the local
/// UTC offset in seconds at a given Unix second, so each time gets its own DST) make it
/// a pure function of its inputs.
pub fn report(
    ids: &[&str],
    limits: &BTreeMap<String, Limits>,
    now_ms: u64,
    offset_at: impl Fn(i64) -> i64,
) -> String {
    let local = |s: i64| {
        let s = s.clamp(0, LAST_SECOND);
        (s + offset_at(s)).max(0) as u64 * 1000
    };
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
            Some(w) if w.resets_at <= secs(now_ms) => {
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
        // Only while it still holds: one rule with the context pane (`blocked_until`).
        let limited = if blocked_until(l, now_ms).is_some() {
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

/// Whether a window of these readings resets in `(after_ms, until_ms]`: what is shown
/// about it (`limited until`, a warning) changes then, so the screen must be redrawn.
pub fn resets_between(limits: &BTreeMap<String, Limits>, after_ms: u64, until_ms: u64) -> bool {
    limits
        .values()
        .flat_map(|l| [l.five_hour, l.weekly])
        .flatten()
        .any(|w| w.resets_at > secs(after_ms) && w.resets_at <= secs(until_ms))
}

/// For the status line: windows at 80% or more that have not reset yet, e.g. `codex 5h 86%`.
pub fn warnings(limits: &BTreeMap<String, Limits>, now_ms: u64) -> Vec<String> {
    let mut out = vec![];
    for (id, l) in limits {
        for (name, w) in [("5h", l.five_hour), ("week", l.weekly)] {
            if let Some(w) = w
                && w.used_percent >= 80.0
                && w.resets_at > secs(now_ms)
            {
                out.push(format!("{id} {name} {:.0}%", w.used_percent));
            }
        }
    }
    out
}

/// `HH:MM` today, else `Mon HH:MM` within a week, else the date; both in local milliseconds.
pub fn when(at_ms: u64, now_ms: u64) -> String {
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
        assert_eq!(blocked_until(&l, NOW), None);
        // Rejected below 100%: blocked all the same, until that window resets.
        let blocked = json!({"rate_limit_info":{"unifiedWindows":{"five_hour":{"utilization":0.9,"resetsAt":1795641000,"status":"rejected"}}}});
        let l = from_claude(&blocked, NOW).unwrap();
        assert_eq!(blocked_until(&l, NOW), Some(1795641000));
        assert!(
            report(
                &["claude"],
                &BTreeMap::from([("claude".into(), l)]),
                NOW,
                |_| 0
            )
            .contains("LIMITED")
        );
        assert_eq!(from_claude(&json!({"type":"rate_limit_event"}), NOW), None);
        let empty = json!({"rate_limit_info":{"unifiedWindows":{}}});
        assert_eq!(
            from_claude(&empty, NOW),
            None,
            "no windows: keep the last reading"
        );
        assert_eq!(
            from_claude(&json!({"rate_limit_info":{"unifiedWindows":null}}), NOW),
            None
        );
    }

    #[test]
    fn codex_session_lines() {
        let line = |primary: Value| {
            json!({"timestamp":"2026-11-25T20:44:00.250Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{
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
        let l = from_codex_session(&text).unwrap();
        assert_eq!(l.seen_at, NOW - 60_000 + 250, "the line's own time");
        assert_eq!(
            l.five_hour,
            Some(Window {
                used_percent: 86.0,
                resets_at: 1795640349,
                blocked: false,
            })
        );
        assert_eq!(l.weekly.unwrap().used_percent, 66.0);
        assert_eq!(blocked_until(&l, NOW), None);
        let reached = json!({"timestamp":"2026-11-25T20:44:00Z","payload":{"rate_limits":{
            "primary":{"used_percent":90.0,"window_minutes":300,"resets_at":1795641000},
            "rate_limit_reached_type":"rate_limit"}}})
        .to_string();
        assert_eq!(
            blocked_until(&from_codex_session(&reached).unwrap(), NOW),
            Some(1795641000),
            "reached below 100%: still limited"
        );
        // Which window it was is unknown: limited until the first one resets, not the week.
        let both = json!({"timestamp":"2026-11-25T20:44:00Z","payload":{"rate_limits":{
            "primary":{"used_percent":90.0,"window_minutes":300,"resets_at":1795641000},
            "secondary":{"used_percent":40.0,"window_minutes":10080,"resets_at":1796137200},
            "rate_limit_reached_type":"rate_limit"}}})
        .to_string();
        let l = from_codex_session(&both).unwrap();
        assert_eq!(blocked_until(&l, NOW), Some(1795641000));
        assert_eq!(
            blocked_until(&l, 1_795_641_001_000),
            None,
            "after the 5h reset: free"
        );
        let full = line(json!({"used_percent":100.0,"window_minutes":300,"resets_at":1}));
        assert!(
            blocked_until(&from_codex_session(&full).unwrap(), 0).is_some(),
            "100% is limited"
        );
        assert_eq!(from_codex_session("{}\nnot json"), None);
    }

    #[test]
    fn recorded_cli_output() {
        // Real lines, sanitized (CLAUDE.md): claude 2.1.332 stream, codex 0.172.0 session.
        let claude: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/claude-2.1.332-rate-limit.jsonl"
        ))
        .unwrap();
        let l = from_claude(&claude, NOW).unwrap();
        assert!((l.five_hour.unwrap().used_percent - 33.0).abs() < 1e-9);
        assert_eq!(l.weekly.unwrap().resets_at, 1_796_137_200);
        assert_eq!(blocked_until(&l, NOW), None);
        let l = from_codex_session(include_str!(
            "../tests/fixtures/codex-0.172.0-session-tail.jsonl"
        ))
        .unwrap();
        assert_eq!(
            (
                l.five_hour.unwrap().used_percent,
                l.weekly.unwrap().used_percent
            ),
            (15.0, 70.0)
        );
        assert_eq!(l.seen_at, iso_ms("2026-11-25T15:44:10.509Z").unwrap());
    }

    #[test]
    fn absurd_reset_times_never_crash() {
        let huge = Limits {
            five_hour: Some(Window {
                used_percent: 100.0,
                resets_at: i64::MAX,
                blocked: true,
            }),
            weekly: Some(Window {
                used_percent: 0.0,
                resets_at: i64::MIN,
                blocked: false,
            }),
            seen_at: u64::MAX,
            limited_until: None,
            identity: None,
        };
        // As if loaded from a damaged limits.json: every reader copes.
        let map = BTreeMap::from([("codex".to_string(), huge.clone())]);
        let _ = report(&["codex"], &map, NOW, |_| 0);
        let _ = warnings(&map, NOW);
        let _ = blocked_until(&huge, NOW);
        let _ = resets_between(&map, 0, u64::MAX);
        // And the parsers never store one.
        let bad = json!({"rate_limit_info":{"unifiedWindows":{"five_hour":{"utilization":0.5,"resetsAt":i64::MAX,"status":"allowed"}}}});
        assert_eq!(from_claude(&bad, NOW), None);
    }

    #[test]
    fn codex_timestamps() {
        assert_eq!(iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_ms("2026-11-25T20:45:00.000Z"), Some(NOW));
        assert_eq!(iso_ms("2026-11-25T20:45:00.5Z"), Some(NOW + 500));
        assert_eq!(iso_ms("2024-02-29T00:00:00Z"), Some(1_709_164_800_000));
        assert_eq!(iso_ms("2026-11-25 20:45:00"), None);
        assert_eq!(iso_ms("garbage-but-long-enough-Z"), None);
        assert_eq!(iso_ms("2026-99-25T15:42:05Z"), None, "no month 99");
        assert_eq!(iso_ms("2026-02-30T00:00:00Z"), None, "no 30 February");
        assert_eq!(iso_ms("2026-11-25T24:00:00Z"), None);
    }

    #[test]
    fn blocked_only_until_the_full_window_resets() {
        let full = |resets_at| Limits {
            five_hour: Some(Window {
                used_percent: 100.0,
                resets_at,
                blocked: false,
            }),
            weekly: None,
            seen_at: 0,
            limited_until: None,
            identity: None,
        };
        assert_eq!(
            blocked_until(&full(1_795_641_000), NOW),
            Some(1_795_641_000)
        );
        assert_eq!(blocked_until(&full(1_795_630_000), NOW), None);
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
                    blocked: false,
                }),
                weekly: Some(Window {
                    used_percent: 32.0,
                    resets_at: 1_796_029_200,
                    blocked: false,
                }),
                seen_at: NOW - 60_000,
                limited_until: None,
                identity: None,
            },
        );
        limits.insert(
            "codex".into(),
            Limits {
                five_hour: Some(Window {
                    used_percent: 100.0,
                    resets_at: 1_795_630_000, // already past
                    blocked: false,
                }),
                weekly: None,
                seen_at: NOW - 3_600_000,
                limited_until: None,
                identity: None,
            },
        );
        insta::assert_snapshot!(report(&["claude", "codex", "llama"], &limits, NOW, |_| 0), @r"
        limits:
        claude  5h 82%  resets 22:00 · week 32%  resets Mon 09:00  (as of 20:44)
        codex   5h reset since (was 100%)  (as of 19:45)
        llama   no limit data yet (after its first turn)
        ");
        assert_eq!(warnings(&limits, NOW), ["claude 5h 82%"]);
        // The claude 5h window resets at 1_795_644_000 s: due in that tick, not before.
        assert!(resets_between(
            &limits,
            1_795_643_999_000,
            1_795_644_000_000
        ));
        assert!(!resets_between(
            &limits,
            1_795_644_000_000,
            1_795_644_001_000
        ));
        // A full window that hasn't reset yet is LIMITED, and warned about.
        limits.get_mut("codex").unwrap().five_hour = Some(Window {
            used_percent: 100.0,
            resets_at: 1_795_641_000,
            blocked: false,
        });
        let report = report(&["codex"], &limits, NOW, |_| 0);
        assert!(
            report.contains("5h 100%  resets 21:10  LIMITED"),
            "{report}"
        );
        assert_eq!(warnings(&limits, NOW), ["claude 5h 82%", "codex 5h 100%"]);
    }
}
