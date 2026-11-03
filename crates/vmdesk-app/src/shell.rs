//! The window's frame around the selected machine: the top bar, and the machine list as a
//! sidebar grouped by state or, collapsed, a rail of initials.

use gpui::{AnyElement, Context, FontWeight, SharedString, Window, div, prelude::*, px, rgb};
use vmdesk_core::{Headline, Vm};

use super::{AppView, headline_tone};
use crate::icon::{Icon, icon};
use crate::kit::{dot, label};
use crate::theme::{CONTROL_HEIGHT, palette, radius, space, text, tint, tone_color};

pub(super) const SIDEBAR_WIDTH: f32 = 264.;
const RAIL_WIDTH: f32 = 56.;

/// Where a machine sits in the list: what needs you comes right after what works.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Group {
    Online,
    Attention,
    Offline,
    Unchecked,
}

impl Group {
    pub(super) fn of(headline: Option<Headline>) -> Self {
        match headline {
            Some(Headline::Online) => Self::Online,
            Some(Headline::SshFailing) => Self::Attention,
            Some(Headline::NoRoute | Headline::Stopped) => Self::Offline,
            Some(Headline::Unknown) | None => Self::Unchecked,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Online => "Online",
            Self::Attention => "Needs attention",
            Self::Offline => "Offline",
            Self::Unchecked => "Not checked",
        }
    }
}

impl AppView {
    /// The bar across the top: the sidebar toggle, the app, the selected machine, search and
    /// the theme picker. Where the app draws its own window frame, it also moves the window
    /// and holds the window buttons.
    pub(super) fn top_bar(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let machine = self
            .selected
            .and_then(|id| self.vms.iter().find(|v| v.id == id))
            .map(|vm| {
                div()
                    .h(px(28.))
                    .px(space::S)
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .rounded(radius::M)
                    .bg(rgb(palette().elevated))
                    .border_1()
                    .border_color(rgb(palette().border))
                    .child(dot(tone_color(headline_tone(self.headline(vm))), 7.))
                    .child(
                        div()
                            .text_size(text::META)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(vm.name.clone()),
                    )
            });
        let leading = div()
            .flex()
            .items_center()
            .gap(space::S)
            .child(self.machines_button("toggle-machines-title", cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .pr(space::S)
                    .child(app_mark())
                    .child(
                        div()
                            .text_size(text::BODY)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("VMDesk"),
                    ),
            )
            .children(machine)
            .when(self.selected.is_some(), |d| {
                d.child(div().w(space::S)).child(self.view_switch(cx))
            });
        let trailing = div()
            .flex()
            .items_center()
            .gap(space::S)
            .child(self.search_button(cx))
            .child(self.themes_button(cx));
        crate::chrome::top_bar(
            window,
            leading.into_any_element(),
            trailing.into_any_element(),
        )
    }

    fn search_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.search.is_some();
        div()
            .id("open-search")
            .debug_selector(|| "open-search".to_owned())
            .h(px(28.))
            .px(space::S)
            .flex()
            .items_center()
            .gap(space::S)
            .rounded(radius::M)
            .border_1()
            .border_color(rgb(if open {
                palette().accent
            } else {
                palette().border
            }))
            .bg(rgb(palette().bg))
            .cursor_pointer()
            .text_size(text::META)
            .text_color(rgb(palette().text_secondary))
            .hover(|s| s.bg(rgb(palette().hover)))
            .on_click(cx.listener(|this, _, window, cx| this.open_search(window, cx)))
            .child(icon(Icon::Search, px(13.), palette().text_muted))
            .child("Search events")
            .child(
                div()
                    .ml(space::M)
                    .text_size(text::SMALL)
                    .text_color(rgb(palette().text_muted))
                    .child("Ctrl K"),
            )
    }

    /// The machine list: a filter and Add, then the machines under their group.
    pub(super) fn sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let vms = self.visible();
        let none_match = vms.is_empty() && !self.vms.is_empty();
        let mut groups: Vec<(Group, Vec<&Vm>)> = Vec::new();
        for vm in vms {
            let group = Group::of(self.headline(vm));
            match groups.iter_mut().find(|(g, _)| *g == group) {
                Some((_, members)) => members.push(vm),
                None => groups.push((group, vec![vm])),
            }
        }
        groups.sort_by_key(|(g, _)| *g);
        div()
            .w(px(SIDEBAR_WIDTH))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(palette().surface))
            .border_r_1()
            .border_color(rgb(palette().border))
            .child(
                div()
                    .p(space::M)
                    .flex()
                    .items_center()
                    .gap(space::S)
                    .child(div().flex_1().min_w_0().child(self.filter_input(cx)))
                    .child(
                        div()
                            .id("add-vm")
                            .debug_selector(|| "add-vm".to_owned())
                            .size(CONTROL_HEIGHT)
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(radius::M)
                            .bg(rgb(palette().accent))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(palette().accent_hover)))
                            .child(icon(Icon::Plus, px(15.), palette().on_accent))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_add_form(window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .id("vm-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .px(space::S)
                    .pb(space::M)
                    .flex()
                    .flex_col()
                    .gap(space::M)
                    .when(none_match, |d| {
                        d.child(
                            div()
                                .pt(space::L)
                                .flex()
                                .justify_center()
                                .child(label(format!("No machines match “{}”", self.filter))),
                        )
                    })
                    .children(groups.into_iter().map(|(group, members)| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .px(space::S)
                                    .pb(px(4.))
                                    .text_size(text::SMALL)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(palette().text_muted))
                                    .child(format!(
                                        "{} · {}",
                                        group.title().to_uppercase(),
                                        members.len()
                                    )),
                            )
                            .children(members.into_iter().map(|vm| self.list_row(vm, cx)))
                    })),
            )
            .into_any_element()
    }

    /// The collapsed machine list: each machine's initials with its status, and Add.
    pub(super) fn rail(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut vms: Vec<&Vm> = self.visible();
        vms.sort_by_key(|vm| Group::of(self.headline(vm)));
        div()
            .id("rail")
            .debug_selector(|| "rail".to_owned())
            .w(px(RAIL_WIDTH))
            .flex_shrink_0()
            .h_full()
            .py(space::M)
            .flex()
            .flex_col()
            .items_center()
            .gap(px(6.))
            .bg(rgb(palette().surface))
            .border_r_1()
            .border_color(rgb(palette().border))
            .children(vms.into_iter().map(|vm| {
                let id = vm.id;
                let selected = self.selected == Some(id);
                let initials: String = vm
                    .name
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .take(2)
                    .collect();
                div()
                    .id(SharedString::from(format!("rail-{id}")))
                    .size(px(38.))
                    .relative()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(radius::L)
                    .cursor_pointer()
                    .text_size(text::META)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(if selected {
                        palette().text
                    } else {
                        palette().text_secondary
                    }))
                    .when(selected, |d| d.bg(tint(palette().accent)))
                    .when(!selected, |d| d.hover(|s| s.bg(rgb(palette().hover))))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.close_search(window, cx);
                        this.select(id, cx);
                    }))
                    .child(initials)
                    .child(
                        div()
                            .absolute()
                            .right(px(3.))
                            .bottom(px(3.))
                            .size(px(9.))
                            .rounded_full()
                            .border_2()
                            .border_color(rgb(palette().surface))
                            .bg(rgb(tone_color(headline_tone(self.headline(vm))))),
                    )
            }))
            .child(div().flex_1())
            .child(
                div()
                    .id("rail-add-vm")
                    .size(px(38.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(radius::L)
                    .bg(rgb(palette().accent))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(palette().accent_hover)))
                    .child(icon(Icon::Plus, px(15.), palette().on_accent))
                    .on_click(cx.listener(|this, _, window, cx| this.open_add_form(window, cx))),
            )
            .into_any_element()
    }
}

/// The app's mark, the launcher icon in small: a terminal prompt on a tile.
fn app_mark() -> impl IntoElement {
    div()
        .size(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .bg(rgb(palette().hover))
        .child(icon(Icon::Terminal, px(13.), palette().accent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_needs_you_comes_right_after_what_works() {
        assert!(Group::of(Some(Headline::Online)) < Group::of(Some(Headline::SshFailing)));
        assert!(Group::of(Some(Headline::SshFailing)) < Group::of(Some(Headline::NoRoute)));
        assert_eq!(Group::of(Some(Headline::Stopped)), Group::Offline);
        assert_eq!(Group::of(None), Group::Unchecked);
    }
}
