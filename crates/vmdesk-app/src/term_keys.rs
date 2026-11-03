//! Translate GPUI keystrokes into libghostty-vt key events.
//!
//! libghostty-vt wants the physical key as a W3C `code` and leaves the escape-sequence
//! encoding, including the Kitty keyboard protocol, to its encoder. GPUI on Linux reports
//! the key's symbol instead, and for shifted symbols such as `!` it clears `shift`, so we
//! map each symbol back to its key on a US layout and restore the shift.

use gpui::{Keystroke, Modifiers};
use vmdesk_terminal::{Key, KeyAction, KeyEvent, Mods};

/// `(code, unshifted character, symbol implies shift)` for a GPUI key name.
fn physical_key(key: &str) -> Option<(&'static str, Option<char>, bool)> {
    const LETTERS: [&str; 26] = [
        "key_a", "key_b", "key_c", "key_d", "key_e", "key_f", "key_g", "key_h", "key_i", "key_j",
        "key_k", "key_l", "key_m", "key_n", "key_o", "key_p", "key_q", "key_r", "key_s", "key_t",
        "key_u", "key_v", "key_w", "key_x", "key_y", "key_z",
    ];
    const DIGITS: [&str; 10] = [
        "digit_0", "digit_1", "digit_2", "digit_3", "digit_4", "digit_5", "digit_6", "digit_7",
        "digit_8", "digit_9",
    ];
    const FKEYS: [&str; 25] = [
        "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "f13", "f14",
        "f15", "f16", "f17", "f18", "f19", "f20", "f21", "f22", "f23", "f24", "f25",
    ];

    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_lowercase() {
            return Some((LETTERS[(c as u8 - b'a') as usize], Some(c), false));
        }
        // Some platforms report the shifted letter as the key itself.
        if c.is_ascii_uppercase() {
            return Some((
                LETTERS[(c.to_ascii_lowercase() as u8 - b'a') as usize],
                Some(c.to_ascii_lowercase()),
                true,
            ));
        }
        if c.is_ascii_digit() {
            return Some((DIGITS[(c as u8 - b'0') as usize], Some(c), false));
        }
        let (code, base, shifted) = match c {
            '`' => ("backquote", '`', false),
            '~' => ("backquote", '`', true),
            '-' => ("minus", '-', false),
            '_' => ("minus", '-', true),
            '=' => ("equal", '=', false),
            '+' => ("equal", '=', true),
            '[' => ("bracket_left", '[', false),
            '{' => ("bracket_left", '[', true),
            ']' => ("bracket_right", ']', false),
            '}' => ("bracket_right", ']', true),
            '\\' => ("backslash", '\\', false),
            '|' => ("backslash", '\\', true),
            ';' => ("semicolon", ';', false),
            ':' => ("semicolon", ';', true),
            '\'' => ("quote", '\'', false),
            '"' => ("quote", '\'', true),
            ',' => ("comma", ',', false),
            '<' => ("comma", ',', true),
            '.' => ("period", '.', false),
            '>' => ("period", '.', true),
            '/' => ("slash", '/', false),
            '?' => ("slash", '/', true),
            '!' => ("digit_1", '1', true),
            '@' => ("digit_2", '2', true),
            '#' => ("digit_3", '3', true),
            '$' => ("digit_4", '4', true),
            '%' => ("digit_5", '5', true),
            '^' => ("digit_6", '6', true),
            '&' => ("digit_7", '7', true),
            '*' => ("digit_8", '8', true),
            '(' => ("digit_9", '9', true),
            ')' => ("digit_0", '0', true),
            _ => return None,
        };
        return Some((code, Some(base), shifted));
    }

    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<usize>().ok()) {
        return FKEYS
            .get(n.wrapping_sub(1))
            .map(|code| (*code, None, false));
    }
    let code = match key {
        "enter" => "enter",
        "escape" => "escape",
        "backspace" => "backspace",
        "tab" => "tab",
        "space" => return Some(("space", Some(' '), false)),
        "delete" => "delete",
        "insert" => "insert",
        "home" => "home",
        "end" => "end",
        "pageup" => "page_up",
        "pagedown" => "page_down",
        "left" => "arrow_left",
        "right" => "arrow_right",
        "up" => "arrow_up",
        "down" => "arrow_down",
        "shift" => "shift_left",
        "control" => "control_left",
        "alt" => "alt_left",
        "platform" | "super" => "meta_left",
        "menu" => "context_menu",
        _ => return None,
    };
    Some((code, None, false))
}

fn mods(m: Modifiers, shift: bool) -> Mods {
    Mods {
        shift,
        ctrl: m.control,
        alt: m.alt,
        super_key: m.platform,
        caps_lock: false,
        num_lock: false,
    }
}

/// The event for `keystroke`, or `None` for a key libghostty-vt has no name for and that
/// produced no character (a media key, for example).
pub fn key_event(keystroke: &Keystroke, action: KeyAction) -> Option<KeyEvent<'_>> {
    let (code, unshifted, implied_shift) = match physical_key(&keystroke.key) {
        Some(found) => found,
        // A non-ASCII symbol from another layout: send it as text on an unidentified key.
        None if keystroke.key_char.is_some() => ("unidentified", None, false),
        None => return None,
    };
    let m = &keystroke.modifiers;
    let shift = m.shift || implied_shift;
    // Ctrl and super combinations type nothing; the encoder derives them from the key.
    let text = match keystroke.key_char.as_deref() {
        Some(text) if !m.control && !m.platform && !text.chars().any(char::is_control) => {
            Some(text)
        }
        _ if code == "space" && !m.control => Some(" "),
        _ => None,
    };
    let consumed = if shift && text.is_some() {
        Mods {
            shift: true,
            ..Mods::default()
        }
    } else {
        Mods::default()
    };
    Some(KeyEvent {
        key: Key::from_code(code),
        action,
        mods: mods(*m, shift),
        consumed_mods: consumed,
        utf8: text,
        unshifted_codepoint: unshifted,
        composing: false,
    })
}

/// Ctrl+Shift shortcuts that belong to the app, not the shell: new tab, close, maximize,
/// split right, split down, toggle the machine list, zoom, the theme picker and the
/// inspector. The terminal lets them bubble.
pub(crate) const APP_SHORTCUTS: [&str; 9] = ["t", "w", "m", "d", "e", "b", "z", "p", "i"];

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(key: &str, key_char: Option<&str>) -> Keystroke {
        Keystroke {
            key: key.to_owned(),
            key_char: key_char.map(str::to_owned),
            modifiers: Modifiers::default(),
        }
    }

    #[test]
    fn restores_shift_for_symbols() {
        let ks = stroke("!", Some("!"));
        let event = key_event(&ks, KeyAction::Press).unwrap();
        assert!(event.mods.shift);
        assert!(event.consumed_mods.shift);
        assert_eq!(event.utf8, Some("!"));
        assert_eq!(event.unshifted_codepoint, Some('1'));
    }

    #[test]
    fn shifted_letters_keep_their_shift() {
        let ks = stroke("A", Some("A"));
        let event = key_event(&ks, KeyAction::Press).unwrap();
        assert!(event.mods.shift);
        assert_eq!(event.utf8, Some("A"));
        assert_eq!(event.unshifted_codepoint, Some('a'));
    }

    #[test]
    fn ctrl_combinations_carry_no_text() {
        let mut ks = stroke("c", Some("c"));
        ks.modifiers.control = true;
        let event = key_event(&ks, KeyAction::Press).unwrap();
        assert!(event.mods.ctrl);
        assert_eq!(event.utf8, None);
        assert_eq!(event.unshifted_codepoint, Some('c'));
    }
}
