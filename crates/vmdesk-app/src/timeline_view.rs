//! The timeline tab: which stretch of the VM's events it holds, each event in words
//! ([`TimelineEntry::text`]) as a row under its day, then how the tab is drawn.

use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use gpui::{AnyElement, Context, Div, FontWeight, div, prelude::*, px, rgb};
use vmdesk_core::human::day;
use vmdesk_db::{Category, EventKey, Line, Span, TimelineEntry, text_of};

use crate::kit::{Button, button, label};
use crate::theme::{MONO, color, radius, space, text, tint, tone_color};
use crate::ui::AppView;

/// Events the tab holds when it opens on the newest.
const LATEST: u32 = 300;
/// Events either side of a search hit when the tab opens on one.
const AROUND: u32 = 100;
/// Events that one Show older or Show newer adds.
const PAGE: u32 = 200;
/// How long a search hit's row stays tinted.
pub const HIGHLIGHT: Duration = Duration::from_secs(4);
/// Items kept above a search hit scrolled to the top; the first sits under the pinned day.
const CONTEXT: usize = 4;

/// Which stretch of the selected VM's events the tab holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimelineWindow {
    /// The newest `count` events.
    Latest { count: u32 },
    /// A search hit, up to `newer` events after it and up to `older` before it. The hit's
    /// row is tinted until `marked_until`.
    Around {
        hit: EventKey,
        newer: u32,
        older: u32,
        marked_until: Instant,
    },
}

impl TimelineWindow {
    pub const LATEST: Self = Self::Latest { count: LATEST };

    /// Opens on `hit` at `now`, as the executor's clock tells it.
    pub fn around(hit: EventKey, now: Instant) -> Self {
        Self::Around {
            hit,
            newer: AROUND,
            older: AROUND,
            marked_until: now + HIGHLIGHT,
        }
    }

    pub fn hit(&self) -> Option<&EventKey> {
        match self {
            Self::Latest { .. } => None,
            Self::Around { hit, .. } => Some(hit),
        }
    }

    /// This window with another page of older events.
    #[must_use]
    pub fn older(&self) -> Self {
        match self.clone() {
            Self::Latest { count } => Self::Latest {
                count: count + PAGE,
            },
            Self::Around {
                hit,
                newer,
                older,
                marked_until,
            } => Self::Around {
                hit,
                newer,
                older: older + PAGE,
                marked_until,
            },
        }
    }

    /// This window with another page of newer events. The newest are always held.
    #[must_use]
    pub fn newer(&self) -> Self {
        match self.clone() {
            Self::Around {
                hit,
                newer,
                older,
                marked_until,
            } => Self::Around {
                hit,
                newer: newer + PAGE,
                older,
                marked_until,
            },
            latest @ Self::Latest { .. } => latest,
        }
    }

    /// Whether `other` opens at the same place: the newest events, or the same opening of
    /// the same hit. Only a move to another place scrolls the tab.
    pub fn same_place(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Latest { .. }, Self::Latest { .. }) => true,
            (
                Self::Around {
                    hit, marked_until, ..
                },
                Self::Around {
                    hit: other_hit,
                    marked_until: other_until,
                    ..
                },
            ) => hit == other_hit && marked_until == other_until,
            _ => false,
        }
    }
}

/// The events of one window, newest first, and whether more lie beyond either end.
#[derive(Debug)]
pub struct TimelineSlice {
    pub window: TimelineWindow,
    pub events: Vec<TimelineEntry>,
    pub more_newer: bool,
    pub more_older: bool,
}

impl TimelineSlice {
    /// `events` as read for `window` with one more than it holds at each end: the extra
    /// ones are dropped and only say that more exist.
    pub fn new(window: TimelineWindow, mut events: Vec<TimelineEntry>) -> Self {
        let (newer, older) = match &window {
            TimelineWindow::Latest { count } => (0, *count),
            TimelineWindow::Around { newer, older, .. } => (*newer, *older),
        };
        let hit = window.hit();
        let newer_side = events
            .iter()
            .filter(|e| hit.is_some_and(|h| e.key() > *h))
            .count();
        let older_side = events
            .iter()
            .filter(|e| hit.is_none_or(|h| e.key() < *h))
            .count();
        let beyond = |found: usize, held: u32| found > usize::try_from(held).unwrap_or(usize::MAX);
        let (more_newer, more_older) = (beyond(newer_side, newer), beyond(older_side, older));
        if more_newer {
            events.remove(0);
        }
        if more_older {
            events.pop();
        }
        Self {
            window,
            events,
            more_newer,
            more_older,
        }
    }
}

fn category_color(c: Category) -> u32 {
    match c {
        Category::Process => color::BLUE,
        Category::Container => color::TEAL,
        Category::Tmux => color::PURPLE,
        Category::Git => color::ORANGE,
        Category::Network => color::PINK,
        Category::Status | Category::Gap | Category::Other => color::GREY,
    }
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
    /// It holds the search hit the tab opened on.
    pub hit: bool,
}

#[derive(Debug, PartialEq)]
pub struct Day {
    pub label: String,
    pub rows: Vec<Row>,
}

/// One event as a timeline row.
pub fn describe(entry: &TimelineEntry) -> Row {
    let text = entry.text();
    Row {
        at: entry.ts.with_timezone(&Local),
        category: text.category,
        color: text.tone.map_or(category_color(text.category), tone_color),
        ended: text.ended,
        title: text.title,
        detail: text.detail,
        source: entry.source_tag().to_owned(),
        count: 1,
        hit: false,
    }
}

/// Newest first, as stored: one day per header, identical events in the same minute
/// folded into one row. The row with `hit` in it is marked as the hit's.
pub fn present(
    entries: &[TimelineEntry],
    now: DateTime<Local>,
    hit: Option<&EventKey>,
) -> Vec<Day> {
    let mut days: Vec<Day> = Vec::new();
    for entry in entries {
        let next = Row {
            hit: hit.is_some_and(|h| *h == entry.key()),
            ..describe(entry)
        };
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
                last.hit |= next.hit;
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

/// What the tab lists, top to bottom. Each is a direct child of the scrolling body, so
/// the body can scroll to one by its index.
#[derive(Debug, PartialEq)]
pub enum Item {
    Newer,
    Day { label: String, first: bool },
    Row(Row),
    Older,
}

pub fn items(slice: &TimelineSlice, now: DateTime<Local>) -> Vec<Item> {
    let mut items = Vec::new();
    if slice.more_newer {
        items.push(Item::Newer);
    }
    for (i, d) in present(&slice.events, now, slice.window.hit())
        .into_iter()
        .enumerate()
    {
        items.push(Item::Day {
            label: d.label,
            first: i == 0,
        });
        items.extend(d.rows.into_iter().map(Item::Row));
    }
    if slice.more_older {
        items.push(Item::Older);
    }
    items
}

/// The item to scroll to the top: a few above the hit's row. `None` for the top.
pub fn anchor(items: &[Item]) -> Option<usize> {
    items
        .iter()
        .position(|i| matches!(i, Item::Row(r) if r.hit))
        .map(|ix| ix.saturating_sub(CONTEXT))
}

/// The day of the item at the top of the scrolled tab, once that day's header has
/// scrolled out above it.
pub fn pinned_day(items: &[Item], top: usize) -> Option<String> {
    let top = top.min(items.len().checked_sub(1)?);
    let (at, label) = items[..=top]
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, item)| match item {
            Item::Day { label, .. } => Some((i, label)),
            _ => None,
        })?;
    (at < top).then(|| label.clone())
}

/// The day header held over the top of the scrolled tab.
pub fn pinned(day: &str) -> Div {
    let selector = format!("pinned-day: {day}");
    div()
        .debug_selector(|| selector)
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .px(space::XL)
        .pt(space::XS)
        .bg(rgb(color::BG))
        .child(day_header(day, true))
}

/// The tab's `items`, as the scrolling body's children. `now` is the executor's clock.
pub fn render(
    items: Vec<Item>,
    window: &TimelineWindow,
    now: Instant,
    cx: &mut Context<AppView>,
) -> Vec<AnyElement> {
    if items.is_empty() {
        let empty = div()
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
        return vec![empty.into_any_element()];
    }
    let marked =
        matches!(window, TimelineWindow::Around { marked_until, .. } if now < *marked_until);
    items
        .into_iter()
        .map(|item| match item {
            Item::Newer => more(
                "timeline-newer",
                "Show newer events",
                TimelineWindow::newer,
                cx,
            ),
            Item::Day { label, first } => day_header(&label, first).into_any_element(),
            Item::Row(r) => row_view(&r, marked && r.hit).into_any_element(),
            Item::Older => more(
                "timeline-older",
                "Show older events",
                TimelineWindow::older,
                cx,
            ),
        })
        .collect()
}

/// A button that loads another page of the window.
fn more(
    id: &'static str,
    text: &'static str,
    next: fn(&TimelineWindow) -> TimelineWindow,
    cx: &mut Context<AppView>,
) -> AnyElement {
    div()
        .my(space::S)
        .flex()
        .justify_center()
        .child(
            button(Button::Secondary, id)
                .child(text)
                .on_click(cx.listener(move |this, _, _, cx| {
                    let window = next(&this.timeline);
                    this.show_timeline(window, cx);
                })),
        )
        .into_any_element()
}

/// Above the tab from the moment it asks for the events around a search hit: when the
/// hit happened, and the way back. Shown before they load, so the scrolling body below
/// it is already where it will be when it scrolls to the hit.
pub fn banner(window: &TimelineWindow, cx: &mut Context<AppView>) -> Option<Div> {
    let hit = window.hit()?.ts.with_timezone(&Local);
    let when = format!("{} {}", day(hit, Local::now()), hit.format("%H:%M"));
    Some(
        div()
            .debug_selector(|| "timeline-banner".to_owned())
            .px(space::XL)
            .py(space::S)
            .flex()
            .items_center()
            .gap(space::M)
            .border_b_1()
            .border_color(rgb(color::BORDER))
            .bg(tint(color::ACCENT))
            .child(
                div()
                    .flex_1()
                    .text_size(text::META)
                    .text_color(rgb(color::TEXT_SECONDARY))
                    .child(format!("Events around the search hit from {when}")),
            )
            .child(
                button(Button::Secondary, "back-to-latest")
                    .child("Back to latest")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_timeline(TimelineWindow::LATEST, cx);
                    })),
            ),
    )
}

pub fn day_header(day: &str, first: bool) -> Div {
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

pub fn spans(line: &[Span]) -> impl Iterator<Item = Div> + '_ {
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

pub fn marker(r: &Row) -> Div {
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

/// The event's local time, in the first column of a row.
pub fn time_cell(r: &Row) -> Div {
    div()
        .w(px(38.))
        .flex_shrink_0()
        .text_size(text::META)
        .text_color(rgb(color::TEXT_MUTED))
        .child(r.at.format("%H:%M").to_string())
}

/// The sentence, its count and its detail.
pub fn sentence(r: &Row) -> Div {
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
        })
}

/// A row; the search hit's, while `marked`, on an accent tint.
fn row_view(r: &Row, marked: bool) -> Div {
    let title = text_of(&r.title);
    let selector = format!("marked: {title}");
    let row = div()
        .debug_selector(|| format!("event: {title}"))
        .h(px(32.))
        .flex()
        .items_center()
        .gap(space::M)
        .child(time_cell(r))
        .child(marker(r))
        .child(sentence(r))
        .child(
            div()
                .flex_shrink_0()
                .text_size(text::SMALL)
                .text_color(rgb(color::TEXT_MUTED))
                .child(r.source.clone()),
        );
    if !marked {
        return row;
    }
    div()
        .debug_selector(|| selector)
        .mx(px(-8.))
        .px(space::S)
        .rounded(radius::M)
        .bg(tint(color::ACCENT))
        .child(row)
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};
    use serde_json::{Value, json};

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

    fn shown(days: &[Day]) -> Vec<String> {
        days.iter()
            .flat_map(|d| {
                std::iter::once(format!("# {}", d.label))
                    .chain(d.rows.iter().map(|r| text_of(&r.title)))
            })
            .collect()
    }

    #[test]
    fn rows_fall_under_their_day_newest_first() {
        let now = Local.with_ymd_and_hms(2026, 10, 19, 18, 0, 0).unwrap();
        let started = |cmdline: &str| json!({ "pid": 7, "cmdline": cmdline });
        let entries = [
            entry(
                "agent:01a1",
                "process_started",
                now - Duration::hours(1),
                started("make"),
            ),
            entry(
                "ssh",
                "process_started",
                now - Duration::days(1),
                started("cargo test"),
            ),
            entry(
                "ssh",
                "process_started",
                now - Duration::days(4),
                started("npm ci"),
            ),
        ];
        let days = present(&entries, now, None);
        assert_eq!(
            shown(&days),
            [
                "# Today",
                "Started make",
                "# Yesterday",
                "Started cargo test",
                "# Thu 15 Oct",
                "Started npm ci",
            ]
        );
        assert_eq!(days[0].rows[0].source, "agent");
        assert_eq!(days[0].rows[0].color, color::BLUE);
    }

    #[test]
    fn a_status_change_takes_the_colour_of_its_outcome() {
        let now = Local.with_ymd_and_hms(2026, 10, 19, 18, 0, 0).unwrap();
        let change = |was: &str, ssh: &str| {
            json!({ "ssh": ssh, "agent": "unknown", "route": "tun0",
                "previous": { "ssh": was, "agent": "unknown", "route": "tun0" } })
        };
        let lost = describe(&entry(
            "laptop",
            "status_changed",
            now,
            change("ok", "timeout"),
        ));
        let back = describe(&entry(
            "laptop",
            "status_changed",
            now,
            change("timeout", "ok"),
        ));
        assert_eq!((lost.color, back.color), (color::AMBER, color::GREEN));
    }

    #[test]
    fn a_window_drops_the_extra_events_and_says_more_lie_beyond() {
        let now = Local.with_ymd_and_hms(2026, 10, 19, 18, 0, 0).unwrap();
        let job = |m: i64| TimelineEntry {
            source: "ssh".into(),
            seq: 100 - m,
            ts: (now - Duration::minutes(m)).with_timezone(&Utc),
            kind: "process_started".into(),
            data: json!({ "cmdline": format!("job {m}") }),
        };
        let jobs = |minutes: std::ops::RangeInclusive<i64>| minutes.map(job).collect::<Vec<_>>();
        let held = |s: &TimelineSlice| -> Vec<String> {
            s.events
                .iter()
                .map(|e| e.data["cmdline"].as_str().unwrap().to_owned())
                .collect()
        };

        let latest = TimelineSlice::new(TimelineWindow::Latest { count: 3 }, jobs(0..=3));
        assert_eq!(held(&latest), ["job 0", "job 1", "job 2"]);
        assert_eq!((latest.more_newer, latest.more_older), (false, true));
        let all = TimelineSlice::new(TimelineWindow::Latest { count: 4 }, jobs(0..=3));
        assert_eq!((all.events.len(), all.more_older), (4, false));
        assert_eq!(
            anchor(&items(&all, now)),
            None,
            "the newest open at the top"
        );

        let around = TimelineWindow::Around {
            hit: job(5).key(),
            newer: 2,
            older: 2,
            marked_until: Instant::now(),
        };
        // Read with one more each side: three newer, the hit, three older.
        let slice = TimelineSlice::new(around.clone(), jobs(2..=8));
        assert_eq!(held(&slice), ["job 3", "job 4", "job 5", "job 6", "job 7"]);
        assert_eq!((slice.more_newer, slice.more_older), (true, true));
        let listed = items(&slice, now);
        assert_eq!(listed.first(), Some(&Item::Newer));
        assert_eq!(listed.last(), Some(&Item::Older));
        let hit_at = listed
            .iter()
            .position(|i| matches!(i, Item::Row(r) if r.hit))
            .unwrap();
        assert_eq!(hit_at, 4, "Show newer, the day, job 3, job 4, then the hit");
        assert_eq!(anchor(&listed), Some(hit_at - CONTEXT));

        let near_top = TimelineSlice::new(around, jobs(4..=8));
        assert_eq!(held(&near_top)[0], "job 4");
        assert!(!near_top.more_newer, "only one event is newer than the hit");
    }

    #[test]
    fn the_day_scrolled_past_is_pinned() {
        let now = Local.with_ymd_and_hms(2026, 10, 19, 18, 0, 0).unwrap();
        let started = |ago: Duration| {
            entry(
                "ssh",
                "process_started",
                now - ago,
                json!({ "cmdline": format!("job {}", ago.num_minutes()) }),
            )
        };
        let slice = TimelineSlice {
            window: TimelineWindow::LATEST,
            events: vec![
                started(Duration::minutes(1)),
                started(Duration::minutes(2)),
                started(Duration::days(1)),
                started(Duration::days(1) + Duration::minutes(1)),
            ],
            more_newer: false,
            more_older: true,
        };
        let listed = items(&slice, now);
        // Today, job 1, job 2, Yesterday, job 1440, job 1441, Show older.
        let pinned = |top| pinned_day(&listed, top);
        assert_eq!(pinned(0), None, "the header itself is at the top");
        assert_eq!(pinned(1).as_deref(), Some("Today"));
        assert_eq!(pinned(2).as_deref(), Some("Today"));
        assert_eq!(pinned(3), None);
        assert_eq!(pinned(5).as_deref(), Some("Yesterday"));
        assert_eq!(pinned(6).as_deref(), Some("Yesterday"));
        assert_eq!(pinned(99).as_deref(), Some("Yesterday"));
        assert_eq!(pinned_day(&[], 0), None);
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
        let days = present(
            &[exited(586_707), exited(586_707), exited(578_412)],
            now,
            None,
        );
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
}
