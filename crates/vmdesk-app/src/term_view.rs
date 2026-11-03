//! The embedded terminal: libghostty-vt's grid painted with GPUI, and the SSH pty behind
//! it.
//!
//! The design follows Rex's GPUI terminal pane. The differences are where the bytes come
//! from: Rex mirrors a server that owns the terminal and sends key presses as JSON, while
//! here the local terminal is the real one, fed by an SSH pty, and keys, mouse and paste
//! are encoded locally by libghostty-vt.

// Pixel and cell maths runs on window-sized values, and the palette is clearer as literals.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::map_unwrap_or,
    clippy::needless_pass_by_value,
    clippy::similar_names,
    clippy::struct_excessive_bools,
    clippy::too_many_lines,
    clippy::trivially_copy_pass_by_ref,
    clippy::unreadable_literal,
    clippy::unused_self,
    clippy::used_underscore_binding,
    clippy::wildcard_enum_match_arm
)]

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Mutex;

use gpui::{
    App, Bounds, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, Font, FontFallbacks,
    FontStyle, FontWeight, Hsla, KeyDownEvent, KeyUpEvent, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollDelta, ScrollWheelEvent, SharedString, Task,
    TextAlign, TextRun, Window, canvas, div, fill, font, outline, point, prelude::*, px, rgb, size,
};
use vmdesk_core::VmId;
use vmdesk_terminal::{
    CursorShape, Frame as TermFrame, Key, KeyAction, KeyEncoder, KeyEvent, Mods, MouseAction,
    MouseButton as TermMouseButton, MouseEncoder, MouseEvent as TermMouseEvent, RenderState, Rgb,
    Scroll, Terminal, ViewportPoint, Width, encode_focus, encode_paste,
};

use crate::backend::Backend;
use crate::term_keys;
use crate::term_session::{Ended, SharedTerm, TermEvent, TerminalSession};
use crate::theme::{self, MONO, palette};

/// Width kept free at the right of the grid for the scrollbar.
const SCROLLBAR_WIDTH: f32 = 10.;
/// The DEC mode a program turns on to get focus gained/lost reports.
const FOCUS_REPORTING: u16 = 1004;
const BRACKETED_PASTE: u16 = 2004;
const ANY_MOTION_MOUSE: u16 = 1003;

/// Monospace families tried, in order, when `VMDESK_TERM_FONT` is not set. Nerd Fonts
/// carry the private-use glyphs shells put in prompts (Powerlevel10k, Starship, …).
const FONT_PREFERENCE: &[&str] = &[
    "Hack Nerd Font Mono",
    "JetBrainsMono Nerd Font Mono",
    "FiraCode Nerd Font Mono",
    "CaskaydiaCove Nerd Font Mono",
    "SauceCodePro Nerd Font Mono",
    "MesloLGS NF",
];

/// Families a missing glyph may come from. All fixed-width where they exist, so a
/// fallback glyph still advances one cell.
const FONT_FALLBACKS: &[&str] = &[
    "Hack Nerd Font Mono",
    "JetBrainsMono Nerd Font Mono",
    "FiraCode Nerd Font Mono",
    "CaskaydiaCove Nerd Font Mono",
    "SauceCodePro Nerd Font Mono",
    "MesloLGS NF",
    "Symbols Nerd Font Mono",
    "Ubuntu Mono",
    "Noto Sans Mono",
    "DejaVu Sans Mono",
    "Liberation Mono",
];

pub struct TerminalView {
    backend: Backend,
    vm: VmId,
    font: Font,
    session: TerminalSession,
    term: SharedTerm,
    status: Option<String>,
    /// Why the shell ended, shown under the last output.
    closed: Option<String>,
    render_state: RenderState,
    keys: KeyEncoder,
    mouse: MouseEncoder,
    cell: Option<CellMetrics>,
    /// Where the grid was last painted, to turn mouse positions into cells.
    origin: Option<Point<Pixels>>,
    /// The grid's size in pixels, for the mouse encoder.
    grid_size: Option<Point<Pixels>>,
    /// The grid's height when last painted, which is also the scrollbar's track.
    track_height: Option<Pixels>,
    /// The size we last asked the pty for.
    requested: Option<(u16, u16)>,
    /// Scroll-wheel movement too small to make a whole line yet.
    scroll_remainder: f32,
    /// Where a left-button drag that selects text started, while it lasts.
    select_anchor: Option<ViewportPoint>,
    /// The scrollbar thumb is being dragged, held this far below its top.
    scroll_drag: Option<Pixels>,
    /// Whether a left, middle or right button is held, for mouse motion encoding.
    mouse_held: bool,
    /// A multi-line paste waiting for the user to confirm it.
    pending_paste: Option<String>,
    /// What focus reporting was last told; `None` before the first frame.
    reported_focus: Option<bool>,
    theme_cache: RefCell<Option<(u64, Rc<PanelTheme>)>>,
    focus_handle: FocusHandle,
    _events: Task<()>,
}

#[derive(Clone, Copy)]
struct CellMetrics {
    width: Pixels,
    height: Pixels,
}

impl TerminalView {
    pub fn new(backend: Backend, vm: VmId, cx: &mut Context<Self>) -> Self {
        let (term, session) = start_session(&backend, vm);
        let events = watch(&session, cx);
        Self {
            backend,
            vm,
            font: terminal_font(cx),
            session,
            term,
            status: Some("Connecting…".into()),
            closed: None,
            render_state: RenderState::new().expect("creating a render state"),
            keys: KeyEncoder::new().expect("creating a key encoder"),
            mouse: MouseEncoder::new().expect("creating a mouse encoder"),
            cell: None,
            origin: None,
            grid_size: None,
            track_height: None,
            requested: None,
            scroll_remainder: 0.,
            select_anchor: None,
            scroll_drag: None,
            mouse_held: false,
            pending_paste: None,
            reported_focus: None,
            theme_cache: RefCell::new(None),
            focus_handle: cx.focus_handle(),
            _events: events,
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// The connection dropped: open a fresh pty and terminal, keeping the tab in place.
    fn reconnect(&mut self, cx: &mut Context<Self>) {
        let (term, session) = start_session(&self.backend, self.vm);
        self.term = term;
        self.session = session;
        self._events = watch(&self.session, cx);
        self.status = Some("Connecting…".into());
        self.closed = None;
        self.render_state.invalidate();
        self.reported_focus = None;
        self.requested = None;
        cx.notify();
    }

    fn term(&self) -> std::sync::MutexGuard<'_, Terminal> {
        self.term.lock().unwrap()
    }

    /// The active theme's terminal colours. After a theme switch they are rebuilt and given
    /// to the terminal, so text without colours of its own takes the new ones.
    fn theme(&self) -> Rc<PanelTheme> {
        let generation = theme::generation();
        let mut cache = self.theme_cache.borrow_mut();
        if let Some((seen, theme)) = cache.as_ref()
            && *seen == generation
        {
            return theme.clone();
        }
        let theme = Rc::new(PanelTheme::app());
        theme.apply(&mut self.term());
        *cache = Some((generation, theme.clone()));
        theme
    }

    /// Paste `text`. A program that turned on bracketed paste gets it as one block. Without
    /// it, a shell runs each line as it arrives, so a multi-line paste waits for a yes.
    fn paste(&mut self, text: String, cx: &mut Context<Self>) {
        {
            let mut shared = self.term();
            shared.clear_selection();
        }
        self.select_anchor = None;
        let lines = text.trim_end_matches(['\n', '\r']).lines().count();
        let bracketed = self.term().dec_mode(BRACKETED_PASTE);
        if lines > 1 && !bracketed {
            self.pending_paste = Some(text);
            cx.notify();
            return;
        }
        self.session.send(encode_paste(&text, bracketed));
    }

    fn confirm_paste(&mut self, as_one_line: bool, cx: &mut Context<Self>) {
        if let Some(text) = self.pending_paste.take() {
            let text = if as_one_line {
                text.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                text
            };
            self.session.send(encode_paste(&text, false));
        }
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let m = &ks.modifiers;
        if self.pending_paste.is_some() {
            match ks.key.as_str() {
                "enter" => self.confirm_paste(false, cx),
                "escape" => {
                    self.pending_paste = None;
                    cx.notify();
                }
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        // Clipboard keys stay with the client, as in most Linux terminals.
        if m.control && m.shift && !m.alt && ks.key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                self.paste(text, cx);
            }
            cx.stop_propagation();
            return;
        }
        if m.control && m.shift && !m.alt && ks.key == "c" {
            let text = self.term().selection_text().unwrap_or_default();
            if !text.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            cx.stop_propagation();
            return;
        }
        // Terminal-tab management belongs to the app, so these keys bubble up.
        if m.control && m.shift && !m.alt && term_keys::APP_SHORTCUTS.contains(&ks.key.as_str()) {
            return;
        }
        if m.shift && !m.control && matches!(ks.key.as_str(), "pageup" | "pagedown") {
            let rows = self.term().rows() as isize;
            let delta = if ks.key == "pageup" { -rows } else { rows };
            self.term().scroll(Scroll::Delta(delta));
            cx.notify();
            cx.stop_propagation();
            return;
        }
        let action = if event.is_held {
            KeyAction::Repeat
        } else {
            KeyAction::Press
        };
        let Some(key_event) = term_keys::key_event(ks, action) else {
            return;
        };
        cx.stop_propagation();
        let bytes = {
            let mut shared = self.term.lock().unwrap();
            // Typing clears a selection and returns the view to the bottom, as in Ghostty.
            if shared.has_selection() || !shared.at_bottom() {
                shared.clear_selection();
                shared.scroll(Scroll::Bottom);
                cx.notify();
            }
            self.keys.sync(&shared);
            self.keys.encode(&key_event)
        };
        self.session.send(bytes);
    }

    fn on_key_up(&mut self, event: &KeyUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(key_event) = term_keys::key_event(&event.keystroke, KeyAction::Release) else {
            return;
        };
        cx.stop_propagation();
        let bytes = {
            let shared = self.term.lock().unwrap();
            self.keys.sync(&shared);
            self.keys.encode(&key_event)
        };
        self.session.send(bytes);
    }

    fn on_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cell) = self.cell else { return };
        let (mouse, alternate) = {
            let shared = self.term();
            (shared.mouse_tracking(), shared.alternate_screen())
        };
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y * scroll_multiplier(),
            ScrollDelta::Pixels(delta) => delta.y / cell.height,
        };
        let lines = lines + self.scroll_remainder;
        let whole = lines.trunc();
        self.scroll_remainder = lines - whole;
        if whole == 0. {
            return;
        }
        let steps = whole.abs() as usize;
        // Full-screen programs and programs that read the mouse get real wheel events. A
        // program that only took the alternate screen gets the arrow keys a terminal
        // would synthesize.
        if (mouse || alternate) && !event.modifiers.shift {
            for _ in 0..steps {
                if mouse {
                    let button = if whole > 0. {
                        TermMouseButton::Four
                    } else {
                        TermMouseButton::Five
                    };
                    self.send_mouse(
                        MouseAction::Press,
                        Some(button),
                        event.position,
                        &event.modifiers,
                    );
                } else {
                    let key = if whole > 0. { "arrow_up" } else { "arrow_down" };
                    let key_event = KeyEvent {
                        key: Key::from_code(key),
                        action: KeyAction::Press,
                        mods: Mods::default(),
                        consumed_mods: Mods::default(),
                        utf8: None,
                        unshifted_codepoint: None,
                        composing: false,
                    };
                    let bytes = {
                        let shared = self.term.lock().unwrap();
                        self.keys.sync(&shared);
                        self.keys.encode(&key_event)
                    };
                    self.session.send(bytes);
                }
            }
            return;
        }
        // Wheel up is positive here; scrolling back is negative there.
        self.term().scroll(Scroll::Delta(-whole as isize));
        cx.notify();
    }

    /// The cell under a window position, as `(column, row)` on screen, clamped to the grid.
    fn cell_at(&self, position: Point<Pixels>) -> Option<(usize, usize)> {
        let (origin, cell) = (self.origin?, self.cell?);
        let column = ((position.x - origin.x) / cell.width).floor().max(0.0) as usize;
        let row = ((position.y - origin.y) / cell.height).floor().max(0.0) as usize;
        let shared = self.term();
        let (cols, rows) = (shared.cols() as usize, shared.rows() as usize);
        Some((
            column.min(cols.saturating_sub(1)),
            row.min(rows.saturating_sub(1)),
        ))
    }

    fn viewport_point(&self, position: Point<Pixels>) -> Option<ViewportPoint> {
        let (x, y) = self.cell_at(position)?;
        Some(ViewportPoint {
            x: x as u16,
            y: y as u16,
        })
    }

    /// Whether mouse events belong to the program. Shift keeps them local, for selecting.
    fn program_wants_mouse(&self, modifiers: &Modifiers) -> bool {
        !modifiers.shift && self.term().mouse_tracking()
    }

    fn send_mouse(
        &mut self,
        action: MouseAction,
        button: Option<TermMouseButton>,
        position: Point<Pixels>,
        modifiers: &Modifiers,
    ) {
        let (Some(origin), Some(cell), Some(grid)) = (self.origin, self.cell, self.grid_size)
        else {
            return;
        };
        let event = TermMouseEvent {
            action,
            button,
            mods: term_mods(modifiers),
            x: f32::from(position.x - origin.x),
            y: f32::from(position.y - origin.y),
        };
        let bytes = {
            let shared = self.term.lock().unwrap();
            self.mouse.set_size(
                f32::from(grid.x).max(1.) as u32,
                f32::from(grid.y).max(1.) as u32,
                f32::from(cell.width).max(1.) as u32,
                f32::from(cell.height).max(1.) as u32,
            );
            self.mouse.set_any_button_pressed(self.mouse_held);
            self.mouse.sync(&shared);
            self.mouse.encode(&event)
        };
        self.session.send(bytes);
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        if self.program_wants_mouse(&event.modifiers) {
            self.mouse_held = matches!(
                event.button,
                MouseButton::Left | MouseButton::Right | MouseButton::Middle
            );
            self.send_mouse(
                MouseAction::Press,
                Some(term_button(event.button)),
                event.position,
                &event.modifiers,
            );
            return;
        }
        if event.button == MouseButton::Middle {
            if let Some(text) = cx.read_from_primary().and_then(|item| item.text()) {
                self.paste(text, cx);
            }
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        let Some(at) = self.viewport_point(event.position) else {
            return;
        };
        match event.click_count {
            1 => {
                // A plain click clears the selection; dragging from here selects.
                self.term().clear_selection();
                self.select_anchor = Some(at);
            }
            2 => {
                self.term().select_word(at);
                self.finish_selection(cx);
            }
            _ => {
                self.term().select_line(at);
                self.finish_selection(cx);
            }
        }
        cx.notify();
    }

    /// Scroll so the thumb's top follows `y`, measured from the top of the track.
    fn scroll_to_thumb(&mut self, top: f32, cx: &mut Context<Self>) {
        let Some(track) = self.track_height else {
            return;
        };
        let mut shared = self.term();
        let bar = shared.scrollbar();
        // The thumb's top marks the first visible row.
        let max = bar.total.saturating_sub(bar.len) as f32;
        let row = (top / f32::from(track) * bar.total as f32)
            .clamp(0.0, max)
            .round() as u64;
        if row != bar.offset {
            shared.scroll(Scroll::Row(row as usize));
            cx.notify();
        }
    }

    fn on_scrollbar_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let (Some(origin), Some(track)) = (self.origin, self.track_height) else {
            return;
        };
        let (thumb_top, thumb_height) = self.thumb(track);
        let y = f32::from(event.position.y - origin.y);
        // Grabbing the thumb keeps its offset under the pointer; clicking the track centers it.
        let grab = if (thumb_top..thumb_top + thumb_height).contains(&y) {
            y - thumb_top
        } else {
            thumb_height / 2.
        };
        self.scroll_drag = Some(px(grab));
        self.scroll_to_thumb(y - grab, cx);
    }

    /// The scrollbar thumb's top and height on a track this tall.
    fn thumb(&self, track: Pixels) -> (f32, f32) {
        let bar = self.term().scrollbar();
        let total = bar.total.max(1) as f32;
        let track = f32::from(track);
        (
            track * bar.offset as f32 / total,
            track * bar.len as f32 / total,
        )
    }

    /// End a drag-selection, keeping what it selected as the primary selection.
    fn finish_selection(&mut self, cx: &mut Context<Self>) {
        self.select_anchor = None;
        // Like other X11 and Wayland terminals, a selection becomes the primary selection.
        let text = self.term().selection_text().filter(|t| !t.is_empty());
        if let Some(text) = text {
            cx.write_to_primary(ClipboardItem::new_string(text));
        }
        cx.notify();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let (Some(grab), Some(origin)) = (self.scroll_drag, self.origin) {
            if event.pressed_button == Some(MouseButton::Left) {
                self.scroll_to_thumb(f32::from(event.position.y - origin.y - grab), cx);
            } else {
                self.scroll_drag = None;
            }
            return;
        }
        if let Some(anchor) = self.select_anchor {
            // The button came up somewhere this pane didn't hear about.
            if event.pressed_button != Some(MouseButton::Left) {
                self.finish_selection(cx);
                return;
            }
            if let Some(at) = self.viewport_point(event.position)
                && (at != anchor || self.term().has_selection())
            {
                self.term().select(anchor, at);
                cx.notify();
            }
            return;
        }
        // Report drags; plain motion only matters to programs in any-motion mode.
        let wants_motion = event.pressed_button.is_some() || self.term().dec_mode(ANY_MOTION_MOUSE);
        if wants_motion && self.program_wants_mouse(&event.modifiers) {
            self.send_mouse(MouseAction::Motion, None, event.position, &event.modifiers);
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.scroll_drag.take().is_some() {
            return;
        }
        if self.select_anchor.is_some() {
            self.finish_selection(cx);
            return;
        }
        if self.program_wants_mouse(&event.modifiers) {
            self.mouse_held = false;
            self.send_mouse(
                MouseAction::Release,
                Some(term_button(event.button)),
                event.position,
                &event.modifiers,
            );
        }
    }

    /// Ask for the size that fills `bounds` when it differs from the last request.
    fn request_size(&mut self, bounds: Bounds<Pixels>, cell: CellMetrics) {
        self.origin = Some(bounds.origin);
        self.track_height = Some(bounds.size.height);
        self.grid_size = Some(point(bounds.size.width, bounds.size.height));
        let width = bounds.size.width - px(SCROLLBAR_WIDTH);
        let cols = ((width / cell.width).floor() as u16).max(2);
        let rows = ((bounds.size.height / cell.height).floor() as u16).max(1);
        if self.requested == Some((cols, rows)) {
            return;
        }
        self.requested = Some((cols, rows));
        let cell_width = f32::from(cell.width).round().max(1.0) as u16;
        let cell_height = f32::from(cell.height).round().max(1.0) as u16;
        self.term()
            .resize(cols, rows, u32::from(cell_width), u32::from(cell_height));
        // Cached rows describe the old grid.
        self.render_state.invalidate();
        self.session.resize(cols, rows, cell_width, cell_height);
    }
}

/// Builds a terminal with the app's colours and starts connecting it.
fn start_session(backend: &Backend, vm: VmId) -> (SharedTerm, TerminalSession) {
    let mut terminal = Terminal::new(80, 24).expect("creating a terminal");
    PanelTheme::app().apply(&mut terminal);
    let term: SharedTerm = std::sync::Arc::new(Mutex::new(terminal));
    let session = TerminalSession::start(backend, vm, &term);
    (term, session)
}

/// Follows one session's events until the view goes away.
fn watch(session: &TerminalSession, cx: &mut Context<TerminalView>) -> Task<()> {
    let (events, dirty) = (session.events.clone(), session.dirty.clone());
    cx.spawn(async move |this, cx| {
        while let Ok(event) = events.recv().await {
            let alive = this.update(cx, |this, cx| {
                match event {
                    // Take the redraw before drawing, so a change made while drawing asks
                    // for another one.
                    TermEvent::Redraw => {
                        dirty.store(false, std::sync::atomic::Ordering::Release);
                    }
                    TermEvent::Status(status) => this.status = status,
                    TermEvent::Closed(Ended::Exited) => {
                        this.closed = Some("the shell exited".to_owned());
                        this.status = None;
                        cx.emit(ShellExited);
                    }
                    TermEvent::Closed(Ended::Lost(reason)) => {
                        this.closed = Some(reason);
                        this.status = None;
                    }
                }
                cx.notify();
            });
            if alive.is_err() {
                break;
            }
        }
    })
}

/// The active theme's terminal colours, resolved for the 256-colour palette.
struct PanelTheme {
    foreground: Rgb,
    background: Rgb,
    cursor: Rgb,
    selection: Rgb,
    selection_fg: Option<Rgb>,
    palette: [Rgb; 256],
}

impl PanelTheme {
    fn app() -> Self {
        let term = palette().term;
        Self {
            foreground: rgb_tuple(term.foreground),
            background: rgb_tuple(term.background),
            cursor: rgb_tuple(term.cursor),
            selection: rgb_tuple(term.selection),
            selection_fg: term.selection_fg.map(rgb_tuple),
            palette: default_palette(&term.ansi),
        }
    }

    fn apply(&self, terminal: &mut Terminal) {
        terminal.set_default_colors(
            self.foreground,
            self.background,
            Some(self.cursor),
            &self.palette,
        );
    }
}

fn rgb_tuple(hex: u32) -> Rgb {
    ((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

fn to_hsla((r, g, b): Rgb) -> Hsla {
    gpui::Rgba {
        r: r as f32 / 255.,
        g: g as f32 / 255.,
        b: b as f32 / 255.,
        a: 1.,
    }
    .into()
}

/// The 256-colour palette: the theme's 16 colours, then the standard cube and gray ramp.
fn default_palette(ansi: &[u32; 16]) -> [Rgb; 256] {
    let mut palette = [(0, 0, 0); 256];
    for (index, &hex) in ansi.iter().enumerate() {
        palette[index] = rgb_tuple(hex);
    }
    let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
    for i in 0..216 {
        palette[16 + i] = (level(i / 36), level(i / 6 % 6), level(i % 6));
    }
    for i in 0..24 {
        let value = (8 + i * 10) as u8;
        palette[232 + i] = (value, value, value);
    }
    palette
}

fn term_mods(m: &Modifiers) -> Mods {
    Mods {
        shift: m.shift,
        ctrl: m.control,
        alt: m.alt,
        super_key: m.platform,
        caps_lock: false,
        num_lock: false,
    }
}

fn term_button(button: MouseButton) -> TermMouseButton {
    match button {
        MouseButton::Left => TermMouseButton::Left,
        MouseButton::Right => TermMouseButton::Right,
        MouseButton::Middle => TermMouseButton::Middle,
        MouseButton::Navigate(_) => TermMouseButton::Four,
    }
}

/// The shell's font, picked once when the view opens: `VMDESK_TERM_FONT` if it is
/// installed, else the first Nerd Font, else the app's monospace family. The fallbacks
/// keep prompt glyphs from borrowing a proportional system font, which would break the
/// cell grid.
fn terminal_font(cx: &App) -> Font {
    let installed = cx.text_system().all_font_names();
    let requested = std::env::var("VMDESK_TERM_FONT").ok();
    let family = font_choice(&installed, requested.as_deref());
    let has = |name: &str| {
        installed
            .iter()
            .any(|installed| installed.eq_ignore_ascii_case(name))
    };
    let mut chosen = font(family.clone());
    let fallbacks: Vec<String> = FONT_FALLBACKS
        .iter()
        .filter(|name| !name.eq_ignore_ascii_case(&family) && has(name))
        .map(|name| (*name).to_owned())
        .collect();
    if !fallbacks.is_empty() {
        chosen.fallbacks = Some(FontFallbacks::from_fonts(fallbacks));
    }
    chosen
}

/// Which family to draw the grid with, given the installed families and the request.
fn font_choice(installed: &[String], requested: Option<&str>) -> String {
    let has = |name: &str| {
        installed
            .iter()
            .any(|installed| installed.eq_ignore_ascii_case(name))
    };
    if let Some(name) = requested
        && (installed.is_empty() || has(name))
    {
        return name.to_owned();
    }
    FONT_PREFERENCE
        .iter()
        .copied()
        .find(|name| has(name))
        .unwrap_or(MONO)
        .to_owned()
}

/// Lines per wheel notch: `VMDESK_SCROLL_LINES`, else 3.
fn scroll_multiplier() -> f32 {
    std::env::var("VMDESK_SCROLL_LINES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3.)
}

fn font_size() -> Pixels {
    px(std::env::var("VMDESK_TERM_FONT_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(14.0))
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// The shell exited on its own, so whoever shows this view should close it.
pub struct ShellExited;

impl EventEmitter<ShellExited> for TerminalView {}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let font = self.font.clone();
        let font_size = font_size();
        let cell = {
            let text = window.text_system();
            let id = text.resolve_font(&font);
            let width = text
                .advance(id, font_size, 'm')
                .map(|s| s.width)
                .unwrap_or(font_size * 0.6);
            let height = (font_size * 1.25).round();
            CellMetrics { width, height }
        };
        self.cell = Some(cell);

        let focused = self.focus_handle.is_focused(window);
        let theme = self.theme();
        {
            let shared = self.term.lock().unwrap();
            // Copy what's on screen, then draw from the copy without the lock.
            self.render_state.update(&shared);
        }
        // Focus reporting, for programs that asked for it (mode 1004).
        if self.reported_focus != Some(focused) {
            self.reported_focus = Some(focused);
            if self.term().dec_mode(FOCUS_REPORTING) {
                self.session.send(encode_focus(focused));
            }
        }
        let frame = PaintFrame::capture(&mut self.render_state, &theme, focused);
        let background = to_hsla(frame.background);
        let status = self.status.clone().or_else(|| {
            self.closed
                .as_ref()
                .map(|reason| format!("Disconnected: {reason}"))
        });
        let entity = cx.entity();
        let scrollbar = self.render_scrollbar(cx);
        let paste_prompt = self
            .pending_paste
            .as_ref()
            .map(|text| self.render_paste_prompt(text, cx));

        div()
            .id(SharedString::from(format!("terminal-{}", self.vm)))
            .track_focus(&self.focus_handle)
            .key_context("Terminal")
            .on_key_down(cx.listener(Self::on_key_down))
            .on_key_up(cx.listener(Self::on_key_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            // A drag that ends outside the pane still has to end.
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.scroll_drag = None;
                    if this.select_anchor.is_some() {
                        this.finish_selection(cx);
                    }
                }),
            )
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(background)
            .child(
                canvas(
                    move |bounds, _window, cx| {
                        entity.update(cx, |this, _| this.request_size(bounds, cell));
                    },
                    move |bounds, (), window, cx| {
                        frame.paint(bounds, cell, font, font_size, window, cx);
                    },
                )
                .size_full(),
            )
            .children(scrollbar)
            .children(paste_prompt)
            .children(status.map(|status| {
                div()
                    .absolute()
                    .bottom_0()
                    .right_0()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(gpui::white().opacity(0.7))
                    .bg(gpui::black().opacity(0.5))
                    .child(status)
            }))
            .children(self.closed.as_ref().map(|_| {
                div().absolute().bottom(px(8.)).left(px(8.)).child(
                    div()
                        .id("terminal-reconnect")
                        .px_3()
                        .py_1()
                        .rounded(px(6.))
                        .text_xs()
                        .cursor_pointer()
                        .bg(rgb(palette().elevated))
                        .hover(|d| d.bg(rgb(palette().hover)))
                        .child("Reconnect")
                        .on_click(cx.listener(|this, _, _, cx| this.reconnect(cx))),
                )
            }))
    }
}

impl TerminalView {
    /// A thin scrollbar over the right edge, when there's scrollback to scroll through.
    fn render_scrollbar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let track = self.track_height?;
        let (has_history, alternate, at_bottom) = {
            let shared = self.term();
            let bar = shared.scrollbar();
            (
                bar.total > bar.len,
                shared.alternate_screen(),
                shared.at_bottom(),
            )
        };
        if !has_history || alternate {
            return None;
        }
        let (top, height) = self.thumb(track);
        let height = height.max(24.);
        let top = top.min(f32::from(track) - height);
        let active = self.scroll_drag.is_some() || !at_bottom;
        let group = SharedString::from(format!("scrollbar-{}", self.vm));
        Some(
            div()
                .id(SharedString::from(format!("scrollbar-track-{}", self.vm)))
                .group(group.clone())
                .absolute()
                .top_0()
                .right_0()
                .w(px(SCROLLBAR_WIDTH))
                .h(track)
                .cursor_default()
                .on_mouse_down(MouseButton::Left, cx.listener(Self::on_scrollbar_down))
                .child(
                    div()
                        .absolute()
                        .top(px(top))
                        .right(px(2.))
                        .w(px(6.))
                        .h(px(height))
                        .rounded_full()
                        .bg(Hsla::from(rgb(palette().text_muted)).opacity(if active {
                            0.55
                        } else {
                            0.3
                        }))
                        .group_hover(group, |d| {
                            d.bg(Hsla::from(rgb(palette().text_muted)).opacity(0.7))
                        }),
                ),
        )
    }

    fn render_paste_prompt(&self, text: &str, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let lines = text.trim_end_matches(['\n', '\r']).lines().count();
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded(px(6.))
                .text_xs()
                .cursor_pointer()
                .map(|d| {
                    if primary {
                        d.bg(rgb(palette().accent))
                            .text_color(rgb(palette().on_accent))
                            .hover(|d| d.opacity(0.9))
                    } else {
                        d.bg(rgb(palette().elevated))
                            .hover(|d| d.bg(rgb(palette().hover)))
                    }
                })
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(label)
        };
        div()
            .absolute()
            .bottom(px(12.))
            .left(px(12.))
            .right(px(12.))
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .rounded(px(10.))
            .border_1()
            .border_color(rgb(palette().border_strong))
            .bg(rgb(palette().elevated))
            .shadow_lg()
            .text_sm()
            .text_color(rgb(palette().text))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(format!("Paste {lines} lines?"))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(palette().text_muted))
                    .child("The program here hasn't turned on bracketed paste, so a shell runs each line as a command as soon as it arrives."),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .justify_end()
                    .child(button("paste-cancel", "Cancel  Esc", false).on_click(cx.listener(|this, _, _, cx| {
                        this.pending_paste = None;
                        cx.notify();
                    })))
                    .child(button("paste-join", "Paste as one line", false).on_click(cx.listener(|this, _, _, cx| this.confirm_paste(true, cx))))
                    .child(button("paste-all", "Paste  Enter", true).on_click(cx.listener(|this, _, _, cx| this.confirm_paste(false, cx)))),
            )
    }
}

/// One cell's look, resolved to colors, so painting needs no access to the terminal.
#[derive(Clone, Copy, PartialEq)]
struct CellStyle {
    fg: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

struct Segment {
    /// Every cell in the segment is one character, so they can be drawn as one run.
    single: bool,
    row: usize,
    col: usize,
    /// How many columns each character advances. 2 for a wide character.
    width: usize,
    text: String,
    style: CellStyle,
}

struct PaintFrame {
    background: Rgb,
    segments: Vec<Segment>,
    backgrounds: Vec<(usize, Range<usize>, Hsla)>,
    cursor: Option<(usize, usize, CursorShape, Hsla, bool)>,
}

impl PaintFrame {
    fn capture(state: &mut RenderState, theme: &PanelTheme, focused: bool) -> Self {
        let frame: TermFrame<'_> = state.frame();
        let default_fg = to_hsla(frame.foreground);
        let default_bg = to_hsla(frame.background);
        let selection_bg = to_hsla(theme.selection);
        let selection_fg = theme.selection_fg.map(to_hsla);
        let mut segments: Vec<Segment> = Vec::new();
        let mut backgrounds: Vec<(usize, Range<usize>, Hsla)> = Vec::new();

        for (row, cells) in frame.rows.iter().enumerate() {
            for (col, cell) in cells.iter().enumerate() {
                if cell.width == Width::Spacer {
                    continue;
                }
                let mut fg = cell.fg.map_or(default_fg, to_hsla);
                let mut bg = cell.bg.map(to_hsla);
                if cell.inverse {
                    let old_fg = fg;
                    fg = bg.unwrap_or(default_bg);
                    bg = Some(old_fg);
                }
                if cell.faint {
                    fg = fg.opacity(0.66);
                }
                if cell.selected {
                    bg = Some(selection_bg);
                    if let Some(selection_fg) = selection_fg {
                        fg = selection_fg;
                    }
                }
                let width = if cell.width == Width::Wide { 2 } else { 1 };
                if let Some(bg) = bg.filter(|bg| *bg != default_bg) {
                    match backgrounds.last_mut() {
                        Some((r, cols, color)) if *r == row && cols.end == col && *color == bg => {
                            cols.end = col + width;
                        }
                        _ => backgrounds.push((row, col..col + width, bg)),
                    }
                }
                if cell.text.is_empty() || cell.text == " " || cell.invisible {
                    continue;
                }
                let style = CellStyle {
                    fg,
                    bold: cell.bold,
                    italic: cell.italic,
                    underline: cell.underline,
                    strike: cell.strikethrough,
                };
                // A cell with combining marks or an emoji sequence draws on its own.
                let single = cell.text.chars().count() == 1;
                match segments.last_mut() {
                    Some(segment)
                        if single
                            && segment.single
                            && segment.row == row
                            && segment.width == width
                            && segment.style == style
                            && segment.col + segment.text.chars().count() * width == col =>
                    {
                        segment.text.push_str(&cell.text);
                    }
                    _ => segments.push(Segment {
                        row,
                        col,
                        width,
                        text: cell.text.clone(),
                        style,
                        single,
                    }),
                }
            }
        }

        let cursor = frame.cursor.as_ref().map(|c| {
            let color = to_hsla(frame.cursor_color.unwrap_or(frame.foreground));
            (c.y as usize, c.x as usize, c.shape, color, focused)
        });

        Self {
            background: frame.background,
            segments,
            backgrounds,
            cursor,
        }
    }

    fn paint(
        self,
        bounds: Bounds<Pixels>,
        cell: CellMetrics,
        font: Font,
        font_size: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) {
        let origin = bounds.origin;
        let at = |row: usize, col: usize| {
            point(
                origin.x + cell.width * col as f32,
                origin.y + cell.height * row as f32,
            )
        };

        for (row, cols, color) in &self.backgrounds {
            let size = size(cell.width * cols.len() as f32, cell.height);
            window.paint_quad(fill(Bounds::new(at(*row, cols.start), size), *color));
        }

        if let Some((row, col, shape, color, focused)) = self.cursor {
            let origin = at(row, col);
            let quad = match (shape, focused) {
                (CursorShape::Bar, _) => {
                    fill(Bounds::new(origin, size(px(2.), cell.height)), color)
                }
                (CursorShape::Underline, _) => fill(
                    Bounds::new(
                        point(origin.x, origin.y + cell.height - px(2.)),
                        size(cell.width, px(2.)),
                    ),
                    color,
                ),
                (CursorShape::Block, true) => fill(
                    Bounds::new(origin, size(cell.width, cell.height)),
                    color.opacity(0.7),
                ),
                // An unfocused pane, or a program asking for it, gets a hollow block.
                _ => outline(
                    Bounds::new(origin, size(cell.width, cell.height)),
                    color,
                    gpui::BorderStyle::Solid,
                ),
            };
            window.paint_quad(quad);
        }

        let text_system = window.text_system().clone();
        for segment in self.segments {
            let mut font = font.clone();
            if segment.style.bold {
                font.weight = FontWeight::BOLD;
            }
            if segment.style.italic {
                font.style = FontStyle::Italic;
            }
            let run = TextRun {
                len: segment.text.len(),
                font,
                color: segment.style.fg,
                background_color: None,
                underline: segment.style.underline.then(|| gpui::UnderlineStyle {
                    thickness: px(1.),
                    color: Some(segment.style.fg),
                    wavy: false,
                }),
                strikethrough: segment.style.strike.then(|| gpui::StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(segment.style.fg),
                }),
            };
            let line = text_system.shape_line(
                segment.text.into(),
                font_size,
                &[run],
                Some(cell.width * segment.width as f32),
            );
            let _ = line.paint(
                at(segment.row, segment.col),
                cell.height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nerd_fonts_are_preferred_and_a_missing_request_falls_back() {
        let installed = vec![
            "Noto Sans Mono".to_owned(),
            "Hack Nerd Font Mono".to_owned(),
        ];
        assert_eq!(font_choice(&installed, None), "Hack Nerd Font Mono");
        assert_eq!(
            font_choice(&installed, Some("Missing Font")),
            "Hack Nerd Font Mono"
        );
        assert_eq!(
            font_choice(&installed, Some("Noto Sans Mono")),
            "Noto Sans Mono"
        );
        assert_eq!(font_choice(&[], None), MONO);
        assert_eq!(font_choice(&[], Some("Anything")), "Anything");
    }
}
