//! The design tokens every view draws with: colours, spacing, radii and text sizes.

use gpui::{Pixels, Rgba, px, rgba};

#[allow(clippy::unreadable_literal)] // hex colours read best as written
pub mod color {
    /// The content area behind the selected VM.
    pub const BG: u32 = 0x15171a;
    /// The sidebar and title bar.
    pub const SURFACE: u32 = 0x1b1e22;
    /// Buttons and menus, one step above their background.
    pub const ELEVATED: u32 = 0x22262b;
    pub const HOVER: u32 = 0x2a2f35;
    pub const BORDER: u32 = 0x2a2e34;
    /// Outlines of controls, which need more contrast than dividers.
    pub const BORDER_STRONG: u32 = 0x393e46;

    pub const TEXT: u32 = 0xe6e8eb;
    pub const TEXT_SECONDARY: u32 = 0xa8afb9;
    pub const TEXT_MUTED: u32 = 0x737b86;

    pub const ACCENT: u32 = 0x4f8bf0;
    pub const ACCENT_HOVER: u32 = 0x649af3;
    pub const ON_ACCENT: u32 = 0xffffff;

    pub const GREEN: u32 = 0x45c08a;
    pub const AMBER: u32 = 0xe6a83e;
    pub const RED: u32 = 0xec6b6b;
    pub const GREY: u32 = 0x7d8591;

    /// Timeline categories that carry no good/bad meaning.
    pub const BLUE: u32 = 0x6a9cff;
    pub const TEAL: u32 = 0x3cc2b4;
    pub const PURPLE: u32 = 0xb48cf8;
    pub const ORANGE: u32 = 0xee8d4f;
    pub const PINK: u32 = 0xe47bb0;
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

    pub const S: Pixels = px(4.);
    pub const M: Pixels = px(6.);
    pub const L: Pixels = px(8.);
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
pub const CONTROL_HEIGHT: Pixels = px(30.);

/// What a status means to the user, whatever it is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Good,
    Warn,
    Bad,
    Neutral,
}

impl Tone {
    pub fn color(self) -> u32 {
        match self {
            Self::Good => color::GREEN,
            Self::Warn => color::AMBER,
            Self::Bad => color::RED,
            Self::Neutral => color::GREY,
        }
    }
}

pub fn alpha(c: u32, a: u8) -> Rgba {
    rgba((c << 8) | u32::from(a))
}

/// `c` at low opacity: the background of a pill or badge in that colour.
pub fn tint(c: u32) -> Rgba {
    alpha(c, 0x26)
}
