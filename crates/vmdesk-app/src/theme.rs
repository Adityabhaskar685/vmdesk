//! The design tokens every view draws with: colours, spacing, radii and text sizes.

use gpui::{Pixels, Rgba, px, rgba};

#[allow(clippy::unreadable_literal)] // hex colours read best as written
pub mod color {
    /// The content area behind the selected VM.
    pub const BG: u32 = 0x11111b;
    /// The sidebar and title bar.
    pub const SURFACE: u32 = 0x181825;
    /// Cards, buttons and menus, one step above their background.
    pub const ELEVATED: u32 = 0x1e1e2e;
    pub const HOVER: u32 = 0x313244;
    pub const BORDER: u32 = 0x2a2a3c;
    /// Outlines of controls, which need more contrast than dividers.
    pub const BORDER_STRONG: u32 = 0x45475a;

    pub const TEXT: u32 = 0xcdd6f4;
    pub const TEXT_SECONDARY: u32 = 0xa6adc8;
    pub const TEXT_MUTED: u32 = 0x6c7086;

    pub const ACCENT: u32 = 0x89b4fa;
    pub const ACCENT_HOVER: u32 = 0xb4befe;
    /// Text on the light accent colours.
    pub const ON_ACCENT: u32 = 0x11111b;

    pub const GREEN: u32 = 0xa6e3a1;
    pub const AMBER: u32 = 0xf9e2af;
    pub const RED: u32 = 0xf38ba8;
    pub const GREY: u32 = 0x7f849c;

    /// Timeline categories that carry no good/bad meaning.
    pub const BLUE: u32 = 0x89b4fa;
    pub const TEAL: u32 = 0x94e2d5;
    pub const PURPLE: u32 = 0xcba6f7;
    pub const ORANGE: u32 = 0xfab387;
    pub const PINK: u32 = 0xf5c2e7;
}

pub mod space {
    use gpui::{Pixels, px};

    pub const XS: Pixels = px(4.);
    pub const S: Pixels = px(8.);
    pub const M: Pixels = px(12.);
    pub const L: Pixels = px(16.);
    pub const XL: Pixels = px(24.);
}

pub mod radius {
    use gpui::{Pixels, px};

    pub const S: Pixels = px(6.);
    pub const M: Pixels = px(8.);
    pub const L: Pixels = px(12.);
}

pub mod text {
    use gpui::{Pixels, px};

    pub const TITLE: Pixels = px(20.);
    pub const SECTION: Pixels = px(14.);
    pub const BODY: Pixels = px(13.);
    pub const META: Pixels = px(12.);
    pub const SMALL: Pixels = px(11.);
}

/// The UI font. It needs real weight faces: GPUI cannot pick weights from a variable font,
/// and the system default here (Ubuntu) is one.
pub const SANS: &str = "Noto Sans";
/// For commands, paths and addresses only.
pub const MONO: &str = "Noto Sans Mono";
/// Height of buttons and inputs.
pub const CONTROL_HEIGHT: Pixels = px(32.);

pub use vmdesk_db::Tone;

pub fn tone_color(tone: Tone) -> u32 {
    match tone {
        Tone::Good => color::GREEN,
        Tone::Warn => color::AMBER,
        Tone::Bad => color::RED,
        Tone::Neutral => color::GREY,
    }
}

pub fn alpha(c: u32, a: u8) -> Rgba {
    rgba((c << 8) | u32::from(a))
}

/// `c` at low opacity: the background of a pill or badge in that colour.
pub fn tint(c: u32) -> Rgba {
    alpha(c, 0x26)
}
