//! What a stored event says, in words: its category, a sentence and an optional detail.
//! The app draws it on the Timeline and in search results; the CLI prints it.

use std::str::FromStr;

use chrono::{DateTime, Local, Utc};
use serde_json::Value;
use vmdesk_core::human::{duration, time_on};
use vmdesk_core::{AgentState, SshState};

use crate::TimelineEntry;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Process,
    Container,
    Tmux,
    Git,
    Network,
    Status,
    Gap,
    /// A kind this build does not know.
    Other,
}

impl Category {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Container => "container",
            Self::Tmux => "tmux",
            Self::Git => "git",
            Self::Network => "network",
            Self::Status => "status",
            Self::Gap => "gap",
            Self::Other => "other",
        }
    }
}

/// What a status means to the user, whatever it is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Good,
    Warn,
    Bad,
    Neutral,
}

/// Part of a sentence; `code` parts (commands, paths, addresses) are set in monospace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub code: bool,
}

pub type Line = Vec<Span>;

pub fn plain(text: impl Into<String>) -> Span {
    Span {
        text: text.into(),
        code: false,
    }
}

pub fn code(text: impl Into<String>) -> Span {
    Span {
        text: text.into(),
        code: true,
    }
}

/// The line as plain text, code parts as they are.
#[must_use]
pub fn text_of(line: &[Span]) -> String {
    line.iter().map(|s| s.text.as_str()).collect()
}

/// One event in words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventText {
    pub category: Category,
    /// Something stopped (an exit, a close), or nothing was observed.
    pub ended: bool,
    /// What the event means, for status changes, reboots and gaps.
    pub tone: Option<Tone>,
    pub title: Line,
    pub detail: Option<Line>,
}

impl TimelineEntry {
    /// The event in words, with times in the laptop's time zone.
    #[must_use]
    pub fn text(&self) -> EventText {
        let event = Event {
            entry: self,
            at: self.ts.with_timezone(&Local),
        };
        let (category, ended, text) = match KINDS.iter().find(|k| k.name == self.kind) {
            Some(k) => (k.category, k.ended, (k.describe)(&event)),
            None => (Category::Other, false, unknown(&event)),
        };
        EventText {
            category,
            ended,
            tone: text.tone,
            title: text.title,
            detail: text.detail,
        }
    }

    /// `agent`, `ssh` or `laptop`: who recorded it, without the agent's id.
    #[must_use]
    pub fn source_tag(&self) -> &str {
        if self.source.starts_with("agent:") {
            "agent"
        } else {
            &self.source
        }
    }
}

/// One stored event, with typed access to its JSON fields.
struct Event<'a> {
    entry: &'a TimelineEntry,
    at: DateTime<Local>,
}

impl Event<'_> {
    fn str(&self, key: &str) -> &str {
        self.opt(key).unwrap_or("")
    }

    fn opt(&self, key: &str) -> Option<&str> {
        self.entry
            .data
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    }

    fn int(&self, key: &str) -> Option<i64> {
        self.entry.data.get(key).and_then(Value::as_i64)
    }

    fn time(&self, key: &str) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(self.opt(key)?)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    }
}

struct Text {
    title: Line,
    detail: Option<Line>,
    tone: Option<Tone>,
}

impl Text {
    fn new(title: Line) -> Self {
        Self {
            title,
            detail: None,
            tone: None,
        }
    }

    fn detail(mut self, detail: Option<Line>) -> Self {
        self.detail = detail;
        self
    }

    fn tone(mut self, tone: Tone) -> Self {
        self.tone = Some(tone);
        self
    }
}

struct Kind {
    name: &'static str,
    category: Category,
    ended: bool,
    describe: fn(&Event) -> Text,
}

/// Every event kind the collectors write (`vmdesk_proto::EventKind`, plus the laptop's
/// `status_changed`).
const KINDS: &[Kind] = &[
    Kind {
        name: "process_started",
        category: Category::Process,
        ended: false,
        describe: |e| {
            Text::new(vec![plain("Started "), code(e.str("cmdline"))])
                .detail(e.opt("cwd").map(|cwd| vec![plain("in "), code(cwd)]))
        },
    },
    Kind {
        name: "process_exited",
        category: Category::Process,
        ended: true,
        describe: |e| {
            let ran = e
                .int("duration_secs")
                .map(|s| format!("ran {}", duration(s)));
            let exit = e
                .int("exit_code")
                .filter(|c| *c != 0)
                .map(|c| format!("exit code {c}"));
            let detail: Vec<String> = ran.into_iter().chain(exit).collect();
            Text::new(vec![plain("Exited "), code(e.str("cmdline"))])
                .detail((!detail.is_empty()).then(|| vec![plain(detail.join(" · "))]))
        },
    },
    Kind {
        name: "container_started",
        category: Category::Container,
        ended: false,
        describe: |e| {
            let project = e.opt("compose_project").map(|p| format!(" · compose {p}"));
            Text::new(vec![
                plain("Container "),
                code(e.str("name")),
                plain(" started"),
            ])
            .detail(Some(vec![
                code(e.str("image")),
                plain(project.unwrap_or_default()),
            ]))
        },
    },
    Kind {
        name: "container_stopped",
        category: Category::Container,
        ended: true,
        describe: |e| {
            Text::new(vec![
                plain("Container "),
                code(e.str("name")),
                plain(" stopped"),
            ])
            .detail(Some(vec![code(e.str("image"))]))
        },
    },
    Kind {
        name: "tmux_created",
        category: Category::Tmux,
        ended: false,
        describe: |e| {
            Text::new(vec![
                plain("tmux session "),
                code(e.str("name")),
                plain(" created"),
            ])
            .detail(e.opt("pane_cwd").map(|cwd| vec![plain("in "), code(cwd)]))
        },
    },
    Kind {
        name: "tmux_closed",
        category: Category::Tmux,
        ended: true,
        describe: |e| {
            Text::new(vec![
                plain("tmux session "),
                code(e.str("name")),
                plain(" closed"),
            ])
        },
    },
    Kind {
        name: "tmux_attached",
        category: Category::Tmux,
        ended: false,
        describe: |e| {
            Text::new(vec![
                plain("Attached to tmux session "),
                code(e.str("name")),
            ])
        },
    },
    Kind {
        name: "git_branch_changed",
        category: Category::Git,
        ended: false,
        describe: |e| {
            Text::new(vec![
                plain("Switched to "),
                code(e.opt("to").unwrap_or("a detached HEAD")),
            ])
            .detail(Some(vec![
                code(e.str("repo")),
                plain(
                    e.opt("from")
                        .map(|f| format!(" · was {f}"))
                        .unwrap_or_default(),
                ),
            ]))
        },
    },
    Kind {
        name: "git_committed",
        category: Category::Git,
        ended: false,
        describe: |e| {
            let on = e
                .opt("branch")
                .map(|b| format!(" on {b}"))
                .unwrap_or_default();
            let sha: String = e.str("sha").chars().take(7).collect();
            Text::new(vec![plain(format!("Committed{on}: {}", e.str("subject")))])
                .detail(Some(vec![code(e.str("repo")), plain(" · "), code(sha)]))
        },
    },
    Kind {
        name: "git_dirty_changed",
        category: Category::Git,
        ended: false,
        describe: |e| {
            let (from, to) = (e.int("from").unwrap_or(0), e.int("to").unwrap_or(0));
            let title = match to {
                0 => "No uncommitted changes".to_owned(),
                1 => "1 uncommitted change".to_owned(),
                n => format!("{n} uncommitted changes"),
            };
            Text::new(vec![plain(title)]).detail(Some(vec![
                plain("in "),
                code(e.str("repo")),
                plain(format!(" · was {from}")),
            ]))
        },
    },
    Kind {
        name: "listener_opened",
        category: Category::Network,
        ended: false,
        describe: |e| {
            let by = e
                .opt("process")
                .map(|p| format!(" by {p}"))
                .unwrap_or_default();
            Text::new(vec![plain(format!(
                "Port {} opened{by}",
                e.int("port").unwrap_or(0)
            ))])
            .detail(Some(listener_address(e)))
        },
    },
    Kind {
        name: "listener_closed",
        category: Category::Network,
        ended: true,
        describe: |e| {
            Text::new(vec![plain(format!(
                "Port {} closed",
                e.int("port").unwrap_or(0)
            ))])
            .detail(Some(listener_address(e)))
        },
    },
    Kind {
        name: "vm_rebooted",
        category: Category::Status,
        ended: false,
        describe: |e| {
            let booted = e
                .int("previous_boot")
                .and_then(|t| DateTime::from_timestamp(t, 0))
                .map(|t| {
                    vec![plain(format!(
                        "previous boot {}",
                        time_on(t.with_timezone(&Local), e.at)
                    ))]
                });
            Text::new(vec![plain("VM rebooted")])
                .detail(booted)
                .tone(Tone::Warn)
        },
    },
    Kind {
        name: "gap",
        category: Category::Gap,
        ended: true,
        describe: gap,
    },
    Kind {
        name: "status_changed",
        category: Category::Status,
        ended: false,
        describe: status_changed,
    },
];

fn listener_address(e: &Event) -> Line {
    vec![
        plain(format!("{} ", e.opt("proto").unwrap_or("tcp"))),
        code(format!("{}:{}", e.str("addr"), e.int("port").unwrap_or(0))),
    ]
}

/// A span with no observations.
fn gap(e: &Event) -> Text {
    let span = match (e.time("from"), e.time("to")) {
        (Some(from), Some(to)) => Some((from, to)),
        _ => None,
    };
    let length = span.map_or_else(
        || "a while".to_owned(),
        |(from, to)| duration((to - from).num_seconds()),
    );
    let between = span.map(|(from, to)| {
        let from = from.with_timezone(&Local);
        format!(
            "{} – {}",
            time_on(from, e.at),
            time_on(to.with_timezone(&Local), from)
        )
    });
    let (title, why) = match e.entry.source_tag() {
        "agent" => (format!("Agent was not recording for {length}"), None),
        "laptop" => (
            format!("Agent history lost for {length}"),
            Some("the agent dropped these events before they were synced"),
        ),
        _ => (format!("Not observed for {length}"), None),
    };
    let detail = [between.as_deref(), why]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    Text::new(vec![plain(title)])
        .detail((!detail.is_empty()).then(|| vec![plain(detail)]))
        .tone(Tone::Neutral)
}

/// The laptop's record of a change in route, SSH or agent state.
fn status_changed(e: &Event) -> Text {
    let data = &e.entry.data;
    let field = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let ssh = |v: &Value| field(v, "ssh").and_then(|s| SshState::from_str(&s).ok());
    let agent = |v: &Value| field(v, "agent").and_then(|s| AgentState::from_str(&s).ok());
    let previous = &data["previous"];
    let route = e.opt("route");

    let ssh_change = match (ssh(previous), ssh(data)) {
        (Some(was), Some(now)) if was != now => Some(ssh_text(was, now, route, e.opt("detail"))),
        _ => None,
    };
    let agent_change = match (agent(previous), agent(data)) {
        (Some(was), Some(now)) if was != now => Some(agent_text(was, now, e.opt("agent_detail"))),
        _ => None,
    };
    match (ssh_change, agent_change) {
        (Some(ssh), Some(agent)) => Text {
            detail: Some(agent.title),
            ..ssh
        },
        (Some(change), None) | (None, Some(change)) => change,
        (None, None) => {
            let was = field(previous, "route");
            Text::new(vec![plain(match route {
                Some(r) => format!("Route changed to {r}"),
                None => "Route lost".to_owned(),
            })])
            .detail(was.map(|w| vec![plain(format!("was {w}"))]))
            .tone(Tone::Neutral)
        }
    }
}

fn ssh_text(was: SshState, now: SshState, route: Option<&str>, why: Option<&str>) -> Text {
    let via = route.map(|r| format!(" via {r}")).unwrap_or_default();
    let reason = match now {
        SshState::Ok | SshState::NotChecked => "",
        SshState::AuthFailed => "login refused",
        SshState::HostKey => "host key not trusted",
        SshState::Refused => "connection refused",
        SshState::Timeout => "timeout",
        SshState::NoRoute => "no route",
        SshState::Error => "error",
    };
    let (title, tone) = match (was, now) {
        (SshState::NotChecked, SshState::Ok) => (format!("SSH reachable{via}"), Tone::Good),
        (_, SshState::Ok) => (format!("SSH back{via}"), Tone::Good),
        (_, SshState::NotChecked) => ("SSH not checked".to_owned(), Tone::Neutral),
        (_, SshState::NoRoute) => ("No route to the VM".to_owned(), Tone::Bad),
        (SshState::Ok | SshState::NotChecked, _) => {
            (format!("SSH lost ({reason}){via}"), Tone::Warn)
        }
        _ => (format!("SSH failing ({reason}){via}"), Tone::Warn),
    };
    Text::new(vec![plain(title)])
        .detail(why.map(|w| vec![plain(w)]))
        .tone(tone)
}

/// `why` is the reason a failed sync gave.
fn agent_text(was: AgentState, now: AgentState, why: Option<&str>) -> Text {
    let (title, detail, tone) = match (was, now) {
        (AgentState::Unknown, AgentState::Ok) => {
            ("Agent connected and recording", None, Tone::Good)
        }
        (AgentState::Absent, AgentState::Ok) => ("Agent installed and recording", None, Tone::Good),
        (AgentState::Outdated, AgentState::Ok) => ("Agent upgraded", None, Tone::Good),
        (AgentState::NotRunning, AgentState::Ok) => ("Agent recording again", None, Tone::Good),
        (_, AgentState::Ok) => ("Agent connected", None, Tone::Good),
        (_, AgentState::Outdated) => (
            "Agent outdated",
            Some("it still records; Upgrade agent installs this laptop's build"),
            Tone::Warn,
        ),
        (_, AgentState::NotRunning) => (
            "Agent stopped recording",
            Some("its daemon is not running; the next poll starts it"),
            Tone::Warn,
        ),
        (_, AgentState::Absent) => (
            "Agent gone from the VM",
            Some("collecting over SSH"),
            Tone::Neutral,
        ),
        (_, AgentState::Error) => (
            "Agent not answering properly",
            Some("collecting over SSH"),
            Tone::Bad,
        ),
        (_, AgentState::Unknown) => ("Agent state unknown", None, Tone::Neutral),
    };
    let detail = detail.map(|d| match why.filter(|_| now == AgentState::Error) {
        Some(why) => format!("{why} · {d}"),
        None => d.to_owned(),
    });
    Text::new(vec![plain(title)])
        .detail(detail.map(|d| vec![plain(d)]))
        .tone(tone)
}

/// A kind this build does not know: its name as a sentence and its plain fields.
fn unknown(e: &Event) -> Text {
    let mut name = e.entry.kind.replace('_', " ");
    if let Some(first) = name.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    let mut fields: Vec<String> = e
        .entry
        .data
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| match v {
            Value::String(s) => Some(format!("{k} {s}")),
            Value::Number(n) => Some(format!("{k} {n}")),
            Value::Bool(b) => Some(format!("{k} {b}")),
            _ => None,
        })
        .collect();
    fields.sort();
    Text::new(vec![plain(name)])
        .detail((!fields.is_empty()).then(|| vec![plain(fields.join(" · "))]))
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};
    use serde_json::json;

    use super::*;

    fn entry(source: &str, kind: &str, ts: DateTime<Local>, data: Value) -> TimelineEntry {
        TimelineEntry {
            source: source.into(),
            seq: 0,
            ts: ts.with_timezone(&Utc),
            kind: kind.into(),
            data,
        }
    }

    /// `title | detail`, as plain text.
    fn said(e: &TimelineEntry) -> String {
        let t = e.text();
        match &t.detail {
            Some(detail) => format!("{} | {}", text_of(&t.title), text_of(detail)),
            None => text_of(&t.title),
        }
    }

    fn status(was_ssh: &str, ssh: &str, was_agent: &str, agent: &str, route: &str) -> Value {
        json!({
            "ssh": ssh, "agent": agent, "route": route, "detail": null,
            "previous": { "ssh": was_ssh, "agent": was_agent, "route": route },
        })
    }

    fn at() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 19, 17, 0, 0).unwrap()
    }

    #[test]
    #[allow(clippy::too_many_lines)] // one case per kind
    fn every_kind_reads_as_a_sentence() {
        let t = at();
        let cases = [
            (
                "process_started",
                json!({ "pid": 7, "cmdline": "python3 -m http.server 18766", "cwd": "/tmp" }),
                Category::Process,
                "Started python3 -m http.server 18766 | in /tmp",
            ),
            (
                "process_exited",
                json!({ "pid": 7, "cmdline": "make", "duration_secs": 70, "exit_code": 2 }),
                Category::Process,
                "Exited make | ran 1m 10s · exit code 2",
            ),
            (
                "container_started",
                json!({ "name": "db", "image": "postgres:16", "compose_project": "inv" }),
                Category::Container,
                "Container db started | postgres:16 · compose inv",
            ),
            (
                "container_stopped",
                json!({ "name": "db", "image": "postgres:16" }),
                Category::Container,
                "Container db stopped | postgres:16",
            ),
            (
                "tmux_created",
                json!({ "name": "fix", "pane_cwd": "/src" }),
                Category::Tmux,
                "tmux session fix created | in /src",
            ),
            (
                "tmux_closed",
                json!({ "name": "fix" }),
                Category::Tmux,
                "tmux session fix closed",
            ),
            (
                "tmux_attached",
                json!({ "name": "fix" }),
                Category::Tmux,
                "Attached to tmux session fix",
            ),
            (
                "git_branch_changed",
                json!({ "repo": "/src/app", "from": "main", "to": "fix/x" }),
                Category::Git,
                "Switched to fix/x | /src/app · was main",
            ),
            (
                "git_committed",
                json!({ "repo": "/src/app", "branch": "main", "sha": "abcdef123", "subject": "Fix it" }),
                Category::Git,
                "Committed on main: Fix it | /src/app · abcdef1",
            ),
            (
                "git_dirty_changed",
                json!({ "repo": "/src/app", "from": 0, "to": 3 }),
                Category::Git,
                "3 uncommitted changes | in /src/app · was 0",
            ),
            (
                "listener_opened",
                json!({ "proto": "tcp", "addr": "0.0.0.0", "port": 18766, "process": "python3" }),
                Category::Network,
                "Port 18766 opened by python3 | tcp 0.0.0.0:18766",
            ),
            (
                "listener_closed",
                json!({ "proto": "udp", "addr": "127.0.0.1", "port": 53 }),
                Category::Network,
                "Port 53 closed | udp 127.0.0.1:53",
            ),
            (
                "vm_rebooted",
                json!({ "previous_boot": (t - Duration::hours(3)).timestamp() }),
                Category::Status,
                "VM rebooted | previous boot 14:00",
            ),
            (
                "status_changed",
                status("ok", "timeout", "unknown", "unknown", "wlo1"),
                Category::Status,
                "SSH lost (timeout) via wlo1",
            ),
            (
                "brand_new_kind",
                json!({ "name": "x", "n": 2, "nested": { "a": 1 } }),
                Category::Other,
                "Brand new kind | n 2 · name x",
            ),
        ];
        for (kind, data, category, text) in cases {
            let e = entry("ssh", kind, t, data);
            assert_eq!(
                (e.text().category, said(&e).as_str()),
                (category, text),
                "{kind}"
            );
        }
    }

    #[test]
    fn ssh_and_agent_changes_say_what_happened() {
        let t = at();
        let said_status = |data| said(&entry("laptop", "status_changed", t, data));
        assert_eq!(
            said_status(status("timeout", "ok", "unknown", "unknown", "tun0")),
            "SSH back via tun0"
        );
        assert_eq!(
            said_status(status("ok", "ok", "unknown", "ok", "tun0")),
            "Agent connected and recording"
        );
        assert_eq!(
            said_status(status("ok", "auth_failed", "ok", "unknown", "tun0")),
            "SSH lost (login refused) via tun0 | Agent state unknown",
            "both changed: the SSH change, then the agent's"
        );
        let mut why = status("ok", "timeout", "unknown", "unknown", "tun0");
        why["detail"] = json!("timed out: lab: 192.0.2.40:22");
        assert_eq!(
            said_status(why),
            "SSH lost (timeout) via tun0 | timed out: lab: 192.0.2.40:22"
        );
        let mut moved = status("ok", "ok", "ok", "ok", "tun0");
        moved["previous"]["route"] = json!("wlo1");
        assert_eq!(said_status(moved), "Route changed to tun0 | was wlo1");

        let mut failed = status("ok", "ok", "ok", "error", "tun0");
        assert_eq!(
            said_status(failed.clone()),
            "Agent not answering properly | collecting over SSH"
        );
        failed["agent_detail"] = json!("timed out waiting for the agent");
        assert_eq!(
            said_status(failed),
            "Agent not answering properly | timed out waiting for the agent · collecting over SSH",
            "with the reason the sync stored"
        );
    }

    #[test]
    fn each_agent_transition_has_its_own_sentence() {
        use AgentState::{Absent, Error, NotRunning, Ok, Outdated, Unknown};
        let cases = [
            (Unknown, Ok, "Agent connected and recording", Tone::Good),
            (Absent, Ok, "Agent installed and recording", Tone::Good),
            (Outdated, Ok, "Agent upgraded", Tone::Good),
            (NotRunning, Ok, "Agent recording again", Tone::Good),
            (Error, Ok, "Agent connected", Tone::Good),
            (Ok, NotRunning, "Agent stopped recording", Tone::Warn),
            (Ok, Outdated, "Agent outdated", Tone::Warn),
            (Ok, Absent, "Agent gone from the VM", Tone::Neutral),
            (Ok, Error, "Agent not answering properly", Tone::Bad),
            (Ok, Unknown, "Agent state unknown", Tone::Neutral),
        ];
        for (was, now, title, tone) in cases {
            let e = entry(
                "laptop",
                "status_changed",
                at(),
                status("ok", "ok", was.as_str(), now.as_str(), "tun0"),
            );
            let t = e.text();
            assert_eq!(
                (text_of(&t.title).as_str(), t.tone),
                (title, Some(tone)),
                "{was} -> {now}"
            );
        }
    }

    #[test]
    fn gaps_say_how_long_and_who_missed_it() {
        let from = at() - Duration::days(4);
        let to = from + Duration::minutes(5 * 60 + 32);
        let span = json!({ "from": from.to_rfc3339(), "to": to.to_rfc3339() });
        let between = format!("{} – {}", from.format("%H:%M"), to.format("%H:%M"));
        let cases = [
            ("ssh", format!("Not observed for 5h 32m | {between}")),
            (
                "agent:01a1",
                format!("Agent was not recording for 5h 32m | {between}"),
            ),
            (
                "laptop",
                format!(
                    "Agent history lost for 5h 32m | {between} · the agent dropped these \
                    events before they were synced"
                ),
            ),
        ];
        for (source, text) in cases {
            let e = entry(source, "gap", from, span.clone());
            assert_eq!(said(&e), text, "{source}");
            assert_eq!((e.text().category, e.text().ended), (Category::Gap, true));
        }
        let later = entry("ssh", "gap", at(), span);
        assert!(
            said(&later).ends_with(&format!(
                "| Thu 15 Oct {} – {}",
                from.format("%H:%M"),
                to.format("%H:%M")
            )),
            "a gap from another day says which: {}",
            said(&later)
        );
        assert_eq!(
            said(&entry("ssh", "gap", at(), json!({}))),
            "Not observed for a while"
        );
    }

    #[test]
    fn no_sentence_carries_an_iso_timestamp() {
        let from = at() - Duration::hours(30);
        let iso = json!({ "from": from.to_rfc3339(), "to": at().to_rfc3339() });
        let texts = [
            entry("ssh", "gap", at(), iso.clone()),
            entry("laptop", "gap", at(), iso),
            entry(
                "ssh",
                "vm_rebooted",
                at(),
                json!({ "previous_boot": from.timestamp() }),
            ),
        ]
        .map(|e| said(&e));
        let iso_date = |w: &[char]| {
            w.iter().enumerate().all(|(i, c)| {
                if i == 4 || i == 7 {
                    *c == '-'
                } else {
                    c.is_ascii_digit()
                }
            })
        };
        for text in texts {
            let chars: Vec<char> = text.chars().collect();
            assert!(!chars.windows(10).any(iso_date), "{text}");
        }
    }

    #[test]
    fn sources_are_tagged_without_the_agent_id() {
        let tags = ["agent:01a1", "ssh", "laptop"]
            .map(|s| entry(s, "gap", at(), json!({})).source_tag().to_owned());
        assert_eq!(tags, ["agent", "ssh", "laptop"]);
    }
}
