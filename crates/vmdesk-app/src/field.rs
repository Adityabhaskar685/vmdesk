//! A minimal single-line text field: typing, Backspace, Ctrl+V and click-to-focus.
//! Tab, Enter and Escape are left to the surrounding form. No selection or cursor
//! movement; text is only appended or removed at the end.

use gpui::{
    Context, FocusHandle, KeyDownEvent, MouseButton, SharedString, Window, div, prelude::*, px, rgb,
};

use crate::ui::color;

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
            .h(px(30.))
            .px_2()
            .flex()
            .items_center()
            .rounded_md()
            .border_1()
            .border_color(rgb(if focused {
                color::ACCENT
            } else {
                color::BORDER
            }))
            .bg(rgb(color::BG))
            .text_sm()
            .overflow_hidden()
            .whitespace_nowrap()
            .cursor_text()
            .child(match shown {
                Some(t) => div().child(if focused { format!("{t}▏") } else { t }),
                None => div().text_color(rgb(color::DIM)).child(if focused {
                    format!("▏{}", self.placeholder)
                } else {
                    self.placeholder.to_string()
                }),
            })
    }
}
