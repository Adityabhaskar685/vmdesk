//! The window frame the app draws itself. GNOME does not decorate Wayland windows, so
//! the title bar (move, maximise on double-click, window menu on right-click), the
//! minimise / maximise / close buttons and the resize edges are ours.

use gpui::{
    AnyElement, App, Bounds, CursorStyle, Decorations, HitboxBehavior, MouseButton, Pixels, Point,
    ResizeEdge, Size, Window, canvas, div, point, prelude::*, px, rgb,
};

use crate::icon::{Icon, icon};
use crate::theme::{palette, text};

/// Width of the strip along each window edge that resizes the window.
const EDGE: Pixels = px(6.);
const TITLE_HEIGHT: Pixels = px(44.);

/// Which edge or corner `pos` is on, if any.
fn resize_edge(pos: Point<Pixels>, size: Size<Pixels>) -> Option<ResizeEdge> {
    let (top, bottom) = (pos.y < EDGE, pos.y > size.height - EDGE);
    let (left, right) = (pos.x < EDGE, pos.x > size.width - EDGE);
    Some(match (top, bottom, left, right) {
        (true, _, true, _) => ResizeEdge::TopLeft,
        (true, _, _, true) => ResizeEdge::TopRight,
        (_, true, true, _) => ResizeEdge::BottomLeft,
        (_, true, _, true) => ResizeEdge::BottomRight,
        (true, ..) => ResizeEdge::Top,
        (_, true, ..) => ResizeEdge::Bottom,
        (_, _, true, _) => ResizeEdge::Left,
        (_, _, _, true) => ResizeEdge::Right,
        _ => return None,
    })
}

fn cursor_for(edge: ResizeEdge) -> CursorStyle {
    match edge {
        ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
        ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
        ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
        ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
    }
}

/// Whether the app draws the frame (the compositor did not).
fn client_decorated(window: &Window) -> bool {
    matches!(window.window_decorations(), Decorations::Client { .. })
}

/// Wraps the window's content with resize edges, when the app draws the frame.
pub fn frame(window: &Window, content: AnyElement) -> AnyElement {
    if !client_decorated(window) || window.is_maximized() {
        return content;
    }
    div()
        .size_full()
        .relative()
        .child(content)
        .child(
            // Shows the resize cursor near the edges; drawn over the content.
            canvas(
                |_, window, _| {
                    let size = window.window_bounds().get_bounds().size;
                    window.insert_hitbox(
                        Bounds::new(point(px(0.), px(0.)), size),
                        HitboxBehavior::Normal,
                    )
                },
                |_, hitbox, window, _| {
                    let size = window.window_bounds().get_bounds().size;
                    if let Some(edge) = resize_edge(window.mouse_position(), size) {
                        window.set_cursor_style(cursor_for(edge), &hitbox);
                    }
                },
            )
            .size_full()
            .absolute()
            .top_0()
            .left_0(),
        )
        .on_mouse_move(|_, window, _| window.refresh())
        .on_mouse_down(MouseButton::Left, |e, window, cx| {
            let size = window.window_bounds().get_bounds().size;
            if let Some(edge) = resize_edge(e.position, size) {
                window.start_window_resize(edge);
                cx.stop_propagation();
            }
        })
        .into_any_element()
}

fn control(id: &'static str, glyph: Icon, close: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .w(px(40.))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .text_size(text::BODY)
        .text_color(rgb(palette().text_secondary))
        .cursor_pointer()
        .hover(move |s| {
            s.bg(rgb(if close {
                palette().red
            } else {
                palette().hover
            }))
            .text_color(rgb(palette().text))
        })
        .child(icon(glyph, px(14.), palette().text_secondary))
}

/// The bar across the top of the window, between `leading` and `trailing`. Where the app
/// draws its own frame (GNOME on Wayland), the space between them moves the window and the
/// window buttons follow `trailing`; otherwise the compositor's title bar does both.
pub fn top_bar(window: &Window, leading: AnyElement, trailing: AnyElement) -> AnyElement {
    let decorated = client_decorated(window);
    let maximized = window.is_maximized();
    let drag = div()
        .id("title-drag")
        .flex_1()
        .h_full()
        .when(decorated, |d| {
            d.on_mouse_down(MouseButton::Left, |e, window, _| {
                if e.click_count >= 2 {
                    window.zoom_window();
                } else {
                    window.start_window_move();
                }
            })
            .on_mouse_down(MouseButton::Right, |e, window, _| {
                window.show_window_menu(e.position);
            })
        });
    div()
        .h(TITLE_HEIGHT)
        .w_full()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(8.))
        .bg(rgb(palette().surface))
        .border_b_1()
        .border_color(rgb(palette().border))
        .pl(px(8.))
        .when(!decorated, |d| d.pr(px(8.)))
        .child(leading)
        .child(drag)
        .child(trailing)
        .when(decorated, |d| {
            d.child(div().w(px(4.)))
                .child(
                    control("win-minimize", Icon::WinMinimize, false)
                        .on_click(|_, window, _| window.minimize_window()),
                )
                .child(
                    control(
                        "win-maximize",
                        if maximized {
                            Icon::WinRestore
                        } else {
                            Icon::WinMaximize
                        },
                        false,
                    )
                    .on_click(|_, window, _| window.zoom_window()),
                )
                .child(
                    control("win-close", Icon::Close, true)
                        .on_click(|_, _, cx: &mut App| cx.quit()),
                )
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use gpui::size;

    use super::*;

    #[test]
    fn edges_and_corners() {
        let s = size(px(800.), px(600.));
        assert_eq!(resize_edge(point(px(400.), px(300.)), s), None);
        assert_eq!(
            resize_edge(point(px(2.), px(2.)), s),
            Some(ResizeEdge::TopLeft)
        );
        assert_eq!(
            resize_edge(point(px(798.), px(598.)), s),
            Some(ResizeEdge::BottomRight)
        );
        assert_eq!(
            resize_edge(point(px(400.), px(1.)), s),
            Some(ResizeEdge::Top)
        );
        assert_eq!(
            resize_edge(point(px(799.), px(300.)), s),
            Some(ResizeEdge::Right)
        );
    }
}
