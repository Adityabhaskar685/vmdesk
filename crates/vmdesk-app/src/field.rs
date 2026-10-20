//! A minimal single-line text field: typing, Backspace, Ctrl+V and click-to-focus.
//! Tab, Enter and Escape are left to the surrounding form. No selection or cursor
//! movement; text is only appended or removed at the end.

use gpui::{
    Context, FocusHandle, KeyDownEvent, MouseButton, SharedString, Window, div, prelude::*, px, rgb,
};

use crate::kit::focus_ring;
use crate::theme::{color, radius, text};

pub struct TextField {
    focus: FocusHandle,
    text: String,
    placeholder: SharedString,
    /// Shown as dots (passwords).
    secret: bool,
}

impl TextField {
    pub fn new(placeholder: &str, secret: bool, cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            text: String::new(),
            placeholder: placeholder.to_owned().into(),
            secret,
        }
    }

    /// Sets the initial text (when editing).
    pub fn with_text(mut self, text: &str) -> Self {
        text.clone_into(&mut self.text);
        self
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        if ks.modifiers.control && ks.key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                self.text.push_str(text.lines().next().unwrap_or_default());
            }
        } else if ks.modifiers.control || ks.modifiers.alt || ks.modifiers.platform {
            return;
        } else if ks.key == "backspace" {
            self.text.pop();
        } else {
            match &ks.key_char {
                Some(c) if !c.chars().any(char::is_control) => self.text.push_str(c),
                // Tab, Enter, Escape, arrows: for the form.
                _ => return,
            }
        }
        cx.stop_propagation();
        cx.notify();
    }
}

impl Render for TextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus.is_focused(window);
        let shown = if self.text.is_empty() {
            None
        } else if self.secret {
            Some("•".repeat(self.text.chars().count()))
        } else {
            Some(self.text.clone())
        };
        let caret = || div().w(px(1.5)).h(px(15.)).bg(rgb(color::ACCENT));
        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus, cx);
                    cx.stop_propagation();
                }),
            )
            .w_full()
            .h(px(32.))
            .px(px(10.))
            .flex()
            .items_center()
            .rounded(radius::M)
            .border_1()
            .border_color(rgb(if focused {
                color::ACCENT
            } else {
                color::BORDER_STRONG
            }))
            .when(focused, |d| d.shadow(focus_ring()))
            .bg(rgb(color::SURFACE))
            .text_size(text::BODY)
            .overflow_hidden()
            .whitespace_nowrap()
            .cursor_text()
            .child(match shown {
                Some(t) => div()
                    .flex()
                    .items_center()
                    .gap(px(1.))
                    .child(t)
                    .when(focused, |d| d.child(caret())),
                None => div()
                    .flex()
                    .items_center()
                    .gap(px(1.))
                    .text_color(rgb(color::TEXT_MUTED))
                    .when(focused, |d| d.child(caret()))
                    .child(self.placeholder.clone()),
            })
    }
}
