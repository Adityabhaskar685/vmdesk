//! The terminal's inspector: the machine's state beside its shells, so you need not leave
//! the terminal to see what runs there.

use chrono::Utc;
use gpui::{AnyElement, Context, Div, FontWeight, div, prelude::*, px, rgb};
use vmdesk_collect::work::CurrentWork;
use vmdesk_core::human::ago;
use vmdesk_core::{Headline, Vm};
use vmdesk_proto::Snapshot;

use super::{AppView, Tab, headline_tone, sentence_case};
use crate::kit::{dot, label, pill};
use crate::theme::{MONO, palette, radius, space, text, tone_color};

pub(super) const WIDTH: f32 = 280.;

impl AppView {
    pub(super) fn inspector(&self, cx: &mut Context<Self>) -> AnyElement {
        let vm = self
            .selected
            .and_then(|id| self.vms.iter().find(|v| v.id == id));
        let detail = self.detail.as_ref();
        let snapshot = detail.and_then(|d| d.snapshot.as_ref());
        let work = detail
            .and_then(|d| d.work.as_ref())
            .filter(|w| w.anchor.is_some())
            .map(CurrentWork::line);
        let taken = snapshot
            .and_then(|s| s.taken_at)
            .map(|t| format!("Snapshot {} ago", ago((Utc::now() - t).num_seconds())));
        div()
            .debug_selector(|| "inspector".to_owned())
            .w(px(WIDTH))
            .flex_shrink_0()
            .p(space::L)
            .flex()
            .flex_col()
            .gap(space::M)
            .rounded(radius::L)
            .bg(rgb(palette().surface))
            .border_1()
            .border_color(rgb(palette().border))
            .children(vm.map(|vm| self.inspector_header(vm)))
            .child(working_on(work))
            .children(snapshot.map(figures))
            .children(snapshot.map(|s| {
                label(format!(
                    "{} tmux · {} containers · {} ports",
                    s.tmux.len(),
                    s.containers.len(),
                    s.listeners.len()
                ))
            }))
            .child(
                div()
                    .flex()
                    .gap(space::S)
                    .child(Self::view_link(
                        "inspector-overview",
                        "Overview",
                        Tab::Overview,
                        cx,
                    ))
                    .child(Self::view_link(
                        "inspector-timeline",
                        "Timeline",
                        Tab::Timeline,
                        cx,
                    )),
            )
            .child(div().flex_1())
            .children(taken.map(label))
            .into_any_element()
    }

    fn inspector_header(&self, vm: &Vm) -> Div {
        let headline: Option<Headline> = self.headline(vm);
        let tone = headline_tone(headline);
        let state = self.status.get(&vm.id).map(|s| s.label(vm.lifecycle));
        div()
            .flex()
            .items_center()
            .gap(space::S)
            .child(dot(tone_color(tone), 8.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(text::SECTION)
                    .font_weight(FontWeight::BOLD)
                    .child(vm.name.clone()),
            )
            .children(state.map(|s| pill(tone, sentence_case(&s))))
    }

    fn view_link(
        id: &'static str,
        title: &'static str,
        tab: Tab,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .h(px(26.))
            .px(space::S)
            .flex()
            .items_center()
            .rounded(radius::M)
            .bg(rgb(palette().elevated))
            .cursor_pointer()
            .text_size(text::META)
            .text_color(rgb(palette().text_secondary))
            .hover(|s| s.bg(rgb(palette().hover)).text_color(rgb(palette().text)))
            .child(title)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_tab(tab, window, cx);
            }))
    }
}

fn working_on(work: Option<String>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .text_size(text::SMALL)
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(palette().accent))
                .child("WORKING ON"),
        )
        .child(match work {
            Some(what) => div()
                .font_family(MONO)
                .text_size(text::META)
                .text_color(rgb(palette().text))
                .child(what),
            None => label("Nothing of yours in the last snapshot."),
        })
}

/// Load, memory and the fullest disk, side by side.
fn figures(s: &Snapshot) -> Div {
    let figure = |caption: &'static str, value: String| {
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(label(caption))
            .child(
                div()
                    .text_size(text::BODY)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(value),
            )
    };
    let percent = |used: u64, total: u64| {
        (used * 100)
            .checked_div(total)
            .map_or_else(|| "—".to_owned(), |p| format!("{p}%"))
    };
    let sys = &s.system;
    let fullest = sys
        .disks
        .iter()
        .max_by_key(|d| (d.used_kb * 100).checked_div(d.total_kb).unwrap_or(0));
    div()
        .flex()
        .gap(space::L)
        .child(figure("Load", format!("{:.2}", sys.load[0])))
        .child(figure("Memory", percent(sys.mem_used_kb, sys.mem_total_kb)))
        .children(fullest.map(|d| figure("Fullest disk", percent(d.used_kb, d.total_kb))))
}
