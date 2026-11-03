//! The theme picker: every built-in and Ghostty theme, filtered as you type. Clicking one
//! applies it at once and saves it for the next start.

use std::sync::Arc;

use gpui::{
    AnyElement, Context, Div, Entity, FontWeight, SharedString, Subscription, div, prelude::*, px,
    rgb, uniform_list,
};

use crate::field::TextField;
use crate::icon::{Icon, icon};
use crate::kit::label;
use crate::theme::palette::active_name;
use crate::theme::{Theme, palette, radius, space, text, themes, tint};
use crate::ui::AppView;

const ROW_HEIGHT: f32 = 36.;

pub struct ThemePicker {
    pub field: Entity<TextField>,
    /// The themes the filter matches, in the registry's order.
    pub matches: Vec<Arc<Theme>>,
    _edits: Subscription,
}

impl ThemePicker {
    pub fn new(cx: &mut Context<AppView>) -> Self {
        crate::theme::palette::ensure_all_loaded();
        let field = cx.new(|cx| TextField::new("Filter themes", false, cx));
        let edits = cx.observe(&field, |this, field, cx| {
            let query = field.read(cx).text().to_owned();
            if let Some(picker) = this.themes.as_mut() {
                picker.matches = matching(&query);
            }
            cx.notify();
        });
        Self {
            field,
            matches: matching(""),
            _edits: edits,
        }
    }
}

/// The themes whose names contain every word of `query`, ignoring case.
#[must_use]
pub fn matching(query: &str) -> Vec<Arc<Theme>> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    themes()
        .into_iter()
        .filter(|t| {
            let name = t.name.to_lowercase();
            words.iter().all(|w| name.contains(w))
        })
        .collect()
}

pub fn render(picker: &ThemePicker, cx: &mut Context<AppView>) -> Div {
    let count = picker.matches.len();
    let list = uniform_list(
        "theme-list",
        count,
        cx.processor(|this: &mut AppView, range: std::ops::Range<usize>, _, cx| {
            let Some(picker) = this.themes.as_ref() else {
                return Vec::new();
            };
            let active = active_name();
            picker.matches[range]
                .iter()
                .map(|theme| row(theme, theme.name == active, cx))
                .collect::<Vec<_>>()
        }),
    )
    .flex_1()
    .min_h_0();
    div()
        .size_full()
        .px(space::XL)
        .pt(space::XL)
        .pb(space::M)
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
                                .child("Theme"),
                        )
                        .child(label(format!(
                            "{} themes: VMDesk's own and Ghostty's. Click one to use it.",
                            themes().len()
                        ))),
                )
                .child(label("Esc closes")),
        )
        .child(div().max_w(px(640.)).child(picker.field.clone()))
        .child(if count == 0 {
            div()
                .text_color(rgb(palette().text_muted))
                .child("No theme matches")
        } else {
            div().flex_1().min_h_0().flex().flex_col().child(list)
        })
}

fn row(theme: &Arc<Theme>, active: bool, cx: &mut Context<AppView>) -> AnyElement {
    let p = palette();
    let t = theme.palette;
    let name = theme.name.clone();
    let id = SharedString::from(format!("theme-{}", theme.name));
    let swatches = [
        t.term.background,
        t.term.foreground,
        t.term.ansi[1],
        t.term.ansi[2],
        t.term.ansi[3],
        t.term.ansi[4],
        t.term.ansi[5],
        t.term.ansi[6],
    ];
    let tag = |s: &'static str| {
        div()
            .px(px(6.))
            .h(px(18.))
            .flex()
            .items_center()
            .rounded(radius::S)
            .border_1()
            .border_color(rgb(p.border_strong))
            .text_size(text::SMALL)
            .text_color(rgb(p.text_muted))
            .child(s)
    };
    div()
        .id(id.clone())
        .debug_selector(move || id.to_string())
        .h(px(ROW_HEIGHT))
        .px(space::S)
        .flex()
        .items_center()
        .gap(space::M)
        .rounded(radius::M)
        .cursor_pointer()
        .when(active, |d| d.bg(tint(p.accent)))
        .when(!active, |d| d.hover(|s| s.bg(rgb(p.hover))))
        .on_click(cx.listener(move |this, _, _, cx| this.apply_theme(&name, cx)))
        .child(
            div()
                .flex()
                .flex_shrink_0()
                .rounded(radius::S)
                .overflow_hidden()
                .border_1()
                .border_color(rgb(p.border_strong))
                .children(swatches.map(|c| div().size(px(14.)).bg(rgb(c)))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(rgb(if active { p.text } else { p.text_secondary }))
                .when(active, |d| d.font_weight(FontWeight::SEMIBOLD))
                .child(theme.name.clone()),
        )
        .when(theme.source == crate::theme::Source::BuiltIn, |d| {
            d.child(tag("built-in"))
        })
        .when(theme.is_light(), |d| d.child(tag("light")))
        .child(
            div()
                .w(px(16.))
                .when(active, |d| d.child(icon(Icon::Check, px(14.), p.accent))),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_word_must_match_ignoring_case() {
        let all = matching("");
        assert!(all.len() >= 2, "the built-in themes are always there");
        let light = matching("vmdesk LIGHT");
        assert_eq!(light.len(), 1);
        assert_eq!(light[0].name, "VMDesk Light");
        assert!(matching("no such theme anywhere").is_empty());
    }
}
