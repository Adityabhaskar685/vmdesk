//! The terminal core behind VMDesk's embedded terminal.
//!
//! libghostty-vt, Ghostty's terminal implementation, owns escape-sequence parsing, the
//! screen and scrollback model, selection and input encoding. This crate exposes only the
//! safe part the app draws with; no libghostty type or pointer leaves it.
//!
//! All `unsafe` code is confined to [`sys`], the module that calls the C API.
//!
//! The design follows Rex's GPUI client, which uses the same library for its terminal:
//! a [`Terminal`] is fed output bytes, a [`RenderState`] copies the screen for drawing, and
//! [`KeyEncoder`], [`MouseEncoder`], [`encode_paste`] and [`encode_focus`] turn input into
//! the bytes a program on the other side of the pty expects.

#[allow(unsafe_code)]
mod sys;

pub use sys::{
    Cell, Cursor, CursorShape, Frame, KeyEncoder, MouseEncoder, RenderState, Scrollbar, Terminal,
    Width, encode_focus, encode_paste,
};

/// A point on screen, in cells from the top-left of the visible area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewportPoint {
    pub x: u16,
    pub y: u16,
}

/// How to move the viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    /// Relative rows; positive scrolls toward newer output.
    Delta(isize),
    /// The newest output.
    Bottom,
    /// The first visible row, as in [`Scrollbar::offset`].
    Row(usize),
}

/// An RGB color, as libghostty-vt reports and accepts it.
pub type Rgb = (u8, u8, u8);

/// Keyboard modifiers, with the extra state the Kitty protocol can report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub super_key: bool,
    pub caps_lock: bool,
    pub num_lock: bool,
}

impl Mods {
    pub(crate) fn bits(self) -> u16 {
        let mut bits = 0;
        if self.shift {
            bits |= 1 << 0;
        }
        if self.ctrl {
            bits |= 1 << 1;
        }
        if self.alt {
            bits |= 1 << 2;
        }
        if self.super_key {
            bits |= 1 << 3;
        }
        if self.caps_lock {
            bits |= 1 << 4;
        }
        if self.num_lock {
            bits |= 1 << 5;
        }
        bits
    }
}

/// Whether a key was pressed, released or is repeating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    Press,
    Release,
    Repeat,
}

/// A physical key, named as in the W3C UI Events `code` standard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key(i32);

impl Key {
    /// The key for a W3C `code`, or [`Key::UNIDENTIFIED`] for a name this library does not
    /// know (a symbol from another layout, for example).
    #[must_use]
    pub fn from_code(code: &str) -> Self {
        Self(sys::key_from_code(code))
    }

    pub const UNIDENTIFIED: Self = Self(0);
}

/// One key press, with the text the layout produced.
#[derive(Clone, Copy, Debug)]
pub struct KeyEvent<'a> {
    pub key: Key,
    pub action: KeyAction,
    pub mods: Mods,
    /// Modifiers the layout already spent producing `utf8` (usually shift for `!` or `A`).
    pub consumed_mods: Mods,
    /// The character the key produced before Ctrl/Meta transformations, if printable.
    pub utf8: Option<&'a str>,
    /// The unshifted character on the current layout.
    pub unshifted_codepoint: Option<char>,
    pub composing: bool,
}

/// What a mouse event did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

/// The mouse button an event is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Four,
    Five,
    Six,
    Seven,
    Eight,
    Nine,
    Ten,
    Eleven,
}

/// One mouse event, positioned in pixels from the top-left of the terminal grid.
#[derive(Clone, Copy, Debug)]
pub struct MouseEvent {
    pub action: MouseAction,
    /// `None` for a motion event with no button held.
    pub button: Option<MouseButton>,
    pub mods: Mods,
    pub x: f32,
    pub y: f32,
}
