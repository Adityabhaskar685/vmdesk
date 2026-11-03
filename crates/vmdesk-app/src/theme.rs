//! The design tokens every view draws with: colours, spacing, radii and text sizes.

use gpui::{Pixels, Rgba, px, rgba};

pub use palette::{Source, Theme, generation, palette, select_saved, themes};

pub mod palette;

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

/// The UI font, built in (see `fonts.rs`).
pub const SANS: &str = "Inter";
/// For commands, paths, addresses and the terminal's fallback, built in.
pub const MONO: &str = "JetBrains Mono";
/// Height of buttons and inputs.
pub const CONTROL_HEIGHT: Pixels = px(32.);

pub use vmdesk_db::Tone;

pub fn tone_color(tone: Tone) -> u32 {
    match tone {
        Tone::Good => palette().green,
        Tone::Warn => palette().amber,
        Tone::Bad => palette().red,
        Tone::Neutral => palette().grey,
    }
}

pub fn alpha(c: u32, a: u8) -> Rgba {
    rgba((c << 8) | u32::from(a))
}

/// `c` at low opacity: the background of a pill or badge in that colour.
pub fn tint(c: u32) -> Rgba {
    alpha(c, 0x26)
}
