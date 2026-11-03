//! Everything that touches libghostty-vt's C API lives here: the generated bindings, the
//! safe wrapper the app uses, and the input encoders.
//!
//! The wrapper and the terminal/render-state code follow Rex's GPUI client, which runs the
//! same library. The encoders are new: Rex leaves key and mouse encoding to its server,
//! while VMDesk has no server between the pty and the screen.

#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    clippy::all
)]

include!(concat!(env!("OUT_DIR"), "/ghostty_vt.rs"));

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{null, null_mut};

use anyhow::{Result, bail};

use crate::{
    KeyAction, KeyEvent, MouseAction, MouseButton, MouseEvent, Rgb, Scroll, ViewportPoint,
};

/// A libghostty-vt result code, as an error.
fn check(code: GhosttyResult, what: &str) -> Result<()> {
    if code == GHOSTTY_SUCCESS {
        Ok(())
    } else {
        bail!("{what} failed with libghostty-vt result {code}")
    }
}

/// A libghostty-vt struct whose first field is its own size, as `GHOSTTY_INIT_SIZED` makes.
macro_rules! sized {
    ($t:ty) => {{
        let mut value: $t = Default::default();
        value.size = size_of::<$t>();
        value
    }};
}

fn rgb(c: GhosttyColorRgb) -> Rgb {
    (c.r, c.g, c.b)
}

fn color(c: Rgb) -> GhosttyColorRgb {
    GhosttyColorRgb {
        r: c.0,
        g: c.1,
        b: c.2,
    }
}

pub struct Scrollbar {
    /// Rows in the screen and scrollback together.
    pub total: u64,
    /// The first visible row.
    pub offset: u64,
    /// Visible rows.
    pub len: u64,
}

pub struct Terminal(GhosttyTerminal);

// SAFETY: a terminal is only used by one thread at a time, behind a mutex.
unsafe impl Send for Terminal {}

impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe { ghostty_terminal_free(self.0) }
    }
}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Result<Self> {
        let mut term = null_mut();
        check(
            unsafe { ghostty_terminal_new(null(), &mut term, cols.max(1), rows.max(1)) },
            "creating a terminal",
        )?;
        Ok(Self(term))
    }

    /// Restore a terminal from a complete Ghostty snapshot.
    pub fn from_snapshot(bytes: &[u8]) -> Result<Self> {
        let mut decoder = null_mut();
        check(
            unsafe {
                ghostty_snapshot_decoder_new_buf(null(), &mut decoder, bytes.as_ptr(), bytes.len())
            },
            "starting the snapshot decoder",
        )?;
        let compress = true;
        let mut term = null_mut();
        let result = unsafe {
            // Keep restored scrollback compressed rather than briefly holding all of it.
            ghostty_snapshot_decoder_set(
                decoder,
                GHOSTTY_SNAPSHOT_DECODER_OPT_COMPRESS_HISTORY,
                &compress as *const bool as *const c_void,
            );
            let result = ghostty_snapshot_decoder_decode(decoder, &mut term);
            ghostty_snapshot_decoder_free(decoder);
            result
        };
        check(result, "decoding the snapshot")?;
        Ok(Self(term))
    }

    pub fn write(&mut self, bytes: &[u8]) {
        unsafe { ghostty_terminal_vt_write(self.0, bytes.as_ptr(), bytes.len()) }
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell_width: u32, cell_height: u32) {
        unsafe {
            ghostty_terminal_resize(
                self.0,
                cols.max(1),
                rows.max(1),
                cell_width.max(1),
                cell_height.max(1),
            )
        };
    }

    fn get<T: Default>(&self, data: GhosttyTerminalData) -> Option<T> {
        let mut out = T::default();
        let code = unsafe { ghostty_terminal_get(self.0, data, &mut out as *mut T as *mut c_void) };
        (code == GHOSTTY_SUCCESS).then_some(out)
    }

    pub fn cols(&self) -> u16 {
        self.get(GHOSTTY_TERMINAL_DATA_COLS).unwrap_or(80)
    }

    pub fn rows(&self) -> u16 {
        self.get(GHOSTTY_TERMINAL_DATA_ROWS).unwrap_or(24)
    }

    /// Set the default colors, which a program's own color changes still override.
    pub fn set_default_colors(
        &mut self,
        fg: Rgb,
        bg: Rgb,
        cursor: Option<Rgb>,
        palette: &[Rgb; 256],
    ) {
        let palette: Vec<GhosttyColorRgb> = palette.iter().copied().map(color).collect();
        let (fg, bg) = (color(fg), color(bg));
        let cursor = cursor.map(color);
        unsafe {
            ghostty_terminal_set(
                self.0,
                GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND,
                &fg as *const _ as *const c_void,
            );
            ghostty_terminal_set(
                self.0,
                GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND,
                &bg as *const _ as *const c_void,
            );
            ghostty_terminal_set(
                self.0,
                GHOSTTY_TERMINAL_OPT_COLOR_CURSOR,
                cursor
                    .as_ref()
                    .map_or(null(), |c| c as *const _ as *const c_void),
            );
            ghostty_terminal_set(
                self.0,
                GHOSTTY_TERMINAL_OPT_COLOR_PALETTE,
                palette.as_ptr() as *const c_void,
            );
        }
    }

    pub fn scroll(&mut self, to: Scroll) {
        let mut behavior = GhosttyTerminalScrollViewport::default();
        match to {
            Scroll::Delta(delta) => {
                behavior.tag = GHOSTTY_SCROLL_VIEWPORT_DELTA;
                behavior.value.delta = delta;
            }
            Scroll::Bottom => behavior.tag = GHOSTTY_SCROLL_VIEWPORT_BOTTOM,
            Scroll::Row(row) => {
                behavior.tag = GHOSTTY_SCROLL_VIEWPORT_ROW;
                behavior.value.row = row;
            }
        }
        unsafe { ghostty_terminal_scroll_viewport(self.0, behavior) }
    }

    pub fn scrollbar(&self) -> Scrollbar {
        let s: GhosttyTerminalScrollbar = self
            .get(GHOSTTY_TERMINAL_DATA_SCROLLBAR)
            .unwrap_or_default();
        Scrollbar {
            total: s.total,
            offset: s.offset,
            len: s.len,
        }
    }

    /// Whether the view shows the newest output.
    pub fn at_bottom(&self) -> bool {
        let s = self.scrollbar();
        s.offset + s.len >= s.total
    }

    /// Whether a DEC private mode, such as 2004 for bracketed paste, is on.
    pub fn dec_mode(&self, mode: u16) -> bool {
        let mut config = GhosttyTerminalModeConfig {
            mode: mode & 0x7fff,
            value: false,
        };
        let code = unsafe {
            ghostty_terminal_get(
                self.0,
                GHOSTTY_TERMINAL_DATA_MODE,
                &mut config as *mut _ as *mut c_void,
            )
        };
        code == GHOSTTY_SUCCESS && config.value
    }

    pub fn mouse_tracking(&self) -> bool {
        self.get::<bool>(GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING)
            .unwrap_or(false)
    }

    pub fn alternate_screen(&self) -> bool {
        self.get::<GhosttyTerminalScreen>(GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN)
            == Some(GHOSTTY_TERMINAL_SCREEN_ALTERNATE)
    }

    /// The cell at `point`, as a reference that stays valid until the terminal changes.
    fn grid_ref(&self, point: ViewportPoint) -> Option<GhosttyGridRef> {
        let mut p = GhosttyPoint {
            tag: GHOSTTY_POINT_TAG_VIEWPORT,
            ..Default::default()
        };
        p.value.coordinate = GhosttyPointCoordinate {
            x: point.x,
            y: point.y as u32,
        };
        let mut out = sized!(GhosttyGridRef);
        let code = unsafe { ghostty_terminal_grid_ref(self.0, p, &mut out) };
        (code == GHOSTTY_SUCCESS).then_some(out)
    }

    fn set_selection(&mut self, selection: Option<&GhosttySelection>) {
        let value = selection.map_or(null(), |s| s as *const _ as *const c_void);
        unsafe { ghostty_terminal_set(self.0, GHOSTTY_TERMINAL_OPT_SELECTION, value) };
    }

    /// Select from `start` to `end`, both included.
    pub fn select(&mut self, start: ViewportPoint, end: ViewportPoint) {
        let (Some(start), Some(end)) = (self.grid_ref(start), self.grid_ref(end)) else {
            return;
        };
        let mut selection = sized!(GhosttySelection);
        selection.start = start;
        selection.end = end;
        self.set_selection(Some(&selection));
    }

    pub fn select_word(&mut self, at: ViewportPoint) {
        let Some(at) = self.grid_ref(at) else { return };
        let mut options = sized!(GhosttyTerminalSelectWordOptions);
        options.ref_ = at;
        let mut selection = sized!(GhosttySelection);
        if unsafe { ghostty_terminal_select_word(self.0, &options, &mut selection) }
            == GHOSTTY_SUCCESS
        {
            self.set_selection(Some(&selection));
        }
    }

    pub fn select_line(&mut self, at: ViewportPoint) {
        let Some(at) = self.grid_ref(at) else { return };
        let mut options = sized!(GhosttyTerminalSelectLineOptions);
        options.ref_ = at;
        let mut selection = sized!(GhosttySelection);
        if unsafe { ghostty_terminal_select_line(self.0, &options, &mut selection) }
            == GHOSTTY_SUCCESS
        {
            self.set_selection(Some(&selection));
        }
    }

    pub fn clear_selection(&mut self) {
        self.set_selection(None);
    }

    pub fn has_selection(&self) -> bool {
        let mut selection = sized!(GhosttySelection);
        let code = unsafe {
            ghostty_terminal_get(
                self.0,
                GHOSTTY_TERMINAL_DATA_SELECTION,
                &mut selection as *mut _ as *mut c_void,
            )
        };
        code == GHOSTTY_SUCCESS
    }

    /// The selected text, with soft-wrapped lines joined.
    pub fn selection_text(&self) -> Option<String> {
        let mut options = sized!(GhosttyTerminalSelectionFormatOptions);
        options.emit = GHOSTTY_FORMATTER_FORMAT_PLAIN;
        options.unwrap = true;
        options.trim = true;
        let (mut ptr, mut len) = (null_mut(), 0usize);
        let code = unsafe {
            ghostty_terminal_selection_format_alloc(self.0, null(), options, &mut ptr, &mut len)
        };
        if code != GHOSTTY_SUCCESS || ptr.is_null() {
            return None;
        }
        let text =
            unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned() };
        unsafe { ghostty_free(null(), ptr, len) };
        Some(text)
    }

    /// The screen and scrollback as plain text.
    pub fn plain_text(&self) -> String {
        let mut options = sized!(GhosttyFormatterTerminalOptions);
        options.emit = GHOSTTY_FORMATTER_FORMAT_PLAIN;
        options.trim = true;
        let mut formatter = null_mut();
        if unsafe { ghostty_formatter_terminal_new(null(), &mut formatter, self.0, options) }
            != GHOSTTY_SUCCESS
        {
            return String::new();
        }
        let (mut ptr, mut len) = (null_mut(), 0usize);
        let code = unsafe { ghostty_formatter_format_alloc(formatter, null(), &mut ptr, &mut len) };
        let text = if code == GHOSTTY_SUCCESS && !ptr.is_null() {
            let text = unsafe {
                String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned()
            };
            unsafe { ghostty_free(null(), ptr, len) };
            text
        } else {
            String::new()
        };
        unsafe { ghostty_formatter_free(formatter) };
        text
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Width {
    Narrow,
    /// The first of the two columns a wide character takes.
    Wide,
    /// The second column of a wide character, or a placeholder for one that wrapped.
    Spacer,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorShape {
    Block,
    HollowBlock,
    Bar,
    Underline,
}

/// One cell, ready to draw. Colors are `None` for the terminal's default.
#[derive(Clone)]
pub struct Cell {
    pub text: String,
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub width: Width,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub selected: bool,
}

pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub shape: CursorShape,
}

/// Everything needed to draw one frame of a terminal.
pub struct Frame<'a> {
    pub rows: &'a [Vec<Cell>],
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor_color: Option<Rgb>,
    pub cursor: Option<Cursor>,
}

/// A copy of what a terminal shows, taken while the terminal is locked, then read without
/// the lock.
pub struct RenderState {
    state: GhosttyRenderState,
    rows: GhosttyRenderStateRowIterator,
    cells: GhosttyRenderStateRowCells,
    /// Each row's cells as last read. Only rows libghostty-vt marks dirty are read again.
    cache: Vec<Vec<Cell>>,
    /// Read every row on the next frame, as after the default colors change.
    stale: bool,
}

// SAFETY: a render state belongs to one pane and is used from the UI thread only.
unsafe impl Send for RenderState {}

impl Drop for RenderState {
    fn drop(&mut self) {
        unsafe {
            ghostty_render_state_row_cells_free(self.cells);
            ghostty_render_state_row_iterator_free(self.rows);
            ghostty_render_state_free(self.state);
        }
    }
}

impl RenderState {
    pub fn new() -> Result<Self> {
        let (mut state, mut rows, mut cells) = (null_mut(), null_mut(), null_mut());
        unsafe {
            check(
                ghostty_render_state_new(null(), &mut state),
                "creating a render state",
            )?;
            check(
                ghostty_render_state_row_iterator_new(null(), &mut rows),
                "creating a row iterator",
            )?;
            check(
                ghostty_render_state_row_cells_new(null(), &mut cells),
                "creating a cell iterator",
            )?;
        }
        Ok(Self {
            state,
            rows,
            cells,
            cache: Vec::new(),
            stale: true,
        })
    }

    /// Copy the terminal's current state. Call it with the terminal locked; reading the copy
    /// with `frame` needs no lock.
    pub fn update(&mut self, terminal: &Terminal) {
        unsafe { ghostty_render_state_update(self.state, terminal.0) };
    }

    /// Read every row again on the next frame, not just the ones libghostty-vt marks dirty.
    pub fn invalidate(&mut self) {
        self.stale = true;
    }

    fn get<T>(&self, data: GhosttyRenderStateData, out: &mut T) -> bool {
        unsafe {
            ghostty_render_state_get(self.state, data, out as *mut T as *mut c_void)
                == GHOSTTY_SUCCESS
        }
    }

    /// The frame to draw. Rows that haven't changed since the last frame come from the cache.
    pub fn frame(&mut self) -> Frame<'_> {
        let mut colors = sized!(GhosttyRenderStateColors);
        self.get(GHOSTTY_RENDER_STATE_DATA_COLORS, &mut colors);

        let mut c = sized!(GhosttyRenderStateCursor);
        self.get(GHOSTTY_RENDER_STATE_DATA_CURSOR, &mut c);
        let cursor = (c.visible && c.viewport_has_value).then_some(Cursor {
            x: c.viewport_x,
            y: c.viewport_y,
            shape: match c.visual_style {
                GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR => CursorShape::Bar,
                GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE => CursorShape::Underline,
                GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW => CursorShape::HollowBlock,
                _ => CursorShape::Block,
            },
        });

        let mut dirty: GhosttyRenderStateDirty = GHOSTTY_RENDER_STATE_DIRTY_FULL;
        self.get(GHOSTTY_RENDER_STATE_DATA_DIRTY, &mut dirty);
        let mut rows: u16 = 0;
        self.get(GHOSTTY_RENDER_STATE_DATA_ROWS, &mut rows);
        let full = std::mem::take(&mut self.stale)
            || dirty == GHOSTTY_RENDER_STATE_DIRTY_FULL
            || self.cache.len() != rows as usize;
        if full || dirty != GHOSTTY_RENDER_STATE_DIRTY_FALSE {
            self.cache.resize_with(rows as usize, Vec::new);
            unsafe {
                // Point the row iterator at this frame's rows.
                ghostty_render_state_get(
                    self.state,
                    GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                    &mut self.rows as *mut _ as *mut c_void,
                );
                let mut y = 0;
                while ghostty_render_state_row_iterator_next(self.rows) && y < self.cache.len() {
                    let mut row_dirty = true;
                    if !full {
                        ghostty_render_state_row_get(
                            self.rows,
                            GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY,
                            &mut row_dirty as *mut _ as *mut c_void,
                        );
                    }
                    if row_dirty {
                        // Point the cell iterator at this row's cells.
                        ghostty_render_state_row_get(
                            self.rows,
                            GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                            &mut self.cells as *mut _ as *mut c_void,
                        );
                        let mut row = std::mem::take(&mut self.cache[y]);
                        row.clear();
                        while ghostty_render_state_row_cells_next(self.cells) {
                            row.push(self.cell());
                        }
                        self.cache[y] = row;
                    }
                    y += 1;
                }
                // What's cached now matches the terminal, so the next frame starts clean.
                ghostty_render_state_clean(self.state);
            }
        }
        Frame {
            rows: &self.cache,
            foreground: rgb(colors.foreground),
            background: rgb(colors.background),
            cursor_color: colors.cursor_has_value.then(|| rgb(colors.cursor)),
            cursor,
        }
    }

    /// The cell the cell iterator is on.
    unsafe fn cell(&self) -> Cell {
        let get = |data, out: *mut c_void| unsafe {
            ghostty_render_state_row_cells_get(self.cells, data, out)
        };
        let mut len: u32 = 0;
        get(
            GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN,
            &mut len as *mut _ as *mut c_void,
        );
        let text = if len == 0 {
            String::new()
        } else {
            let mut codepoints = vec![0u32; len as usize];
            get(
                GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
                codepoints.as_mut_ptr() as *mut c_void,
            );
            codepoints.into_iter().filter_map(char::from_u32).collect()
        };
        let mut raw: GhosttyCell = Default::default();
        get(
            GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW,
            &mut raw as *mut _ as *mut c_void,
        );
        let mut wide: GhosttyCellWide = GHOSTTY_CELL_WIDE_NARROW;
        unsafe {
            ghostty_cell_get(
                raw,
                GHOSTTY_CELL_DATA_WIDE,
                &mut wide as *mut _ as *mut c_void,
            )
        };
        let width = match wide {
            GHOSTTY_CELL_WIDE_WIDE => Width::Wide,
            GHOSTTY_CELL_WIDE_SPACER_TAIL | GHOSTTY_CELL_WIDE_SPACER_HEAD => Width::Spacer,
            _ => Width::Narrow,
        };
        let color = |data| {
            let mut c = GhosttyColorRgb::default();
            (get(data, &mut c as *mut _ as *mut c_void) == GHOSTTY_SUCCESS).then(|| rgb(c))
        };
        let mut style = sized!(GhosttyStyle);
        let mut styled = false;
        get(
            GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_HAS_STYLING,
            &mut styled as *mut _ as *mut c_void,
        );
        if styled {
            get(
                GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
                &mut style as *mut _ as *mut c_void,
            );
        }
        let mut selected = false;
        get(
            GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_SELECTED,
            &mut selected as *mut _ as *mut c_void,
        );
        Cell {
            text,
            fg: color(GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR),
            bg: color(GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR),
            width,
            bold: style.bold,
            italic: style.italic,
            faint: style.faint,
            inverse: style.inverse,
            invisible: style.invisible,
            underline: style.underline != GHOSTTY_SGR_UNDERLINE_NONE,
            strikethrough: style.strikethrough,
            selected,
        }
    }
}

/// Ask libghostty-vt for the size of an encoded input sequence, then write it.
fn encode_alloc(
    mut call: impl FnMut(*mut std::os::raw::c_char, usize, *mut usize) -> GhosttyResult,
) -> Vec<u8> {
    let mut written = 0usize;
    let code = call(null_mut(), 0, &mut written);
    if code != GHOSTTY_OUT_OF_SPACE || written == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; written];
    let code = call(
        buf.as_mut_ptr() as *mut std::os::raw::c_char,
        buf.len(),
        &mut written,
    );
    if code != GHOSTTY_SUCCESS {
        return Vec::new();
    }
    buf.truncate(written);
    buf
}

/// Encodes key presses against the terminal's current modes (cursor keys, Kitty protocol,
/// modifyOtherKeys, backarrow).
pub struct KeyEncoder(GhosttyKeyEncoder);

impl Drop for KeyEncoder {
    fn drop(&mut self) {
        unsafe { ghostty_key_encoder_free(self.0) };
    }
}

impl KeyEncoder {
    pub fn new() -> Result<Self> {
        let mut encoder = null_mut();
        check(
            unsafe { ghostty_key_encoder_new(null(), &mut encoder) },
            "creating a key encoder",
        )?;
        Ok(Self(encoder))
    }

    /// Take cursor-key, keypad, modifyOtherKeys and Kitty flags from the terminal. Call it
    /// before encoding, since a program can change any of them at any time.
    pub fn sync(&mut self, terminal: &Terminal) {
        unsafe { ghostty_key_encoder_setopt_from_terminal(self.0, terminal.0) };
    }

    /// The bytes to write to the pty. Empty when the key produces nothing (a bare
    /// modifier, a release without the Kitty protocol, …).
    pub fn encode(&mut self, event: &KeyEvent<'_>) -> Vec<u8> {
        let mut ev = null_mut();
        if unsafe { ghostty_key_event_new(null(), &mut ev) } != GHOSTTY_SUCCESS {
            return Vec::new();
        }
        let out = unsafe {
            ghostty_key_event_set_action(
                ev,
                match event.action {
                    KeyAction::Press => GHOSTTY_KEY_ACTION_PRESS,
                    KeyAction::Release => GHOSTTY_KEY_ACTION_RELEASE,
                    KeyAction::Repeat => GHOSTTY_KEY_ACTION_REPEAT,
                },
            );
            ghostty_key_event_set_key(ev, event.key.0);
            ghostty_key_event_set_mods(ev, event.mods.bits());
            ghostty_key_event_set_consumed_mods(ev, event.consumed_mods.bits());
            ghostty_key_event_set_composing(ev, event.composing);
            if let Some(text) = event.utf8 {
                ghostty_key_event_set_utf8(
                    ev,
                    text.as_ptr() as *const std::os::raw::c_char,
                    text.len(),
                );
            }
            if let Some(c) = event.unshifted_codepoint {
                ghostty_key_event_set_unshifted_codepoint(ev, c as u32);
            }
            let out = encode_alloc(|buf, len, written| {
                ghostty_key_encoder_encode(self.0, ev, buf, len, written)
            });
            ghostty_key_event_free(ev);
            out
        };
        out
    }
}

/// Encodes mouse events in the format the program asked for (X10, UTF-8, SGR, URxvt,
/// SGR-pixels).
pub struct MouseEncoder(GhosttyMouseEncoder);

impl Drop for MouseEncoder {
    fn drop(&mut self) {
        unsafe { ghostty_mouse_encoder_free(self.0) };
    }
}

impl MouseEncoder {
    pub fn new() -> Result<Self> {
        let mut encoder = null_mut();
        check(
            unsafe { ghostty_mouse_encoder_new(null(), &mut encoder) },
            "creating a mouse encoder",
        )?;
        Ok(Self(encoder))
    }

    /// The pixel geometry of the grid, which SGR-pixels and the cell rounding need.
    pub fn set_size(
        &mut self,
        screen_width: u32,
        screen_height: u32,
        cell_width: u32,
        cell_height: u32,
    ) {
        let mut size = sized!(GhosttyMouseEncoderSize);
        size.screen_width = screen_width;
        size.screen_height = screen_height;
        size.cell_width = cell_width.max(1);
        size.cell_height = cell_height.max(1);
        unsafe {
            ghostty_mouse_encoder_setopt(
                self.0,
                GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                &size as *const _ as *const c_void,
            );
        }
    }

    /// Whether any button is held, which motion events need to be encoded as drags.
    pub fn set_any_button_pressed(&mut self, pressed: bool) {
        unsafe {
            ghostty_mouse_encoder_setopt(
                self.0,
                GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED,
                &pressed as *const bool as *const c_void,
            );
        }
    }

    /// Take the tracking mode and output format from the terminal.
    pub fn sync(&mut self, terminal: &Terminal) {
        unsafe { ghostty_mouse_encoder_setopt_from_terminal(self.0, terminal.0) };
    }

    pub fn encode(&mut self, event: &MouseEvent) -> Vec<u8> {
        let mut ev = null_mut();
        if unsafe { ghostty_mouse_event_new(null(), &mut ev) } != GHOSTTY_SUCCESS {
            return Vec::new();
        }
        let out = unsafe {
            ghostty_mouse_event_set_action(
                ev,
                match event.action {
                    MouseAction::Press => GHOSTTY_MOUSE_ACTION_PRESS,
                    MouseAction::Release => GHOSTTY_MOUSE_ACTION_RELEASE,
                    MouseAction::Motion => GHOSTTY_MOUSE_ACTION_MOTION,
                },
            );
            match event.button {
                Some(button) => ghostty_mouse_event_set_button(ev, mouse_button(button)),
                None => ghostty_mouse_event_clear_button(ev),
            }
            ghostty_mouse_event_set_mods(ev, event.mods.bits());
            ghostty_mouse_event_set_position(
                ev,
                GhosttyMousePosition {
                    x: event.x,
                    y: event.y,
                },
            );
            let out = encode_alloc(|buf, len, written| {
                ghostty_mouse_encoder_encode(self.0, ev, buf, len, written)
            });
            ghostty_mouse_event_free(ev);
            out
        };
        out
    }
}

fn mouse_button(button: MouseButton) -> GhosttyMouseButton {
    match button {
        MouseButton::Left => GHOSTTY_MOUSE_BUTTON_LEFT,
        MouseButton::Right => GHOSTTY_MOUSE_BUTTON_RIGHT,
        MouseButton::Middle => GHOSTTY_MOUSE_BUTTON_MIDDLE,
        MouseButton::Four => GHOSTTY_MOUSE_BUTTON_FOUR,
        MouseButton::Five => GHOSTTY_MOUSE_BUTTON_FIVE,
        MouseButton::Six => GHOSTTY_MOUSE_BUTTON_SIX,
        MouseButton::Seven => GHOSTTY_MOUSE_BUTTON_SEVEN,
        MouseButton::Eight => GHOSTTY_MOUSE_BUTTON_EIGHT,
        MouseButton::Nine => GHOSTTY_MOUSE_BUTTON_NINE,
        MouseButton::Ten => GHOSTTY_MOUSE_BUTTON_TEN,
        MouseButton::Eleven => GHOSTTY_MOUSE_BUTTON_ELEVEN,
    }
}

/// Encode text for the pty: bracketed paste when the program asked for it, unsafe control
/// bytes replaced, and newlines as carriage returns otherwise.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut data = text.as_bytes().to_vec();
    // The encoder replaces bytes one for one and adds at most the two bracketed markers.
    let mut out = vec![0u8; data.len() + 16];
    let mut written = 0usize;
    let code = unsafe {
        ghostty_paste_encode(
            data.as_mut_ptr() as *mut std::os::raw::c_char,
            data.len(),
            bracketed,
            out.as_mut_ptr() as *mut std::os::raw::c_char,
            out.len(),
            &mut written,
        )
    };
    if code != GHOSTTY_SUCCESS {
        return Vec::new();
    }
    out.truncate(written);
    out
}

/// The CSI I / CSI O focus report, when focus reporting (mode 1004) may be on.
pub fn encode_focus(gained: bool) -> Vec<u8> {
    encode_alloc(|buf, len, written| unsafe {
        ghostty_focus_encode(
            if gained {
                GHOSTTY_FOCUS_GAINED
            } else {
                GHOSTTY_FOCUS_LOST
            },
            buf,
            len,
            written,
        )
    })
}

/// The libghostty-vt key for a W3C UI Events `code`.
pub fn key_from_code(code: &str) -> i32 {
    match code {
        "key_a" => GHOSTTY_KEY_A,
        "key_b" => GHOSTTY_KEY_B,
        "key_c" => GHOSTTY_KEY_C,
        "key_d" => GHOSTTY_KEY_D,
        "key_e" => GHOSTTY_KEY_E,
        "key_f" => GHOSTTY_KEY_F,
        "key_g" => GHOSTTY_KEY_G,
        "key_h" => GHOSTTY_KEY_H,
        "key_i" => GHOSTTY_KEY_I,
        "key_j" => GHOSTTY_KEY_J,
        "key_k" => GHOSTTY_KEY_K,
        "key_l" => GHOSTTY_KEY_L,
        "key_m" => GHOSTTY_KEY_M,
        "key_n" => GHOSTTY_KEY_N,
        "key_o" => GHOSTTY_KEY_O,
        "key_p" => GHOSTTY_KEY_P,
        "key_q" => GHOSTTY_KEY_Q,
        "key_r" => GHOSTTY_KEY_R,
        "key_s" => GHOSTTY_KEY_S,
        "key_t" => GHOSTTY_KEY_T,
        "key_u" => GHOSTTY_KEY_U,
        "key_v" => GHOSTTY_KEY_V,
        "key_w" => GHOSTTY_KEY_W,
        "key_x" => GHOSTTY_KEY_X,
        "key_y" => GHOSTTY_KEY_Y,
        "key_z" => GHOSTTY_KEY_Z,
        "digit_0" => GHOSTTY_KEY_DIGIT_0,
        "digit_1" => GHOSTTY_KEY_DIGIT_1,
        "digit_2" => GHOSTTY_KEY_DIGIT_2,
        "digit_3" => GHOSTTY_KEY_DIGIT_3,
        "digit_4" => GHOSTTY_KEY_DIGIT_4,
        "digit_5" => GHOSTTY_KEY_DIGIT_5,
        "digit_6" => GHOSTTY_KEY_DIGIT_6,
        "digit_7" => GHOSTTY_KEY_DIGIT_7,
        "digit_8" => GHOSTTY_KEY_DIGIT_8,
        "digit_9" => GHOSTTY_KEY_DIGIT_9,
        "backquote" => GHOSTTY_KEY_BACKQUOTE,
        "backslash" => GHOSTTY_KEY_BACKSLASH,
        "bracket_left" => GHOSTTY_KEY_BRACKET_LEFT,
        "bracket_right" => GHOSTTY_KEY_BRACKET_RIGHT,
        "comma" => GHOSTTY_KEY_COMMA,
        "equal" => GHOSTTY_KEY_EQUAL,
        "intl_backslash" => GHOSTTY_KEY_INTL_BACKSLASH,
        "intl_ro" => GHOSTTY_KEY_INTL_RO,
        "intl_yen" => GHOSTTY_KEY_INTL_YEN,
        "minus" => GHOSTTY_KEY_MINUS,
        "period" => GHOSTTY_KEY_PERIOD,
        "quote" => GHOSTTY_KEY_QUOTE,
        "semicolon" => GHOSTTY_KEY_SEMICOLON,
        "slash" => GHOSTTY_KEY_SLASH,
        "alt_left" => GHOSTTY_KEY_ALT_LEFT,
        "alt_right" => GHOSTTY_KEY_ALT_RIGHT,
        "backspace" => GHOSTTY_KEY_BACKSPACE,
        "caps_lock" => GHOSTTY_KEY_CAPS_LOCK,
        "context_menu" => GHOSTTY_KEY_CONTEXT_MENU,
        "control_left" => GHOSTTY_KEY_CONTROL_LEFT,
        "control_right" => GHOSTTY_KEY_CONTROL_RIGHT,
        "enter" => GHOSTTY_KEY_ENTER,
        "meta_left" => GHOSTTY_KEY_META_LEFT,
        "meta_right" => GHOSTTY_KEY_META_RIGHT,
        "shift_left" => GHOSTTY_KEY_SHIFT_LEFT,
        "shift_right" => GHOSTTY_KEY_SHIFT_RIGHT,
        "space" => GHOSTTY_KEY_SPACE,
        "tab" => GHOSTTY_KEY_TAB,
        "convert" => GHOSTTY_KEY_CONVERT,
        "kana_mode" => GHOSTTY_KEY_KANA_MODE,
        "non_convert" => GHOSTTY_KEY_NON_CONVERT,
        "delete" => GHOSTTY_KEY_DELETE,
        "end" => GHOSTTY_KEY_END,
        "help" => GHOSTTY_KEY_HELP,
        "home" => GHOSTTY_KEY_HOME,
        "insert" => GHOSTTY_KEY_INSERT,
        "page_down" => GHOSTTY_KEY_PAGE_DOWN,
        "page_up" => GHOSTTY_KEY_PAGE_UP,
        "arrow_down" => GHOSTTY_KEY_ARROW_DOWN,
        "arrow_left" => GHOSTTY_KEY_ARROW_LEFT,
        "arrow_right" => GHOSTTY_KEY_ARROW_RIGHT,
        "arrow_up" => GHOSTTY_KEY_ARROW_UP,
        "num_lock" => GHOSTTY_KEY_NUM_LOCK,
        "numpad_0" => GHOSTTY_KEY_NUMPAD_0,
        "numpad_1" => GHOSTTY_KEY_NUMPAD_1,
        "numpad_2" => GHOSTTY_KEY_NUMPAD_2,
        "numpad_3" => GHOSTTY_KEY_NUMPAD_3,
        "numpad_4" => GHOSTTY_KEY_NUMPAD_4,
        "numpad_5" => GHOSTTY_KEY_NUMPAD_5,
        "numpad_6" => GHOSTTY_KEY_NUMPAD_6,
        "numpad_7" => GHOSTTY_KEY_NUMPAD_7,
        "numpad_8" => GHOSTTY_KEY_NUMPAD_8,
        "numpad_9" => GHOSTTY_KEY_NUMPAD_9,
        "numpad_add" => GHOSTTY_KEY_NUMPAD_ADD,
        "numpad_backspace" => GHOSTTY_KEY_NUMPAD_BACKSPACE,
        "numpad_clear" => GHOSTTY_KEY_NUMPAD_CLEAR,
        "numpad_clear_entry" => GHOSTTY_KEY_NUMPAD_CLEAR_ENTRY,
        "numpad_comma" => GHOSTTY_KEY_NUMPAD_COMMA,
        "numpad_decimal" => GHOSTTY_KEY_NUMPAD_DECIMAL,
        "numpad_divide" => GHOSTTY_KEY_NUMPAD_DIVIDE,
        "numpad_enter" => GHOSTTY_KEY_NUMPAD_ENTER,
        "numpad_equal" => GHOSTTY_KEY_NUMPAD_EQUAL,
        "numpad_multiply" => GHOSTTY_KEY_NUMPAD_MULTIPLY,
        "numpad_subtract" => GHOSTTY_KEY_NUMPAD_SUBTRACT,
        "escape" => GHOSTTY_KEY_ESCAPE,
        "f1" => GHOSTTY_KEY_F1,
        "f2" => GHOSTTY_KEY_F2,
        "f3" => GHOSTTY_KEY_F3,
        "f4" => GHOSTTY_KEY_F4,
        "f5" => GHOSTTY_KEY_F5,
        "f6" => GHOSTTY_KEY_F6,
        "f7" => GHOSTTY_KEY_F7,
        "f8" => GHOSTTY_KEY_F8,
        "f9" => GHOSTTY_KEY_F9,
        "f10" => GHOSTTY_KEY_F10,
        "f11" => GHOSTTY_KEY_F11,
        "f12" => GHOSTTY_KEY_F12,
        "f13" => GHOSTTY_KEY_F13,
        "f14" => GHOSTTY_KEY_F14,
        "f15" => GHOSTTY_KEY_F15,
        "f16" => GHOSTTY_KEY_F16,
        "f17" => GHOSTTY_KEY_F17,
        "f18" => GHOSTTY_KEY_F18,
        "f19" => GHOSTTY_KEY_F19,
        "f20" => GHOSTTY_KEY_F20,
        "f21" => GHOSTTY_KEY_F21,
        "f22" => GHOSTTY_KEY_F22,
        "f23" => GHOSTTY_KEY_F23,
        "f24" => GHOSTTY_KEY_F24,
        "f25" => GHOSTTY_KEY_F25,
        "fn" => GHOSTTY_KEY_FN,
        "fn_lock" => GHOSTTY_KEY_FN_LOCK,
        "print_screen" => GHOSTTY_KEY_PRINT_SCREEN,
        "scroll_lock" => GHOSTTY_KEY_SCROLL_LOCK,
        "pause" => GHOSTTY_KEY_PAUSE,
        "copy" => GHOSTTY_KEY_COPY,
        "cut" => GHOSTTY_KEY_CUT,
        "paste" => GHOSTTY_KEY_PASTE,
        _ => GHOSTTY_KEY_UNIDENTIFIED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Key, Mods};

    #[test]
    fn writes_and_renders() {
        let mut term = Terminal::new(20, 3).unwrap();
        term.write("hi \x1b[1;31mred\x1b[0m 日本\r\n".as_bytes());
        let mut state = RenderState::new().unwrap();
        state.update(&term);
        let frame = state.frame();
        assert_eq!(frame.rows.len(), 3);
        let row = &frame.rows[0];
        let text: String = row
            .iter()
            .filter(|c| c.width != Width::Spacer)
            .map(|c| c.text.as_str())
            .collect();
        assert!(
            text.starts_with("hired日本") || text.starts_with("hi"),
            "{text:?}"
        );
        assert!(row[3].bold);
        assert!(row[3].fg.is_some());
        assert_eq!(row[7].width, Width::Wide);
        assert_eq!(row[8].width, Width::Spacer);
        assert!(term.plain_text().starts_with("hi red 日本"));
    }

    #[test]
    fn selects_and_copies() {
        let mut term = Terminal::new(20, 3).unwrap();
        term.write(b"hello world\r\n");
        term.select(ViewportPoint { x: 6, y: 0 }, ViewportPoint { x: 10, y: 0 });
        assert!(term.has_selection());
        assert_eq!(term.selection_text().as_deref(), Some("world"));
        term.clear_selection();
        assert!(!term.has_selection());
    }

    #[test]
    fn encodes_keys_and_paste() {
        let term = Terminal::new(20, 3).unwrap();
        let mut keys = KeyEncoder::new().unwrap();
        keys.sync(&term);
        let event = KeyEvent {
            key: Key::from_code("key_a"),
            action: KeyAction::Press,
            mods: Mods::default(),
            consumed_mods: Mods::default(),
            utf8: Some("a"),
            unshifted_codepoint: Some('a'),
            composing: false,
        };
        assert_eq!(keys.encode(&event), b"a");
        // The same key with ctrl becomes the control byte.
        let ctrl = KeyEvent {
            mods: Mods {
                ctrl: true,
                ..Mods::default()
            },
            consumed_mods: Mods {
                ctrl: true,
                ..Mods::default()
            },
            ..event
        };
        assert_eq!(keys.encode(&ctrl), b"\x01");

        assert_eq!(
            encode_paste("one\ntwo", true),
            b"\x1b[200~one\ntwo\x1b[201~"
        );
        assert_eq!(encode_focus(true), b"\x1b[I");
        assert_eq!(encode_focus(false), b"\x1b[O");
    }

    #[test]
    fn moves_the_viewport() {
        let mut term = Terminal::new(20, 3).unwrap();
        for i in 0..10 {
            term.write(format!("line {i}\r\n").as_bytes());
        }
        assert!(term.scrollbar().total > term.scrollbar().len);
        term.scroll(Scroll::Delta(-4));
        assert!(!term.at_bottom());
        term.scroll(Scroll::Bottom);
        assert!(term.at_bottom());
    }
}
