//! The app's building blocks: buttons, pills, cards, tables and menus, drawn with the
//! [`theme`](crate::theme) tokens.

use gpui::{
    BoxShadow, Div, FontWeight, Hsla, SharedString, Stateful, div, hsla, point, prelude::*, px, rgb,
};

use crate::icon::{Icon, icon};
use crate::theme::{CONTROL_HEIGHT, MONO, Tone, alpha, color, radius, space, text, tint};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Button {
    /// The one main action of a view.
    Primary,
    Secondary,
    /// Confirms something that cannot be undone.
    Danger,
}

/// A button with no content yet; add a label (and an icon) as children.
pub fn button(kind: Button, id: &'static str) -> Stateful<Div> {
    let base = div()
        .id(id)
        .debug_selector(|| id.to_owned())
        .h(CONTROL_HEIGHT)
        .px(space::M)
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .rounded(radius::M)
        .border_1()
        .text_size(text::BODY)
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer();
    match kind {
        Button::Primary => base
            .bg(rgb(color::ACCENT))
            .border_color(rgb(color::ACCENT))
            .text_color(rgb(color::ON_ACCENT))
            .hover(|s| {
                s.bg(rgb(color::ACCENT_HOVER))
                    .border_color(rgb(color::ACCENT_HOVER))
            }),
        Button::Secondary => base
            .bg(rgb(color::ELEVATED))
            .border_color(rgb(color::BORDER_STRONG))
            .text_color(rgb(color::TEXT))
            .hover(|s| s.bg(rgb(color::HOVER))),
        Button::Danger => base
            .bg(rgb(color::RED))
            .border_color(rgb(color::RED))
            .text_color(rgb(color::ON_ACCENT))
            .hover(|s| s.opacity(0.9)),
    }
}

/// A square button that shows only `glyph`.
pub fn icon_button(id: &'static str, glyph: Icon, active: bool) -> Stateful<Div> {
    div()
        .id(id)
        .debug_selector(|| id.to_owned())
        .size(CONTROL_HEIGHT)
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded(radius::M)
        .border_1()
        .border_color(rgb(color::BORDER_STRONG))
        .bg(rgb(if active {
            color::HOVER
        } else {
            color::ELEVATED
        }))
        .hover(|s| s.bg(rgb(color::HOVER)))
        .cursor_pointer()
        .child(icon(glyph, px(16.), color::TEXT))
}

/// Small muted text.
pub fn label(content: impl Into<SharedString>) -> Div {
    div()
        .text_size(text::META)
        .text_color(rgb(color::TEXT_MUTED))
        .child(content.into())
}

pub fn dot(c: u32, size: f32) -> Div {
    div()
        .size(px(size))
        .flex_shrink_0()
        .rounded_full()
        .bg(rgb(c))
}

/// A rounded, tinted badge with a leading dot: a status at a glance.
pub fn pill(tone: Tone, content: impl Into<SharedString>) -> Div {
    let c = tone.color();
    div()
        .h(px(22.))
        .px(space::S)
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(6.))
        .rounded_full()
        .bg(tint(c))
        .text_color(rgb(c))
        .text_size(text::META)
        .font_weight(FontWeight::SEMIBOLD)
        .child(dot(c, 6.))
        .child(content.into())
}

/// A count beside a heading.
pub fn count_badge(n: usize) -> Div {
    div()
        .px(px(6.))
        .h(px(18.))
        .flex()
        .items_center()
        .rounded_full()
        .bg(rgb(color::HOVER))
        .text_size(text::SMALL)
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(color::TEXT_SECONDARY))
        .child(n.to_string())
}

/// A raised panel with a hairline border.
pub fn card() -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(radius::L)
        .border_1()
        .border_color(rgb(color::BORDER))
        .bg(rgb(color::SURFACE))
}

/// A tinted box for a question or a problem, in `tone`'s colour.
pub fn callout(tone: Tone) -> Div {
    div()
        .p(space::M)
        .flex()
        .items_center()
        .gap(space::M)
        .rounded(radius::M)
        .border_1()
        .border_color(alpha(tone.color(), 0x80))
        .bg(tint(tone.color()))
        .text_size(text::BODY)
}

/// A table column; the last column takes the remaining width.
pub struct Col {
    pub title: &'static str,
    pub width: f32,
    /// Commands, paths and addresses.
    pub mono: bool,
}

pub const fn col(title: &'static str, width: f32) -> Col {
    Col {
        title,
        width,
        mono: false,
    }
}

pub const fn mono(title: &'static str, width: f32) -> Col {
    Col {
        title,
        width,
        mono: true,
    }
}

fn cells(cols: &[Col], values: Vec<String>, style: impl Fn(&Col, Div) -> Div) -> Div {
    let n = cols.len();
    div()
        .flex()
        .items_center()
        .gap(space::M)
        .children(cols.iter().zip(values).enumerate().map(|(i, (c, v))| {
            let cell = div()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(v);
            let cell = if i + 1 == n {
                cell.flex_1().min_w_0()
            } else {
                cell.w(px(c.width)).flex_shrink_0()
            };
            style(c, cell)
        }))
}

pub fn table(cols: &[Col], rows: Vec<Vec<String>>) -> Div {
    let head = cells(
        cols,
        cols.iter().map(|c| c.title.to_owned()).collect(),
        |_, d| d,
    )
    .h(px(30.))
    .text_size(text::META)
    .font_weight(FontWeight::MEDIUM)
    .text_color(rgb(color::TEXT_MUTED))
    .border_b_1()
    .border_color(rgb(color::BORDER));
    let n = rows.len();
    div()
        .flex()
        .flex_col()
        .child(head)
        .children(rows.into_iter().enumerate().map(|(i, r)| {
            cells(cols, r, |c, d| {
                if c.mono {
                    d.font_family(MONO).text_size(text::META)
                } else {
                    d
                }
            })
            .h(px(30.))
            .text_size(text::BODY)
            .when(i + 1 < n, |d| {
                d.border_b_1().border_color(rgb(color::BORDER))
            })
        }))
}

fn shadow() -> Vec<BoxShadow> {
    let black = |a: f32| -> Hsla { hsla(0., 0., 0., a) };
    vec![
        BoxShadow {
            color: black(0.35),
            offset: point(px(0.), px(8.)),
            blur_radius: px(24.),
            spread_radius: px(0.),
            inset: false,
        },
        BoxShadow {
            color: black(0.25),
            offset: point(px(0.), px(1.)),
            blur_radius: px(3.),
            spread_radius: px(0.),
            inset: false,
        },
    ]
}

/// The halo around a focused input.
pub fn focus_ring() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: tint(color::ACCENT).into(),
        offset: point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: px(2.),
        inset: false,
    }]
}

/// A popover menu's panel.
pub fn menu() -> Div {
    div()
        .min_w(px(220.))
        .py(space::XS)
        .flex()
        .flex_col()
        .rounded(radius::L)
        .border_1()
        .border_color(rgb(color::BORDER_STRONG))
        .bg(rgb(color::ELEVATED))
        .shadow(shadow())
}

pub fn menu_separator() -> Div {
    div().my(space::XS).h(px(1.)).bg(rgb(color::BORDER))
}

/// A menu row. A disabled one is drawn muted and has no id to click.
pub fn menu_item(id: &'static str, content: &'static str, danger: bool, enabled: bool) -> Div {
    let text_color = match (enabled, danger) {
        (false, _) => color::TEXT_MUTED,
        (true, true) => color::RED,
        (true, false) => color::TEXT,
    };
    div()
        .mx(space::XS)
        .h(px(30.))
        .px(space::S)
        .flex()
        .items_center()
        .rounded(radius::S)
        .text_size(text::BODY)
        .text_color(rgb(text_color))
        .child(content)
        .when(enabled, |d| d.debug_selector(|| id.to_owned()))
}

/// The clickable version of [`menu_item`].
pub fn menu_action(id: &'static str, content: &'static str, danger: bool) -> Stateful<Div> {
    menu_item(id, content, danger, true)
        .id(id)
        .cursor_pointer()
        .hover(|s| s.bg(rgb(color::HOVER)))
}
