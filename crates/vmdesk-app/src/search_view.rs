//! Event search across every VM: "which VM was I running X on last week?". The query
//! runs a moment after typing stops; each hit reads as its timeline sentence, under
//! the day it happened, with the VM it happened on.

use std::time::Duration;

use chrono::{DateTime, Local};
use gpui::{
    AppContext, Context, Div, ElementId, Entity, FontWeight, Subscription, Task, div, prelude::*,
    px, rgb,
};
use vmdesk_core::VmId;
use vmdesk_db::SearchHit;

use crate::backend::SEARCH_LIMIT;
use crate::field::TextField;
use crate::human::day;
use crate::kit::{callout, label};
use crate::theme::{Tone, color, radius, space, text};
use crate::timeline_view::{Row, day_header, describe, marker, sentence, text_of, time_cell};
use crate::ui::AppView;

/// How long typing must pause before the query runs.
pub const DEBOUNCE: Duration = Duration::from_millis(200);

/// What the search pane shows for the query last sent.
#[derive(Debug, PartialEq)]
pub enum SearchState {
    /// Nothing to search for: no letter or digit typed yet.
    Idle,
    Searching {
        query: String,
    },
    Results {
        days: Vec<HitDay>,
        count: usize,
    },
    Empty {
        query: String,
    },
    Error {
        message: String,
    },
}

impl SearchState {
    /// The state once the backend answered `query` ([`crate::backend::Backend::search`]).
    pub fn answered(
        query: String,
        found: anyhow::Result<Option<Vec<SearchHit>>>,
        now: DateTime<Local>,
    ) -> Self {
        match found {
            Ok(None) => Self::Idle,
            Ok(Some(hits)) if hits.is_empty() => Self::Empty { query },
            Ok(Some(hits)) => Self::Results {
                count: hits.len(),
                days: present(&hits, now),
            },
            Err(e) => Self::Error {
                message: format!("{e:#}"),
            },
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct HitDay {
    pub label: String,
    pub hits: Vec<Hit>,
}

#[derive(Debug, PartialEq)]
pub struct Hit {
    pub vm: VmId,
    pub vm_name: String,
    pub row: Row,
}

/// Hits, newest first as found, under the day each happened.
pub fn present(hits: &[SearchHit], now: DateTime<Local>) -> Vec<HitDay> {
    let mut days: Vec<HitDay> = Vec::new();
    for h in hits {
        let row = describe(&h.event);
        let label = day(row.at, now);
        let hit = Hit {
            vm: h.vm,
            vm_name: h.vm_name.clone(),
            row,
        };
        match days.last_mut() {
            Some(d) if d.label == label => d.hits.push(hit),
            _ => days.push(HitDay {
                label,
                hits: vec![hit],
            }),
        }
    }
    days
}

/// The open search pane.
pub struct Search {
    pub field: Entity<TextField>,
    pub state: SearchState,
    /// The wait, then the query, for the latest edit. Replacing it drops (so cancels)
    /// the one before, and an older query never overwrites a newer one.
    pub pending: Option<Task<()>>,
    _edits: Subscription,
}

impl Search {
    pub fn new(cx: &mut Context<AppView>) -> Self {
        let field = cx.new(|cx| TextField::new("Search events on every VM", false, cx));
        let edits = cx.observe(&field, |this, _, cx| this.search_edited(cx));
        Self {
            field,
            state: SearchState::Idle,
            pending: None,
            _edits: edits,
        }
    }
}

fn quiet(title: String, why: &'static str) -> Div {
    div()
        .pt(space::XL)
        .flex()
        .flex_col()
        .items_center()
        .gap(space::XS)
        .child(
            div()
                .text_size(text::BODY)
                .text_color(rgb(color::TEXT_SECONDARY))
                .child(title),
        )
        .child(label(why))
}

fn hit_row(hit: &Hit, index: usize, cx: &mut Context<AppView>) -> impl IntoElement {
    let vm = hit.vm;
    let selector = format!("hit: {} {}", hit.vm_name, text_of(&hit.row.title));
    div()
        .id(ElementId::NamedInteger("hit".into(), index as u64))
        .debug_selector(move || selector)
        .h(px(32.))
        .mx(px(-8.))
        .px(space::S)
        .flex()
        .items_center()
        .gap(space::M)
        .rounded(radius::M)
        .cursor_pointer()
        .hover(|s| s.bg(rgb(color::HOVER)))
        .on_click(cx.listener(move |this, _, window, cx| this.open_hit(vm, window, cx)))
        .child(time_cell(&hit.row))
        .child(
            div()
                .w(px(112.))
                .flex_shrink_0()
                .text_size(text::BODY)
                .font_weight(FontWeight::MEDIUM)
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .child(hit.vm_name.clone()),
        )
        .child(marker(&hit.row))
        .child(sentence(&hit.row))
}

fn results(state: &SearchState, cx: &mut Context<AppView>) -> Div {
    match state {
        SearchState::Idle => quiet(
            "Find where you ran something".to_owned(),
            "Commands, directories, repos, branches, commits, containers, tmux sessions and ports.",
        ),
        SearchState::Searching { query } => quiet(format!("Searching for “{query}”…"), ""),
        SearchState::Empty { query } => quiet(
            format!("No events match “{query}”"),
            "Each word must match the start of a word: upf finds upf-tests.",
        ),
        SearchState::Error { message } => div().child(callout(Tone::Bad).child(message.clone())),
        SearchState::Results { days, count } => {
            let summary = if *count >= SEARCH_LIMIT as usize {
                format!("The newest {count} events")
            } else if *count == 1 {
                "1 event".to_owned()
            } else {
                format!("{count} events, newest first")
            };
            let mut list = div().flex().flex_col().child(label(summary).mb(space::S));
            let mut index = 0;
            for (i, d) in days.iter().enumerate() {
                list = list.child(day_header(&d.label, i == 0));
                for h in &d.hits {
                    list = list.child(hit_row(h, index, cx));
                    index += 1;
                }
            }
            list
        }
    }
}

/// The search pane, in place of the selected VM.
pub fn render(search: &Search, cx: &mut Context<AppView>) -> Div {
    div()
        .px(space::XL)
        .py(space::XL)
        .flex()
        .flex_col()
        .gap(space::L)
        .child(
            div()
                .flex()
                .items_end()
                .gap(space::M)
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(space::XS)
                        .child(
                            div()
                                .text_size(text::TITLE)
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Search events"),
                        )
                        .child(label(
                            "Every VM, newest first. Click an event to open its VM's timeline.",
                        )),
                )
                .child(label("Esc closes")),
        )
        .child(div().max_w(px(640.)).child(search.field.clone()))
        .child(results(&search.state, cx))
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};
    use serde_json::json;
    use vmdesk_db::TimelineEntry;

    use super::*;

    fn hit(vm_name: &str, at: DateTime<Local>, cmdline: &str) -> SearchHit {
        let event = TimelineEntry {
            source: "agent:01a1".into(),
            seq: 1,
            ts: at.with_timezone(&Utc),
            kind: "process_started".into(),
            data: json!({ "pid": 7, "cmdline": cmdline, "cwd": "/root/regal" }),
        };
        SearchHit {
            vm: VmId::new(),
            vm_name: vm_name.into(),
            summary: event.summary(),
            event,
        }
    }

    #[test]
    fn hits_read_as_timeline_sentences_under_their_day() {
        let now = Local.with_ymd_and_hms(2026, 10, 19, 18, 0, 0).unwrap();
        let hits = vec![
            hit("dev93", now - Duration::hours(1), "pytest upf-tests/n4.py"),
            hit("lab", now - Duration::days(6), "pytest upf-tests"),
        ];
        let SearchState::Results { days, count } =
            SearchState::answered("upf".into(), Ok(Some(hits)), now)
        else {
            panic!("results expected")
        };
        assert_eq!(count, 2);
        let shown: Vec<(String, Vec<String>)> = days
            .iter()
            .map(|d| {
                let rows = d
                    .hits
                    .iter()
                    .map(|h| format!("{} {}", h.vm_name, text_of(&h.row.title)))
                    .collect();
                (d.label.clone(), rows)
            })
            .collect();
        assert_eq!(
            shown,
            [
                (
                    "Today".to_owned(),
                    vec!["dev93 Started pytest upf-tests/n4.py".to_owned()]
                ),
                (
                    "Tue 13 Oct".to_owned(),
                    vec!["lab Started pytest upf-tests".to_owned()]
                ),
            ]
        );
    }

    #[test]
    fn nothing_to_search_for_is_quiet_and_no_match_says_so() {
        let now = Local::now();
        assert_eq!(
            SearchState::answered("--".into(), Ok(None), now),
            SearchState::Idle
        );
        assert_eq!(
            SearchState::answered("zzz".into(), Ok(Some(vec![])), now),
            SearchState::Empty {
                query: "zzz".into()
            }
        );
    }
}
