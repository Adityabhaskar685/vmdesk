//! Colour themes for the window and its terminals, and the saved choice.
//!
//! Themes come from two places: the built-in ones, and Ghostty's theme files wherever Ghostty
//! keeps them (`$XDG_DATA_DIRS/ghostty/themes`, `~/.local/share/ghostty/themes`,
//! `~/.config/ghostty/themes`). A Ghostty theme only names terminal colours, so the window's
//! colours are derived from them. Views read the active theme with [`palette`] on every
//! render, so switching takes effect on the next frame.

#![allow(clippy::unreadable_literal)] // hex colours read best as written

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, RwLock};

/// Every colour a view draws with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// The content area behind the selected VM.
    pub bg: u32,
    /// The sidebar and title bar.
    pub surface: u32,
    /// Cards, buttons and menus, one step above their background.
    pub elevated: u32,
    pub hover: u32,
    pub border: u32,
    /// Outlines of controls, which need more contrast than dividers.
    pub border_strong: u32,
    pub text: u32,
    pub text_secondary: u32,
    pub text_muted: u32,
    pub accent: u32,
    pub accent_hover: u32,
    /// Text on the accent colour.
    pub on_accent: u32,
    pub green: u32,
    pub amber: u32,
    pub red: u32,
    pub grey: u32,
    /// Timeline categories that carry no good/bad meaning.
    pub blue: u32,
    pub teal: u32,
    pub purple: u32,
    pub orange: u32,
    pub pink: u32,
    pub term: TermColors,
}

/// A terminal's default colours. Programs can still set their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermColors {
    pub foreground: u32,
    pub background: u32,
    pub cursor: u32,
    pub selection: u32,
    /// The text colour inside a selection, when the theme sets one.
    pub selection_fg: Option<u32>,
    /// The 16 ANSI colours: 8 normal, then 8 bright.
    pub ansi: [u32; 16],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    BuiltIn,
    /// A Ghostty theme file.
    File(PathBuf),
}

#[derive(Clone, Debug)]
pub struct Theme {
    pub name: String,
    pub source: Source,
    pub palette: Palette,
}

impl Theme {
    /// Whether the theme has a light background.
    #[must_use]
    pub fn is_light(&self) -> bool {
        luminance(self.palette.term.background) >= 0.5
    }
}

const MOCHA_ANSI: [u32; 16] = [
    0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de, 0x585b70,
    0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xa6adc8,
];

const LATTE_ANSI: [u32; 16] = [
    0x5c5f77, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0xacb0be, 0x6c6f85,
    0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0xbcc0cc,
];

/// The default: Catppuccin Mocha, tuned by hand for the app's surfaces.
pub const DARK: Palette = Palette {
    bg: 0x11111b,
    surface: 0x181825,
    elevated: 0x1e1e2e,
    hover: 0x313244,
    border: 0x2a2a3c,
    border_strong: 0x45475a,
    text: 0xcdd6f4,
    text_secondary: 0xa6adc8,
    text_muted: 0x6c7086,
    accent: 0x89b4fa,
    accent_hover: 0xb4befe,
    on_accent: 0x11111b,
    green: 0xa6e3a1,
    amber: 0xf9e2af,
    red: 0xf38ba8,
    grey: 0x7f849c,
    blue: 0x89b4fa,
    teal: 0x94e2d5,
    purple: 0xcba6f7,
    orange: 0xfab387,
    pink: 0xf5c2e7,
    term: TermColors {
        foreground: 0xcdd6f4,
        background: 0x11111b,
        cursor: 0xf5e0dc,
        selection: 0x45475a,
        selection_fg: None,
        ansi: MOCHA_ANSI,
    },
};

/// Catppuccin Latte, tuned the same way.
pub const LIGHT: Palette = Palette {
    bg: 0xe6e9ef,
    surface: 0xdce0e8,
    elevated: 0xeff1f5,
    hover: 0xccd0da,
    border: 0xd3d7e0,
    border_strong: 0xbcc0cc,
    text: 0x4c4f69,
    text_secondary: 0x5c5f77,
    text_muted: 0x8c8fa1,
    accent: 0x1e66f5,
    accent_hover: 0x7287fd,
    on_accent: 0xeff1f5,
    green: 0x40a02b,
    amber: 0xdf8e1d,
    red: 0xd20f39,
    grey: 0x8c8fa1,
    blue: 0x1e66f5,
    teal: 0x179299,
    purple: 0x8839ef,
    orange: 0xfe640b,
    pink: 0xea76cb,
    term: TermColors {
        foreground: 0x4c4f69,
        background: 0xeff1f5,
        cursor: 0xdc8a78,
        selection: 0xccd0da,
        selection_fg: None,
        ansi: LATTE_ANSI,
    },
};

pub const DEFAULT: &str = "VMDesk Dark";

fn built_in() -> Vec<Theme> {
    vec![
        Theme {
            name: DEFAULT.to_owned(),
            source: Source::BuiltIn,
            palette: DARK,
        },
        Theme {
            name: "VMDesk Light".to_owned(),
            source: Source::BuiltIn,
            palette: LIGHT,
        },
    ]
}

struct Registry {
    themes: Vec<Arc<Theme>>,
    active: Arc<Theme>,
}

static REGISTRY: LazyLock<RwLock<Registry>> = LazyLock::new(|| {
    let themes: Vec<Arc<Theme>> = built_in().into_iter().map(Arc::new).collect();
    RwLock::new(Registry {
        active: Arc::clone(&themes[0]),
        themes,
    })
});

/// Bumped on every switch, so views that cache colours (terminals) know to refresh.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Whether every Ghostty theme has been read. Until the picker asks for them, only the
/// saved theme's file is.
static ALL_LOADED: AtomicBool = AtomicBool::new(false);

/// The active theme's colours.
#[must_use]
pub fn palette() -> Palette {
    REGISTRY.read().map_or(DARK, |r| r.active.palette)
}

/// The active theme's name.
#[must_use]
pub fn active_name() -> String {
    REGISTRY
        .read()
        .map_or_else(|_| DEFAULT.to_owned(), |r| r.active.name.clone())
}

#[must_use]
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// Every theme, built-in ones first, then the rest by name.
#[must_use]
pub fn themes() -> Vec<Arc<Theme>> {
    REGISTRY
        .read()
        .map_or_else(|_| Vec::new(), |r| r.themes.clone())
}

/// Makes the theme called `name` (any case) the active one, reading just its Ghostty file
/// when the full list has not been loaded. `false` if there is none.
pub fn select(name: &str) -> bool {
    let known = REGISTRY
        .read()
        .is_ok_and(|r| r.themes.iter().any(|t| t.name.eq_ignore_ascii_case(name)));
    if !known && !ALL_LOADED.load(Ordering::Acquire) {
        let Some(theme) = find_ghostty_theme(&ghostty_theme_dirs(), name) else {
            return false;
        };
        if let Ok(mut registry) = REGISTRY.write() {
            registry.themes.push(Arc::new(theme));
        }
    }
    let Ok(mut registry) = REGISTRY.write() else {
        return false;
    };
    let Some(theme) = registry
        .themes
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(name))
        .cloned()
    else {
        return false;
    };
    registry.active = theme;
    GENERATION.fetch_add(1, Ordering::AcqRel);
    true
}

/// The Ghostty theme called `name`: its file is named after it, so only that file is read.
fn find_ghostty_theme(dirs: &[PathBuf], name: &str) -> Option<Theme> {
    dirs.iter().find_map(|dir| {
        let entries = std::fs::read_dir(dir).ok()?;
        entries
            .flatten()
            .find(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|f| f.trim_end_matches(".conf").eq_ignore_ascii_case(name))
            })
            .and_then(|e| parse_ghostty(&e.path()))
    })
}

/// Reads every Ghostty theme, once: the first call does the work, later ones return at once.
pub fn ensure_all_loaded() {
    if !ALL_LOADED.swap(true, Ordering::AcqRel) {
        load_ghostty_themes();
    }
}

/// Adds Ghostty's theme files to the built-in themes. A file whose name repeats one already
/// found is skipped, so a built-in theme keeps its name.
fn load_ghostty_themes() {
    let mut found: Vec<Theme> = Vec::new();
    for dir in ghostty_theme_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Some(theme) = parse_ghostty(&entry.path())
                && !found
                    .iter()
                    .any(|t| t.name.eq_ignore_ascii_case(&theme.name))
            {
                found.push(theme);
            }
        }
    }
    found.sort_by_key(|t| t.name.to_lowercase());
    if let Ok(mut registry) = REGISTRY.write() {
        let mut themes: Vec<Arc<Theme>> = built_in().into_iter().map(Arc::new).collect();
        for theme in found {
            if !themes
                .iter()
                .any(|t| t.name.eq_ignore_ascii_case(&theme.name))
            {
                themes.push(Arc::new(theme));
            }
        }
        registry.themes = themes;
    }
}

fn ghostty_theme_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(config) = dirs::config_dir() {
        dirs.push(config.join("ghostty/themes"));
    }
    if let Some(data) = dirs::data_dir() {
        dirs.push(data.join("ghostty/themes"));
    }
    let system =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    dirs.extend(
        system
            .split(':')
            .filter(|d| !d.is_empty())
            .map(|d| Path::new(d).join("ghostty/themes")),
    );
    dirs.push("/snap/ghostty/current/share/ghostty/themes".into());
    dirs
}

const SETTINGS: &str = "settings.json";

/// Applies the theme named by `VMDESK_THEME`, else the one saved in `data_dir`. An unknown
/// name leaves the default.
pub fn select_saved(data_dir: &Path) {
    let wanted = std::env::var("VMDESK_THEME").ok().or_else(|| {
        let text = std::fs::read_to_string(data_dir.join(SETTINGS)).ok()?;
        let settings: serde_json::Value = serde_json::from_str(&text).ok()?;
        settings["theme"].as_str().map(str::to_owned)
    });
    if let Some(name) = wanted
        && !select(&name)
    {
        tracing::warn!(theme = %name, "no such theme; using the default");
    }
}

/// Saves `name` as the theme to start with, keeping any other settings in the file.
pub fn remember(data_dir: &Path, name: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join(SETTINGS);
    let mut settings: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    settings["theme"] = serde_json::Value::String(name.to_owned());
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&settings)?)?;
    std::fs::rename(tmp, path)
}

fn parse_ghostty(path: &Path) -> Option<Theme> {
    let text = std::fs::read_to_string(path).ok()?;
    let name = path
        .file_name()?
        .to_str()?
        .trim_end_matches(".conf")
        .to_owned();
    let (mut background, mut foreground, mut cursor, mut selection, mut selection_fg) =
        (None, None, None, None, None);
    let mut ansi: [Option<u32>; 16] = [None; 16];
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "background" => background = hex(value),
            "foreground" => foreground = hex(value),
            "cursor-color" => cursor = hex(value),
            "selection-background" => selection = hex(value),
            "selection-foreground" => selection_fg = hex(value),
            "palette" => {
                if let Some((index, color)) = value.split_once('=')
                    && let (Ok(index), Some(color)) = (index.trim().parse::<usize>(), hex(color))
                    && index < 16
                {
                    ansi[index] = Some(color);
                }
            }
            _ => {}
        }
    }
    let (background, foreground) = (background?, foreground?);
    let ansi: [u32; 16] = std::array::from_fn(|i| ansi[i].unwrap_or(MOCHA_ANSI[i]));
    let term = TermColors {
        foreground,
        background,
        cursor: cursor.unwrap_or(foreground),
        selection: selection.unwrap_or_else(|| mix(background, foreground, 0.25)),
        selection_fg,
        ansi,
    };
    Some(Theme {
        name,
        source: Source::File(path.to_path_buf()),
        palette: derive(term),
    })
}

/// Window colours that sit well around a terminal with these colours: surfaces step from
/// the terminal's background towards black (dark themes) or stay close to it (light ones),
/// text fades towards the background, and the status and category colours are the
/// terminal's own ANSI colours.
fn derive(term: TermColors) -> Palette {
    let (bg, fg) = (term.background, term.foreground);
    let dark = luminance(bg) < 0.5;
    let shade = |t: f32| mix(bg, 0x000000, t);
    let accent = term.ansi[4];
    Palette {
        bg: if dark { shade(0.28) } else { shade(0.05) },
        surface: if dark { shade(0.18) } else { shade(0.09) },
        elevated: bg,
        hover: mix(bg, fg, 0.10),
        border: mix(bg, fg, 0.10),
        border_strong: mix(bg, fg, 0.22),
        text: fg,
        text_secondary: mix(fg, bg, 0.28),
        text_muted: mix(fg, bg, 0.52),
        accent,
        accent_hover: term.ansi[12],
        on_accent: if luminance(accent) > 0.5 {
            shade(0.4)
        } else {
            0xffffff
        },
        green: term.ansi[2],
        amber: term.ansi[3],
        red: term.ansi[1],
        grey: mix(fg, bg, 0.45),
        blue: accent,
        teal: term.ansi[6],
        purple: term.ansi[5],
        orange: mix(term.ansi[1], term.ansi[3], 0.5),
        pink: term.ansi[13],
        term: TermColors {
            background: if dark { shade(0.28) } else { bg },
            ..term
        },
    }
}

fn hex(value: &str) -> Option<u32> {
    let value = value.trim().trim_start_matches('#');
    (value.len() == 6)
        .then(|| u32::from_str_radix(value, 16).ok())
        .flatten()
}

/// `from` moved `t` of the way towards `to`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // channels stay 0..=255
fn mix(from: u32, to: u32, t: f32) -> u32 {
    let channel = |shift: u32| {
        let start = f32::from(((from >> shift) & 0xff) as u8);
        let end = f32::from(((to >> shift) & 0xff) as u8);
        ((start + (end - start) * t).round() as u32) << shift
    };
    channel(16) | channel(8) | channel(0)
}

/// Relative luminance, 0 (black) to 1 (white).
fn luminance(c: u32) -> f32 {
    let channel = |shift: u32| f32::from(((c >> shift) & 0xff) as u8) / 255.;
    0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_theme(dir: &Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("vmdesk-theme-{tag}-{}", std::process::id()))
    }

    #[test]
    fn a_ghostty_theme_gives_terminal_and_window_colours() {
        let dir = scratch("parse");
        let path = write_theme(
            &dir,
            "Night Owl",
            "# comment\nbackground = #011627\nforeground = d6deeb\npalette = 1=#ef5350\n\
             palette = 2=#22da6e\npalette = 4=#82aaff\ncursor-color = #80a4c2\n",
        );
        let theme = parse_ghostty(&path).expect("a theme");
        assert_eq!(theme.name, "Night Owl");
        let p = theme.palette;
        assert_eq!(
            (p.text, p.red, p.green, p.accent),
            (0xd6deeb, 0xef5350, 0x22da6e, 0x82aaff)
        );
        assert_eq!(p.term.cursor, 0x80a4c2);
        assert_eq!(p.term.ansi[3], MOCHA_ANSI[3], "an unset colour falls back");
        assert!(
            luminance(p.bg) < luminance(p.elevated),
            "dark themes put cards above the background"
        );
        assert!(!theme.is_light());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_file_without_both_base_colours_is_not_a_theme() {
        let dir = scratch("partial");
        let path = write_theme(&dir, "Half", "background = #000000\n");
        assert!(parse_ghostty(&path).is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn light_themes_keep_readable_text() {
        let term = TermColors {
            foreground: 0x383a42,
            background: 0xfafafa,
            cursor: 0x526eff,
            selection: 0xe5e5e6,
            selection_fg: None,
            ansi: LATTE_ANSI,
        };
        let p = derive(term);
        assert!(luminance(p.elevated) - luminance(p.text_muted) > 0.25);
        assert_eq!(p.term.background, 0xfafafa);
    }

    #[test]
    fn the_saved_choice_round_trips_and_keeps_other_settings() {
        let dir = scratch("settings");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SETTINGS), r#"{"other": 1}"#).unwrap();
        remember(&dir, "VMDesk Light").unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(SETTINGS)).unwrap()).unwrap();
        assert_eq!(saved["theme"], "VMDesk Light");
        assert_eq!(saved["other"], 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn one_theme_is_found_by_its_file_name_alone() {
        let dir = scratch("find");
        write_theme(
            &dir,
            "Night Owl",
            "background = #011627\nforeground = #d6deeb\n",
        );
        write_theme(&dir, "Broken", "not a theme\n");
        let dirs = [scratch("missing"), dir.clone()];
        let found = find_ghostty_theme(&dirs, "night owl").expect("found, ignoring case");
        assert_eq!(found.name, "Night Owl");
        assert!(find_ghostty_theme(&dirs, "Broken").is_none());
        assert!(find_ghostty_theme(&dirs, "Nope").is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn mix_and_luminance() {
        assert_eq!(mix(0x000000, 0xffffff, 0.5), 0x808080);
        assert_eq!(mix(0x102030, 0x102030, 0.7), 0x102030);
        assert!(luminance(0xffffff) > 0.99 && luminance(0x000000) < 0.01);
    }
}
