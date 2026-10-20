//! The timeline tab: each stored event in words ([`TimelineEntry::text`]) as a row,
//! grouped under day headers, then how a row is drawn.

use chrono::{DateTime, Local};
use gpui::{Div, FontWeight, div, prelude::*, px, rgb};
use vmdesk_core::human::day;
use vmdesk_db::{Category, Line, Span, TimelineEntry, text_of};

use crate::kit::label;
use crate::theme::{MONO, color, space, text, tone_color};

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
    }
}

/// Newest first, as stored: one day per header, identical events in the same minute
/// folded into one row.
pub fn present(entries: &[TimelineEntry], now: DateTime<Local>) -> Vec<Day> {
    let mut days: Vec<Day> = Vec::new();
    for entry in entries {
        let next = describe(entry);
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

fn row_view(r: &Row) -> Div {
    let title = text_of(&r.title);
    div()
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
        )
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
        let days = present(&entries, now);
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
}
