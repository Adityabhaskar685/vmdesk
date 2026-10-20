//! Your work on a VM: the current-work card at the top of the Overview and the Sessions
//! tab, each with its notes. First what they say (typed view models, built from the
//! VM's detail), then how they are drawn.

use std::collections::HashMap;

use chrono::{DateTime, Local, Utc};
use gpui::{
    Context, Div, ElementId, Entity, FontWeight, SharedString, Stateful, div, prelude::*, px, rgb,
};
use vmdesk_collect::work::{Anchor, CurrentWork};
use vmdesk_db::{Note, NoteId, SessionId, WorkSession};

use crate::field::TextField;
use crate::human::{day, duration, time_on};
use crate::icon::{Icon, icon};
use crate::kit::{Button, button, card, label, pill};
use crate::theme::{MONO, Tone, color, radius, space, text};
use crate::timeline_view::{Line, Span, code, day_header, plain, text_of};
use crate::ui::AppView;

/// Commands listed per session, most runs first.
const COMMANDS_SHOWN: usize = 3;
/// Containers named on the card; the rest are counted.
const CONTAINERS_SHOWN: usize = 3;

/// The current-work card: "what was I doing here?".
#[derive(Debug, PartialEq)]
pub struct CurrentWorkView {
    pub doing: Doing,
    /// The newest note on the latest session, shown above what you were doing.
    pub note: Option<NoteRow>,
    /// The VM's latest session.
    pub session: Option<SessionRow>,
}

#[derive(Debug, PartialEq)]
pub enum Doing {
    /// No snapshot yet.
    Unknown,
    /// The latest snapshot shows no command or tmux session to anchor on.
    Nothing,
    /// The anchor (a command or a tmux session), then where and since when:
    /// `regal @ group2-fix`, `3 modified`, `tmux nightly-debug`, `since 21:07`.
    Work { what: Line, facts: Vec<Line> },
}

/// One work session, as a row of the Sessions tab and the card's session line.
#[derive(Debug, PartialEq)]
pub struct SessionRow {
    pub id: SessionId,
    pub title: Option<String>,
    /// `16:40` today, `yesterday 16:40`, else `Mon 19 Oct 16:40`.
    pub started: String,
    /// The start's time of day, under the day header that gives its day.
    pub start_time: String,
    pub end: End,
    /// From the first activity to the last; `None` for a single moment.
    pub duration: Option<String>,
    /// `regal @ group2-fix, main`.
    pub repos: Vec<Line>,
    /// `pytest tests/group2 ×3`, most runs first.
    pub commands: Vec<String>,
    /// Newest first.
    pub notes: Vec<NoteRow>,
}

#[derive(Debug, PartialEq)]
pub enum End {
    /// More activity now would still extend the session.
    Ongoing,
    /// All its activity was at one moment.
    Moment,
    /// Its last activity: `17:35`, or with its day when that was another day.
    At(String),
}

impl SessionRow {
    /// `16:40 – 17:35`, `16:40 – now` or `16:40`, for the Sessions tab.
    pub fn span(&self) -> String {
        match &self.end {
            End::Ongoing => format!("{} – now", self.start_time),
            End::Moment => self.start_time.clone(),
            End::At(end) => format!("{} – {end}", self.start_time),
        }
    }

    pub fn ongoing(&self) -> bool {
        self.end == End::Ongoing
    }

    /// For the card: `started 16:40 · 55m · 3 repos` while ongoing, else
    /// `yesterday 16:40 – 17:38 · 58m · 1 repo`.
    pub fn brief(&self) -> String {
        let mut parts = vec![match &self.end {
            End::Ongoing => format!("started {}", self.started),
            End::Moment => self.started.clone(),
            End::At(end) => format!("{} – {end}", self.started),
        }];
        parts.extend(self.duration.clone());
        match self.repos.len() {
            0 => {}
            1 => parts.push("1 repo".to_owned()),
            n => parts.push(format!("{n} repos")),
        }
        parts.join(" · ")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NoteRow {
    pub id: NoteId,
    pub body: String,
    /// When it was written: `16:52`, else with its day.
    pub written: String,
}

/// The Sessions tab: sessions under the day each started, newest first.
#[derive(Debug, PartialEq)]
pub struct SessionsView {
    pub days: Vec<SessionDay>,
    /// Notes on no session shown here: written before the VM had one, or pinned to one
    /// too old to list.
    pub other_notes: Vec<NoteRow>,
}

#[derive(Debug, PartialEq)]
pub struct SessionDay {
    pub label: String,
    pub sessions: Vec<SessionRow>,
}

fn local(t: DateTime<Utc>) -> DateTime<Local> {
    t.with_timezone(&Local)
}

/// A time inside a sentence: `16:40` today, `yesterday 16:40`, else `Mon 19 Oct 16:40`.
fn moment(at: DateTime<Local>, now: DateTime<Local>) -> String {
    let t = time_on(at, now);
    match t.strip_prefix("Yesterday") {
        Some(rest) => format!("yesterday{rest}"),
        None => t,
    }
}

fn last_dir(path: &str) -> &str {
    path.rsplit('/').find(|p| !p.is_empty()).unwrap_or(path)
}

fn note_row(n: &Note, now: DateTime<Local>) -> NoteRow {
    NoteRow {
        id: n.id,
        body: n.body.clone(),
        written: time_on(local(n.created_at), now),
    }
}

fn session_row(s: &WorkSession, notes: Vec<NoteRow>, now: DateTime<Local>) -> SessionRow {
    let (start, end) = (local(s.started_at), local(s.ended_at));
    let end = if s.ongoing(now.with_timezone(&Utc)) {
        End::Ongoing
    } else if end == start {
        End::Moment
    } else {
        End::At(time_on(end, start))
    };
    let secs = (s.ended_at - s.started_at).num_seconds();
    SessionRow {
        id: s.id,
        title: s.title.clone(),
        started: moment(start, now),
        start_time: start.format("%H:%M").to_string(),
        end,
        duration: (secs > 0).then(|| duration(secs)),
        repos: s
            .summary
            .repos
            .iter()
            .map(|r| {
                let mut line = vec![plain(last_dir(&r.path))];
                if !r.branches.is_empty() {
                    line.extend([plain(" @ "), code(r.branches.join(", "))]);
                }
                line
            })
            .collect(),
        commands: s
            .summary
            .commands
            .iter()
            .take(COMMANDS_SHOWN)
            .map(|c| match c.runs {
                1 => c.cmdline.clone(),
                n => format!("{} ×{n}", c.cmdline),
            })
            .collect(),
        notes,
    }
}

/// Notes by the session they are pinned to, each list newest first as `notes` is.
fn notes_by_session(
    notes: &[Note],
    now: DateTime<Local>,
) -> HashMap<Option<SessionId>, Vec<NoteRow>> {
    let mut by: HashMap<Option<SessionId>, Vec<NoteRow>> = HashMap::new();
    for n in notes {
        by.entry(n.session).or_default().push(note_row(n, now));
    }
    by
}

/// The Sessions tab from the VM's sessions and notes, both newest first.
pub fn present_sessions(
    sessions: &[WorkSession],
    notes: &[Note],
    now: DateTime<Local>,
) -> SessionsView {
    let mut by = notes_by_session(notes, now);
    let mut days: Vec<SessionDay> = Vec::new();
    for s in sessions {
        let row = session_row(s, by.remove(&Some(s.id)).unwrap_or_default(), now);
        let label = day(local(s.started_at), now);
        match days.last_mut() {
            Some(d) if d.label == label => d.sessions.push(row),
            _ => days.push(SessionDay {
                label,
                sessions: vec![row],
            }),
        }
    }
    let other_notes = notes
        .iter()
        .filter(|n| by.contains_key(&n.session))
        .map(|n| note_row(n, now))
        .collect();
    SessionsView { days, other_notes }
}

/// The card from what the backend worked out (`None` before the first snapshot) and the
/// VM's sessions and notes, newest first.
pub fn present_work(
    work: Option<&CurrentWork>,
    sessions: &[WorkSession],
    notes: &[Note],
    now: DateTime<Local>,
) -> CurrentWorkView {
    let session = sessions.first().map(|s| {
        let pinned = notes_by_session(notes, now).remove(&Some(s.id));
        session_row(s, pinned.unwrap_or_default(), now)
    });
    let doing = match work {
        None => Doing::Unknown,
        Some(CurrentWork { anchor: None, .. }) => Doing::Nothing,
        Some(
            w @ CurrentWork {
                anchor: Some(anchor),
                ..
            },
        ) => doing(w, anchor, now),
    };
    CurrentWorkView {
        doing,
        note: session.as_ref().and_then(|s| s.notes.first().cloned()),
        session,
    }
}

fn doing(w: &CurrentWork, anchor: &Anchor, now: DateTime<Local>) -> Doing {
    let (what, cwd) = match anchor {
        Anchor::Process { cmdline, cwd, .. } => (vec![code(cmdline.clone())], cwd.as_deref()),
        Anchor::Tmux { name, cwd } => (
            vec![plain("tmux session "), code(name.clone())],
            cwd.as_deref(),
        ),
    };
    let mut facts: Vec<Line> = Vec::new();
    match (&w.repo, cwd) {
        (Some(r), _) => {
            let mut place = vec![plain("in "), plain(r.name())];
            if let Some(b) = &r.branch {
                place.extend([plain(" @ "), code(b.clone())]);
            }
            facts.push(place);
            if r.dirty > 0 {
                facts.push(vec![plain(format!("{} modified", r.dirty))]);
            }
        }
        (None, Some(cwd)) => facts.push(vec![plain("in "), code(cwd)]),
        (None, None) => {}
    }
    if let Some(t) = &w.tmux {
        facts.push(vec![plain("tmux "), code(t.clone())]);
    }
    if !w.containers.is_empty() {
        let shown = w.containers[..w.containers.len().min(CONTAINERS_SHOWN)].join(", ");
        let line = match w.containers.len().saturating_sub(CONTAINERS_SHOWN) {
            0 => format!("containers {shown}"),
            more => format!("containers {shown} +{more}"),
        };
        facts.push(vec![plain(line)]);
    }
    if let Some(since) = w.since {
        facts.push(vec![plain(format!("since {}", moment(local(since), now)))]);
    }
    Doing::Work { what, facts }
}

/// A line of spans at `size`: code in monospace.
fn inline(line: &[Span], size: gpui::Pixels) -> impl Iterator<Item = Div> + '_ {
    line.iter().filter(|s| !s.text.is_empty()).map(move |s| {
        let d = div()
            .whitespace_nowrap()
            .text_size(size)
            .child(s.text.clone());
        if s.code {
            d.font_family(MONO)
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
        } else {
            d.flex_shrink_0()
        }
    })
}

fn heading(content: &'static str) -> Div {
    div()
        .text_size(text::SECTION)
        .font_weight(FontWeight::SEMIBOLD)
        .child(content)
}

/// The keys an inline field answers to.
fn field_hint() -> Div {
    label("Enter saves · Esc cancels").flex_shrink_0()
}

fn small_icon_button(id: ElementId) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(20.))
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded(radius::S)
        .cursor_pointer()
        .hover(|s| s.bg(rgb(color::HOVER)))
        .child(icon(Icon::Close, px(12.), color::TEXT_MUTED))
}

/// A note: an accent bar, its text, when it was written and a delete button.
fn note_view(n: &NoteRow, cx: &mut Context<AppView>) -> Div {
    let id = n.id;
    let body = n.body.clone();
    div()
        .debug_selector(|| format!("note: {body}"))
        .flex()
        .items_center()
        .gap(space::M)
        .pl(space::M)
        .py(px(2.))
        .border_l_2()
        .border_color(rgb(color::ACCENT))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(text::BODY)
                .child(n.body.clone()),
        )
        .child(label(n.written.clone()).flex_shrink_0())
        .child({
            let body = n.body.clone();
            small_icon_button(ElementId::Name(format!("delete-note-{id}").into()))
                .debug_selector(move || format!("delete note: {body}"))
                .on_click(cx.listener(move |this, _, window, cx| this.delete_note(id, window, cx)))
        })
}

fn fact_line(line: &[Span]) -> Div {
    div()
        .flex()
        .items_center()
        .min_w_0()
        .overflow_hidden()
        .children(inline(line, text::META))
}

/// Facts joined by dots, in the secondary text colour.
fn facts_row(facts: &[Line]) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(6.))
        .text_color(rgb(color::TEXT_SECONDARY))
        .children(facts.iter().enumerate().flat_map(|(i, f)| {
            let dot = (i > 0).then(|| div().text_size(text::META).child("·"));
            dot.into_iter().chain(std::iter::once(fact_line(f)))
        }))
}

fn doing_view(doing: &Doing) -> Div {
    let quiet = |title: &'static str, why: &'static str| {
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(
                div()
                    .text_size(text::BODY)
                    .text_color(rgb(color::TEXT_SECONDARY))
                    .child(title),
            )
            .child(label(why))
    };
    match doing {
        Doing::Unknown => quiet(
            "No snapshot yet",
            "Refresh collects one once the VM is reachable.",
        )
        .debug_selector(|| "work: none".to_owned()),
        Doing::Nothing => quiet(
            "Nothing running",
            "The last snapshot shows no command or tmux session of yours.",
        )
        .debug_selector(|| "work: nothing".to_owned()),
        Doing::Work { what, facts } => {
            let selector = format!("work: {}", text_of(what));
            div()
                .debug_selector(move || selector)
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .min_w_0()
                        .overflow_hidden()
                        .font_weight(FontWeight::MEDIUM)
                        .children(inline(what, text::SECTION)),
                )
                .when(!facts.is_empty(), |d| d.child(facts_row(facts)))
        }
    }
}

/// The card at the top of the Overview. `adding` is the note field, while it is open.
pub fn card_view(
    view: &CurrentWorkView,
    adding: Option<&Entity<TextField>>,
    cx: &mut Context<AppView>,
) -> Div {
    card()
        .debug_selector(|| "work-card".to_owned())
        .p(space::L)
        .gap(space::M)
        .child(
            div()
                .h(px(24.))
                .flex()
                .items_center()
                .gap(space::S)
                .child(heading("Current work"))
                .child(div().flex_1())
                .when(adding.is_none(), |d| {
                    d.child(
                        button(Button::Secondary, "add-note")
                            .h(px(26.))
                            .px(space::S)
                            .text_size(text::META)
                            .child(icon(Icon::Plus, px(12.), color::TEXT))
                            .child("Add note")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start_note(window, cx);
                            })),
                    )
                }),
        )
        .when_some(adding, |d, field| {
            d.child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::M)
                    .child(div().flex_1().child(field.clone()))
                    .child(field_hint()),
            )
        })
        .when_some(view.note.as_ref(), |d, n| d.child(note_view(n, cx)))
        .child(doing_view(&view.doing))
        .when_some(view.session.as_ref(), |d, s| {
            d.child(
                div()
                    .pt(space::M)
                    .border_t_1()
                    .border_color(rgb(color::BORDER))
                    .flex()
                    .items_center()
                    .gap(space::S)
                    .text_size(text::META)
                    .child(label(if s.ongoing() {
                        "Session"
                    } else {
                        "Last session"
                    }))
                    .when_some(s.title.clone(), |d, t| {
                        d.child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(color::TEXT))
                                .child(t),
                        )
                    })
                    .child(
                        div()
                            .debug_selector(|| "work-session".to_owned())
                            .text_color(rgb(color::TEXT_SECONDARY))
                            .child(s.brief()),
                    )
                    .when(s.ongoing(), |d| d.child(pill(Tone::Good, "ongoing"))),
            )
        })
}

/// A caption beside its value, for a session's repos and commands.
fn session_fact(caption: &'static str, value: Div) -> Div {
    div()
        .flex()
        .items_center()
        .gap(space::M)
        .min_w_0()
        .child(label(caption).w(px(72.)).flex_shrink_0())
        .child(value.flex_1().min_w_0().overflow_hidden())
}

fn session_title(s: &SessionRow, cx: &mut Context<AppView>) -> Stateful<Div> {
    let (id, current) = (s.id, s.title.clone());
    let shown = s
        .title
        .clone()
        .unwrap_or_else(|| "Untitled session".to_owned());
    let selector = format!("session: {shown}");
    div()
        .id(ElementId::Name(format!("rename-{id}").into()))
        .debug_selector(move || selector)
        .min_w_0()
        .px(px(6.))
        .mx(px(-6.))
        .h(px(26.))
        .flex()
        .items_center()
        .rounded(radius::S)
        .cursor_text()
        .hover(|s| s.bg(rgb(color::HOVER)))
        .text_size(text::SECTION)
        .font_weight(FontWeight::SEMIBOLD)
        .whitespace_nowrap()
        .overflow_hidden()
        .text_ellipsis()
        .when(s.title.is_none(), |d| {
            d.font_weight(FontWeight::MEDIUM)
                .text_color(rgb(color::TEXT_MUTED))
        })
        .child(shown)
        .on_click(cx.listener(move |this, _, window, cx| {
            this.start_rename(id, current.as_deref(), window, cx);
        }))
}

fn session_card(
    s: &SessionRow,
    renaming: Option<&Entity<TextField>>,
    cx: &mut Context<AppView>,
) -> Div {
    let commands = s.commands.join(" · ");
    card()
        .px(space::L)
        .py(space::M)
        .gap(space::S)
        .child(
            div()
                .h(px(28.))
                .flex()
                .items_center()
                .gap(space::M)
                .child(match renaming {
                    Some(field) => div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .gap(space::M)
                        .child(div().flex_1().max_w(px(420.)).child(field.clone()))
                        .child(field_hint()),
                    None => div().flex_1().min_w_0().flex().child(session_title(s, cx)),
                })
                .when(s.ongoing(), |d| d.child(pill(Tone::Good, "ongoing")))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(text::META)
                        .text_color(rgb(color::TEXT_SECONDARY))
                        .child(s.span()),
                )
                .when_some(s.duration.clone(), |d, t| {
                    d.child(label(t).flex_shrink_0().min_w(px(44.)).flex().justify_end())
                }),
        )
        .when(!s.repos.is_empty(), |d| {
            d.child(session_fact(
                "Repos",
                div()
                    .flex()
                    .items_center()
                    .gap(space::M)
                    .children(s.repos.iter().map(|r| {
                        div()
                            .flex()
                            .items_center()
                            .flex_shrink_0()
                            .text_color(rgb(color::TEXT))
                            .children(inline(r, text::BODY))
                    })),
            ))
        })
        .when(!s.commands.is_empty(), |d| {
            d.child(session_fact(
                "Commands",
                div()
                    .font_family(MONO)
                    .text_size(text::META)
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(commands),
            ))
        })
        .when(!s.notes.is_empty(), |d| {
            d.child(
                div()
                    .pt(space::XS)
                    .flex()
                    .flex_col()
                    .gap(space::S)
                    .children(s.notes.iter().map(|n| note_view(n, cx))),
            )
        })
}

/// The Sessions tab. `renaming` is the session whose title is being edited, with its field.
pub fn sessions_view(
    view: &SessionsView,
    renaming: Option<(SessionId, &Entity<TextField>)>,
    cx: &mut Context<AppView>,
) -> Div {
    if view.days.is_empty() && view.other_notes.is_empty() {
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
                    .child("No work sessions yet"),
            )
            .child(label(
                "One starts with the first command, tmux session or git change recorded here.",
            ));
    }
    let editing = |id: SessionId| renaming.and_then(|(r, f)| (r == id).then_some(f));
    div()
        .flex()
        .flex_col()
        .children(view.days.iter().enumerate().map(|(i, d)| {
            div()
                .flex()
                .flex_col()
                .child(day_header(&d.label, i == 0))
                .child(
                    div().flex().flex_col().gap(space::S).children(
                        d.sessions
                            .iter()
                            .map(|s| session_card(s, editing(s.id), cx)),
                    ),
                )
        }))
        .when(!view.other_notes.is_empty(), |d| {
            d.child(day_header("Other notes", view.days.is_empty()))
                .child(
                    card()
                        .px(space::L)
                        .py(space::M)
                        .gap(space::S)
                        .children(view.other_notes.iter().map(|n| note_view(n, cx))),
                )
        })
}

/// What a sidebar row says it was doing, muted and on one line.
pub fn sidebar_line(vm_name: &str, line: &str) -> Div {
    let selector = format!("{vm_name} work: {line}");
    label(SharedString::from(line.to_owned()))
        .debug_selector(move || selector)
        .whitespace_nowrap()
        .overflow_hidden()
        .text_ellipsis()
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};
    use uuid::Uuid;
    use vmdesk_collect::work::WorkRepo;
    use vmdesk_core::VmId;
    use vmdesk_db::{CommandRuns, RepoTouched, SessionSummary};

    use super::*;

    fn now() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 19, 18, 0, 0).unwrap()
    }

    fn session(start: DateTime<Local>, minutes: i64, title: Option<&str>) -> WorkSession {
        WorkSession {
            id: SessionId(Uuid::now_v7()),
            vm: VmId::new(),
            started_at: start.with_timezone(&Utc),
            ended_at: (start + Duration::minutes(minutes)).with_timezone(&Utc),
            title: title.map(str::to_owned),
            summary: SessionSummary {
                events: 4,
                repos: vec![
                    RepoTouched {
                        path: "/root/regal".into(),
                        branches: vec!["group2-fix".into(), "main".into()],
                    },
                    RepoTouched {
                        path: "/root/5G_Test_Suites".into(),
                        branches: vec![],
                    },
                ],
                commands: vec![
                    CommandRuns {
                        cmdline: "pytest tests/group2".into(),
                        runs: 3,
                        longest_secs: Some(60),
                    },
                    CommandRuns {
                        cmdline: "make build".into(),
                        runs: 1,
                        longest_secs: None,
                    },
                ],
                ..SessionSummary::default()
            },
        }
    }

    fn note(session: Option<SessionId>, body: &str, at: DateTime<Local>) -> Note {
        Note {
            id: NoteId(Uuid::now_v7()),
            vm: VmId::new(),
            session,
            body: body.into(),
            created_at: at.with_timezone(&Utc),
            updated_at: at.with_timezone(&Utc),
        }
    }

    #[test]
    fn the_card_names_the_command_its_repo_and_the_session() {
        let now = now();
        let latest = session(now - Duration::minutes(55), 50, Some("nightly debug"));
        let older = session(now - Duration::days(1), 0, None);
        let notes = [
            note(Some(latest.id), "waiting on CI", now - Duration::minutes(5)),
            note(
                Some(latest.id),
                "older thought",
                now - Duration::minutes(30),
            ),
            note(Some(older.id), "yesterday's", now - Duration::days(1)),
        ];
        let work = CurrentWork {
            anchor: Some(Anchor::Process {
                pid: 7,
                cmdline: "pytest tests/group2".into(),
                cwd: Some("/root/regal".into()),
            }),
            repo: Some(WorkRepo {
                path: "/root/regal".into(),
                branch: Some("group2-fix".into()),
                dirty: 3,
            }),
            tmux: Some("nightly-debug".into()),
            containers: vec![],
            since: Some(latest.started_at),
            note: Some("waiting on CI".into()),
        };
        let view = present_work(Some(&work), &[latest, older], &notes, now);
        let Doing::Work { what, facts } = &view.doing else {
            panic!("{:?}", view.doing)
        };
        assert_eq!(text_of(what), "pytest tests/group2");
        assert!(what[0].code, "the command is set in monospace");
        assert_eq!(
            facts.iter().map(|f| text_of(f)).collect::<Vec<_>>(),
            [
                "in regal @ group2-fix",
                "3 modified",
                "tmux nightly-debug",
                "since 17:05"
            ]
        );
        assert_eq!(
            view.note.as_ref().map(|n| n.body.as_str()),
            Some("waiting on CI")
        );
        let s = view.session.unwrap();
        assert_eq!(s.title.as_deref(), Some("nightly debug"));
        assert_eq!(s.brief(), "started 17:05 · 50m · 2 repos");
        assert!(s.ongoing());
    }

    #[test]
    fn the_card_says_when_there_is_nothing_to_show() {
        let now = now();
        assert_eq!(
            present_work(None, &[], &[], now),
            CurrentWorkView {
                doing: Doing::Unknown,
                note: None,
                session: None
            }
        );
        let idle = CurrentWork {
            anchor: None,
            repo: None,
            tmux: None,
            containers: vec![],
            since: None,
            note: None,
        };
        assert_eq!(
            present_work(Some(&idle), &[], &[], now).doing,
            Doing::Nothing
        );
    }

    #[test]
    fn sessions_list_under_their_day_with_their_notes() {
        let now = now();
        let today = session(now - Duration::hours(3), 55, None);
        let yesterday = session(
            now - Duration::days(1) - Duration::hours(2),
            0,
            Some("deploy"),
        );
        let notes = [
            note(Some(today.id), "flaky N4 test", now - Duration::hours(2)),
            note(None, "before any session", now - Duration::days(3)),
        ];
        let view = present_sessions(&[today, yesterday], &notes, now);
        let days: Vec<(&str, Vec<String>)> = view
            .days
            .iter()
            .map(|d| {
                (
                    d.label.as_str(),
                    d.sessions.iter().map(SessionRow::span).collect(),
                )
            })
            .collect();
        assert_eq!(
            days,
            [
                ("Today", vec!["15:00 – 15:55".to_owned()]),
                ("Yesterday", vec!["16:00".to_owned()])
            ]
        );

        let first = &view.days[0].sessions[0];
        assert_eq!(first.title, None);
        assert!(!first.ongoing(), "it ended more than 2 h ago");
        assert_eq!(first.brief(), "15:00 – 15:55 · 55m · 2 repos");
        assert_eq!(first.duration.as_deref(), Some("55m"));
        assert_eq!(
            first.repos.iter().map(|r| text_of(r)).collect::<Vec<_>>(),
            ["regal @ group2-fix, main", "5G_Test_Suites"]
        );
        assert_eq!(first.commands, ["pytest tests/group2 ×3", "make build"]);
        assert_eq!(
            first
                .notes
                .iter()
                .map(|n| n.body.as_str())
                .collect::<Vec<_>>(),
            ["flaky N4 test"]
        );
        assert_eq!(view.days[1].sessions[0].duration, None, "a single moment");
        assert_eq!(
            view.other_notes
                .iter()
                .map(|n| n.body.as_str())
                .collect::<Vec<_>>(),
            ["before any session"]
        );
    }
}
