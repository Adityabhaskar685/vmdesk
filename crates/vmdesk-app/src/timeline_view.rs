//! What the timeline tab says about each stored event: a category, a sentence and an
//! optional detail, grouped under day headers; then how a row is drawn.

use std::str::FromStr;

use chrono::{DateTime, Local, Utc};
use gpui::{Div, FontWeight, div, prelude::*, px, rgb};
use serde_json::Value;
use vmdesk_core::{AgentState, SshState};
use vmdesk_db::TimelineEntry;

use crate::human::{day, duration, time_on};
use crate::kit::label;
use crate::theme::{MONO, Tone, color, space, text};

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
    fn color(self) -> u32 {
        match self {
            Self::Process => color::BLUE,
            Self::Container => color::TEAL,
            Self::Tmux => color::PURPLE,
            Self::Git => color::ORANGE,
            Self::Network => color::PINK,
            Self::Status | Self::Gap | Self::Other => color::GREY,
        }
    }
}

/// Part of a sentence; `code` parts (commands, paths, addresses) are set in monospace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub code: bool,
}

pub type Line = Vec<Span>;

fn plain(text: impl Into<String>) -> Span {
    Span {
        text: text.into(),
        code: false,
    }
}

fn code(text: impl Into<String>) -> Span {
    Span {
        text: text.into(),
        code: true,
    }
}

pub fn text_of(line: &[Span]) -> String {
    line.iter().map(|s| s.text.as_str()).collect()
}

#[derive(Debug, PartialEq)]
pub struct Row {
    pub at: DateTime<Local>,
    pub category: Category,
    /// The category's colour, or for a status change the colour of its outcome.
    pub color: u32,
    /// Something stopped (an exit, a close), or nothing was observed: drawn hollow.
    pub ended: bool,
    pub title: Line,
    pub detail: Option<Line>,
    /// `agent`, `ssh`, `laptop`.
    pub source: String,
    /// Identical events in the same minute, shown once.
    pub count: usize,
}

#[derive(Debug, PartialEq)]
pub struct Day {
    pub label: String,
    pub rows: Vec<Row>,
}

/// One stored event, with typed access to its JSON fields.
struct Event<'a> {
    entry: &'a TimelineEntry,
    /// When it happened, in local time.
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

/// A span with no observations. Who missed it decides the wording: the SSH collector
/// (`ssh`), the agent's daemon (`agent:…`), or the laptop, which records the span whose
/// events an agent rotated out before they were synced.
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
    let (title, why) = match source_tag(&e.entry.source) {
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
        (Some(was), Some(now)) if was != now => Some(agent_text(was, now)),
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

fn agent_text(was: AgentState, now: AgentState) -> Text {
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

/// `agent`, `ssh` or `laptop` for the stored source.
fn source_tag(source: &str) -> &str {
    if source.starts_with("agent:") {
        "agent"
    } else {
        source
    }
}

fn row(entry: &TimelineEntry) -> Row {
    let event = Event {
        entry,
        at: entry.ts.with_timezone(&Local),
    };
    let kind = KINDS.iter().find(|k| k.name == entry.kind);
    let (category, ended, text) = match kind {
        Some(k) => (k.category, k.ended, (k.describe)(&event)),
        None => (Category::Other, false, unknown(&event)),
    };
    Row {
        at: event.at,
        category,
        color: text.tone.map_or(category.color(), Tone::color),
        ended,
        title: text.title,
        detail: text.detail,
        source: source_tag(&entry.source).to_owned(),
        count: 1,
    }
}

/// Newest first, as stored: one day per header, identical events in the same minute
/// folded into one row.
pub fn present(entries: &[TimelineEntry], now: DateTime<Local>) -> Vec<Day> {
    let mut days: Vec<Day> = Vec::new();
    for entry in entries {
        let next = row(entry);
        let label = day(next.at, now);
        if days.last().is_none_or(|d| d.label != label) {
            days.push(Day {
                label,
                rows: Vec::new(),
            });
        }
        let rows = &mut days.last_mut().expect("a day was just pushed").rows;
        match rows.last_mut() {
            Some(last) if same_moment(last, &next) => {
                last.count += 1;
                if last.detail != next.detail {
                    last.detail = None;
                }
            }
            _ => rows.push(next),
        }
    }
    days
}

fn same_moment(a: &Row, b: &Row) -> bool {
    a.category == b.category
        && a.title == b.title
        && a.source == b.source
        && a.at.timestamp().div_euclid(60) == b.at.timestamp().div_euclid(60)
}

/// The rows as drawn: a time column, a rail of category dots, the sentence and its source.
pub fn render(days: &[Day]) -> Div {
    if days.is_empty() {
        return div()
            .pt(space::XL)
            .flex()
            .flex_col()
            .items_center()
            .gap(space::XS)
            .child(
                div()
                    .text_size(text::BODY)
                    .text_color(rgb(color::TEXT_SECONDARY))
                    .child("No events yet"),
            )
            .child(label("They appear as snapshots change."));
    }
    div()
        .flex()
        .flex_col()
        .children(days.iter().enumerate().map(|(i, d)| {
            div()
                .flex()
                .flex_col()
                .child(day_header(&d.label, i == 0))
                .children(d.rows.iter().map(row_view))
        }))
}

fn day_header(day: &str, first: bool) -> Div {
    let selector = format!("day: {day}");
    div()
        .debug_selector(|| selector)
        .when(!first, |d| d.mt(space::L))
        .mb(space::XS)
        .h(px(24.))
        .flex()
        .items_center()
        .gap(space::M)
        .child(
            div()
                .text_size(text::META)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(color::TEXT_SECONDARY))
                .child(day.to_owned()),
        )
        .child(div().flex_1().h(px(1.)).bg(rgb(color::BORDER)))
}

fn spans(line: &[Span]) -> impl Iterator<Item = Div> + '_ {
    line.iter().filter(|s| !s.text.is_empty()).map(|s| {
        let d = div().whitespace_nowrap().child(s.text.clone());
        if s.code {
            d.font_family(MONO)
                .text_size(text::META)
                .min_w_0()
                .max_w(px(520.))
                .overflow_hidden()
                .text_ellipsis()
        } else {
            d.flex_shrink_0()
        }
    })
}

fn marker(r: &Row) -> Div {
    let dot = div()
        .size(px(9.))
        .rounded_full()
        .border_2()
        .border_color(rgb(r.color));
    div()
        .w(px(16.))
        .h_full()
        .flex_shrink_0()
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(7.5))
                .w(px(1.))
                .bg(rgb(color::BORDER)),
        )
        .child(dot.bg(rgb(if r.ended { color::BG } else { r.color })))
}

fn row_view(r: &Row) -> Div {
    let title = text_of(&r.title);
    div()
        .debug_selector(|| format!("event: {title}"))
        .h(px(32.))
        .flex()
        .items_center()
        .gap(space::M)
        .child(
            div()
                .w(px(38.))
                .flex_shrink_0()
                .text_size(text::META)
                .text_color(rgb(color::TEXT_MUTED))
                .child(r.at.format("%H:%M").to_string()),
        )
        .child(marker(r))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .flex()
                .items_center()
                .gap(space::S)
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .flex()
                        .items_center()
                        .text_size(text::BODY)
                        .text_color(rgb(color::TEXT))
                        .children(spans(&r.title)),
                )
                .when(r.count > 1, |d| {
                    d.child(
                        div()
                            .flex_shrink_0()
                            .px(px(6.))
                            .rounded_full()
                            .bg(rgb(color::HOVER))
                            .text_size(text::SMALL)
                            .text_color(rgb(color::TEXT_SECONDARY))
                            .child(format!("×{}", r.count)),
                    )
                })
                .when_some(r.detail.as_ref(), |d, detail| {
                    d.child(
                        div()
                            .flex_shrink(20.)
                            .min_w_0()
                            .overflow_hidden()
                            .flex()
                            .items_center()
                            .text_size(text::META)
                            .text_color(rgb(color::TEXT_MUTED))
                            .children(spans(detail)),
                    )
                }),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(text::SMALL)
                .text_color(rgb(color::TEXT_MUTED))
                .child(r.source.clone()),
        )
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

    /// Day headers as `# Today`, rows as `title | detail`.
    fn shown(days: &[Day]) -> Vec<String> {
        days.iter()
            .flat_map(|d| {
                std::iter::once(format!("# {}", d.label)).chain(d.rows.iter().map(|r| {
                    match &r.detail {
                        Some(detail) => format!("{} | {}", text_of(&r.title), text_of(detail)),
                        None => text_of(&r.title),
                    }
                }))
            })
            .collect()
    }

    fn status(was_ssh: &str, ssh: &str, was_agent: &str, agent: &str, route: &str) -> Value {
        json!({
            "ssh": ssh, "agent": agent, "route": route, "detail": null,
            "previous": { "ssh": was_ssh, "agent": was_agent, "route": route },
        })
    }

    #[test]
    fn events_read_as_sentences_under_their_day() {
        let now = Local.with_ymd_and_hms(2026, 10, 19, 18, 0, 0).unwrap();
        let (today, yesterday) = (now - Duration::hours(1), now - Duration::days(1));
        let entries = [
            ("laptop", "status_changed", today, status("ok", "ok", "unknown", "ok", "tun0")),
            (
                "agent:01a1",
                "process_started",
                today,
                json!({ "pid": 7, "cmdline": "python3 -m http.server 18766", "cwd": "/tmp" }),
            ),
            (
                "agent:01a1",
                "process_exited",
                today,
                json!({ "pid": 7, "cmdline": "python3 -m http.server", "duration_secs": 70 }),
            ),
            (
                "ssh",
                "git_committed",
                yesterday,
                json!({ "repo": "/src/app", "branch": "main", "sha": "abcdef123", "subject": "Fix it" }),
            ),
            (
                "ssh",
                "listener_opened",
                yesterday,
                json!({ "proto": "tcp", "addr": "0.0.0.0", "port": 18766, "process": "python3" }),
            ),
            ("laptop", "status_changed", yesterday, status("ok", "timeout", "unknown", "unknown", "wlo1")),
            ("laptop", "status_changed", yesterday, status("timeout", "ok", "unknown", "unknown", "tun0")),
        ]
        .map(|(source, kind, ts, data)| entry(source, kind, ts, data));
        assert_eq!(
            shown(&present(&entries, now)),
            [
                "# Today",
                "Agent connected and recording",
                "Started python3 -m http.server 18766 | in /tmp",
                "Exited python3 -m http.server | ran 1m 10s",
                "# Yesterday",
                "Committed on main: Fix it | /src/app · abcdef1",
                "Port 18766 opened by python3 | tcp 0.0.0.0:18766",
                "SSH lost (timeout) via wlo1",
                "SSH back via tun0",
            ]
        );
    }

    #[test]
    fn gaps_say_how_long_and_unknown_kinds_still_read() {
        let now = Local.with_ymd_and_hms(2026, 10, 19, 18, 0, 0).unwrap();
        let from = now - Duration::days(4);
        let to = from + Duration::minutes(5 * 60 + 32);
        let span = json!({ "from": from.to_rfc3339(), "to": to.to_rfc3339() });
        let entries = [
            entry("ssh", "gap", from, span.clone()),
            entry("agent:01a1", "gap", from, span.clone()),
            entry("laptop", "gap", from, span),
            entry(
                "ssh",
                "brand_new_kind",
                from,
                json!({ "name": "x", "n": 2 }),
            ),
        ];
        let between = format!("{} – {}", from.format("%H:%M"), to.format("%H:%M"));
        assert_eq!(
            shown(&present(&entries, now)),
            [
                "# Thu 15 Oct".to_owned(),
                format!("Not observed for 5h 32m | {between}"),
                format!("Agent was not recording for 5h 32m | {between}"),
                format!(
                    "Agent history lost for 5h 32m | {between} · the agent dropped these events \
                    before they were synced"
                ),
                "Brand new kind | n 2 · name x".to_owned(),
            ]
        );
    }
    #[test]
    fn identical_events_in_one_minute_fold_into_one_row() {
        let now = Local.with_ymd_and_hms(2026, 10, 19, 18, 0, 0).unwrap();
        let exited = |secs: i64| {
            entry(
                "agent:01a1",
                "process_exited",
                now,
                json!({ "cmdline": "/lib/systemd/systemd-udevd", "duration_secs": secs }),
            )
        };
        let days = present(&[exited(586_707), exited(586_707), exited(578_412)], now);
        let rows = &days[0].rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].count, 3);
        assert_eq!(
            rows[0].detail, None,
            "the folded rows ran for different times"
        );
        assert!(rows[0].ended);
        assert_eq!(rows[0].source, "agent");
    }

    #[test]
    fn each_agent_transition_has_its_own_sentence() {
        use AgentState::{Absent, Error, NotRunning, Ok, Outdated, Unknown};
        let cases = [
            (Unknown, Ok, "Agent connected and recording"),
            (Absent, Ok, "Agent installed and recording"),
            (NotRunning, Ok, "Agent recording again"),
            (Ok, NotRunning, "Agent stopped recording"),
            (Ok, Outdated, "Agent outdated"),
            (Ok, Absent, "Agent gone from the VM"),
            (Ok, Error, "Agent not answering properly"),
        ];
        for (was, now, title) in cases {
            assert_eq!(
                text_of(&agent_text(was, now).title),
                title,
                "{was} -> {now}"
            );
        }
    }
}
