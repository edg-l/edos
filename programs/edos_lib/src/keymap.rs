//! Keyboard layouts: raw keycode to Unicode character.
//!
//! The kernel sends only raw keycodes, so character decoding happens here.
//! Which layout does it is a runtime choice rather than a compile-time one:
//! `current_layout` resolves it once per process from the `keymap=` boot
//! parameter or `/etc/keymap`, so a machine whose keyboard is not the one the
//! image was built on still types what is printed on its keys.
//!
//! A layout is a table of physical keys. Keycodes are `pc_keyboard::KeyCode`
//! values and name *positions*, not characters: `Oem5` is the key next to the
//! left shift that only ISO boards have, and `Oem7` is the one left of Enter,
//! which is why the same constant is a backslash on US and a c-cedilla on
//! Spanish.

use std::sync::{
    Once,
    atomic::{AtomicUsize, Ordering},
};

use crate::{config, procinfo::boot_param};

/// Where the layout is recorded across boots.
pub const CONFIG_PATH: &str = config::KEYMAP;

/// Modifier state tracked from KeyPress/KeyRelease events.
#[derive(Default)]
pub struct Modifiers {
    pub shift: bool,
    pub altgr: bool,
    pub caps_lock: bool,
    pub ctrl: bool,
}

// KeyCode numeric values (pc_keyboard repr(u8) enum order).
// Only the ones we need for the layout mapping.
pub mod keycode {
    pub const ESCAPE: u32 = 0;
    pub const F4: u32 = 4;
    pub const OEM8: u32 = 17;
    pub const KEY1: u32 = 18;
    pub const KEY2: u32 = 19;
    pub const KEY3: u32 = 20;
    pub const KEY4: u32 = 21;
    pub const KEY5: u32 = 22;
    pub const KEY6: u32 = 23;
    pub const KEY7: u32 = 24;
    pub const KEY8: u32 = 25;
    pub const KEY9: u32 = 26;
    pub const KEY0: u32 = 27;
    pub const OEM_MINUS: u32 = 28;
    pub const OEM_PLUS: u32 = 29;
    pub const BACKSPACE: u32 = 30;
    pub const HOME: u32 = 32;
    pub const PAGE_UP: u32 = 33;
    pub const NUMPAD_DIVIDE: u32 = 35;
    pub const NUMPAD_MULTIPLY: u32 = 36;
    pub const NUMPAD_SUBTRACT: u32 = 37;
    pub const TAB: u32 = 38;
    pub const Q: u32 = 39;
    pub const W: u32 = 40;
    pub const E: u32 = 41;
    pub const R: u32 = 42;
    pub const T: u32 = 43;
    pub const Y: u32 = 44;
    pub const U: u32 = 45;
    pub const I: u32 = 46;
    pub const O: u32 = 47;
    pub const P: u32 = 48;
    pub const OEM4: u32 = 49;
    pub const OEM6: u32 = 50;
    pub const OEM5: u32 = 51;
    pub const OEM7: u32 = 52;
    pub const DELETE: u32 = 53;
    pub const END: u32 = 54;
    pub const PAGE_DOWN: u32 = 55;
    pub const NUMPAD7: u32 = 56;
    pub const NUMPAD8: u32 = 57;
    pub const NUMPAD9: u32 = 58;
    pub const NUMPAD_ADD: u32 = 59;
    pub const CAPS_LOCK: u32 = 60;
    pub const A: u32 = 61;
    pub const S: u32 = 62;
    pub const D: u32 = 63;
    pub const F: u32 = 64;
    pub const G: u32 = 65;
    pub const H: u32 = 66;
    pub const J: u32 = 67;
    pub const K: u32 = 68;
    pub const L: u32 = 69;
    pub const OEM1: u32 = 70;
    pub const OEM3: u32 = 71;
    pub const RETURN: u32 = 72;
    pub const NUMPAD4: u32 = 73;
    pub const NUMPAD5: u32 = 74;
    pub const NUMPAD6: u32 = 75;
    pub const LSHIFT: u32 = 76;
    pub const Z: u32 = 77;
    pub const X: u32 = 78;
    pub const C: u32 = 79;
    pub const V: u32 = 80;
    pub const B: u32 = 81;
    pub const N: u32 = 82;
    pub const M: u32 = 83;
    pub const OEM_COMMA: u32 = 84;
    pub const OEM_PERIOD: u32 = 85;
    pub const OEM2: u32 = 86;
    pub const RSHIFT: u32 = 87;
    pub const ARROW_UP: u32 = 88;
    pub const NUMPAD1: u32 = 89;
    pub const NUMPAD2: u32 = 90;
    pub const NUMPAD3: u32 = 91;
    pub const NUMPAD_ENTER: u32 = 92;
    pub const LCONTROL: u32 = 93;
    pub const LALT: u32 = 95;
    pub const SPACEBAR: u32 = 96;
    pub const RALTGR: u32 = 97;
    pub const RCONTROL: u32 = 100;
    pub const ARROW_LEFT: u32 = 101;
    pub const ARROW_DOWN: u32 = 102;
    pub const ARROW_RIGHT: u32 = 103;
    pub const NUMPAD0: u32 = 104;
    pub const NUMPAD_PERIOD: u32 = 105;
}

use keycode::*;

/// One physical key: what it produces unshifted, shifted, and with AltGr.
pub struct Key {
    code: u32,
    base: char,
    shift: char,
    /// NUL when the key has no AltGr level.
    altgr: char,
    /// True when Caps Lock selects the shifted level, i.e. this key is a letter.
    caps: bool,
}

/// A key with a shifted level and no AltGr.
const fn k(code: u32, base: char, shift: char) -> Key {
    Key { code, base, shift, altgr: '\0', caps: false }
}

/// A key with a shifted level and an AltGr level.
const fn ka(code: u32, base: char, shift: char, altgr: char) -> Key {
    Key { code, base, shift, altgr, caps: false }
}

/// A letter, so Caps Lock applies as well as Shift.
const fn l(code: u32, lower: char, upper: char) -> Key {
    Key { code, base: lower, shift: upper, altgr: '\0', caps: true }
}

/// A letter carrying an AltGr level.
const fn la(code: u32, lower: char, upper: char, altgr: char) -> Key {
    Key { code, base: lower, shift: upper, altgr, caps: true }
}

/// The 26 letters, identical on every QWERTY layout. A layout that moves one
/// of them, or hangs an AltGr level on it, overrides it in its own table,
/// which is searched first.
const LETTERS: &[Key] = &[
    l(Q, 'q', 'Q'),
    l(W, 'w', 'W'),
    l(E, 'e', 'E'),
    l(R, 'r', 'R'),
    l(T, 't', 'T'),
    l(Y, 'y', 'Y'),
    l(U, 'u', 'U'),
    l(I, 'i', 'I'),
    l(O, 'o', 'O'),
    l(P, 'p', 'P'),
    l(A, 'a', 'A'),
    l(S, 's', 'S'),
    l(D, 'd', 'D'),
    l(F, 'f', 'F'),
    l(G, 'g', 'G'),
    l(H, 'h', 'H'),
    l(J, 'j', 'J'),
    l(K, 'k', 'K'),
    l(L, 'l', 'L'),
    l(Z, 'z', 'Z'),
    l(X, 'x', 'X'),
    l(C, 'c', 'C'),
    l(V, 'v', 'V'),
    l(B, 'b', 'B'),
    l(N, 'n', 'N'),
    l(M, 'm', 'M'),
];

/// US English, ANSI 104-key. `OEM5` is absent from the board entirely, which is
/// why it carries nothing here.
const US_KEYS: &[Key] = &[
    k(OEM8, '`', '~'),
    k(KEY1, '1', '!'),
    k(KEY2, '2', '@'),
    k(KEY3, '3', '#'),
    k(KEY4, '4', '$'),
    k(KEY5, '5', '%'),
    k(KEY6, '6', '^'),
    k(KEY7, '7', '&'),
    k(KEY8, '8', '*'),
    k(KEY9, '9', '('),
    k(KEY0, '0', ')'),
    k(OEM_MINUS, '-', '_'),
    k(OEM_PLUS, '=', '+'),
    k(OEM4, '[', '{'),
    k(OEM6, ']', '}'),
    k(OEM7, '\\', '|'),
    k(OEM1, ';', ':'),
    k(OEM3, '\'', '"'),
    k(OEM_COMMA, ',', '<'),
    k(OEM_PERIOD, '.', '>'),
    k(OEM2, '/', '?'),
];

/// United Kingdom, ISO 105-key. Differs from US on five keys: the number row
/// carries a pound sign, the quote key carries the at sign, and the two keys
/// US has nowhere near each other (`Oem5` next to the left shift, `Oem7` left
/// of Enter) are the backslash and the hash.
const UK_KEYS: &[Key] = &[
    ka(OEM8, '`', '\u{00AC}', '\u{00A6}'),
    k(KEY1, '1', '!'),
    k(KEY2, '2', '"'),
    k(KEY3, '3', '\u{00A3}'),
    ka(KEY4, '4', '$', '\u{20AC}'),
    k(KEY5, '5', '%'),
    k(KEY6, '6', '^'),
    k(KEY7, '7', '&'),
    k(KEY8, '8', '*'),
    k(KEY9, '9', '('),
    k(KEY0, '0', ')'),
    k(OEM_MINUS, '-', '_'),
    k(OEM_PLUS, '=', '+'),
    k(OEM4, '[', '{'),
    k(OEM6, ']', '}'),
    k(OEM5, '\\', '|'),
    k(OEM7, '#', '~'),
    k(OEM1, ';', ':'),
    k(OEM3, '\'', '@'),
    k(OEM_COMMA, ',', '<'),
    k(OEM_PERIOD, '.', '>'),
    k(OEM2, '/', '?'),
];

/// Spanish, ISO 105-key. Dead keys are not implemented, so the two accent keys
/// produce the accent itself rather than composing with the letter after it.
const ES_KEYS: &[Key] = &[
    ka(OEM8, '\u{00BA}', '\u{00AA}', '\\'),
    ka(KEY1, '1', '!', '|'),
    ka(KEY2, '2', '"', '@'),
    ka(KEY3, '3', '\u{00B7}', '#'),
    ka(KEY4, '4', '$', '~'),
    k(KEY5, '5', '%'),
    ka(KEY6, '6', '&', '\u{00AC}'),
    k(KEY7, '7', '/'),
    k(KEY8, '8', '('),
    k(KEY9, '9', ')'),
    k(KEY0, '0', '='),
    k(OEM_MINUS, '\'', '?'),
    k(OEM_PLUS, '\u{00A1}', '\u{00BF}'),
    ka(OEM4, '`', '^', '['),
    ka(OEM6, '+', '*', ']'),
    ka(OEM5, '<', '>', '\\'),
    la(OEM7, '\u{00E7}', '\u{00C7}', '}'),
    l(OEM1, '\u{00F1}', '\u{00D1}'),
    ka(OEM3, '\u{00B4}', '\u{00A8}', '{'),
    k(OEM_COMMA, ',', ';'),
    k(OEM_PERIOD, '.', ':'),
    k(OEM2, '-', '_'),
    la(E, 'e', 'E', '\u{20AC}'),
];

/// A named layout.
pub struct Layout {
    pub name: &'static str,
    pub description: &'static str,
    keys: &'static [Key],
}

impl Layout {
    /// The key at `code`, the layout's own table winning over the shared letters.
    ///
    /// Linear, because a layout is a few dozen keys and this runs once per
    /// keystroke; a lookup table indexed by keycode would be faster and would
    /// have to be kept dense by hand.
    fn key(&self, code: u32) -> Option<&'static Key> {
        self.keys
            .iter()
            .chain(LETTERS.iter())
            .find(|key| key.code == code)
    }
}

static US: Layout = Layout {
    name: "us",
    description: "US English (ANSI 104-key)",
    keys: US_KEYS,
};
static UK: Layout = Layout {
    name: "uk",
    description: "United Kingdom (ISO 105-key)",
    keys: UK_KEYS,
};
static ES: Layout = Layout {
    name: "es",
    description: "Spanish (ISO 105-key)",
    keys: ES_KEYS,
};

/// Every layout the system knows, in the order `keymap` lists them. The first
/// is the default, chosen because an image handed to somebody else is far more
/// likely to meet a US board than any other.
pub static LAYOUTS: &[&Layout] = &[&US, &UK, &ES];

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static RESOLVED: Once = Once::new();

/// The layout named `name`, or None if there is no such layout.
pub fn layout_by_name(name: &str) -> Option<&'static Layout> {
    LAYOUTS.iter().copied().find(|layout| layout.name == name)
}

/// Make `name` the layout this process decodes with. Returns false, changing
/// nothing, if there is no such layout.
///
/// This does not write [`CONFIG_PATH`]; `keymap` is what makes a choice
/// persist, and a program that decodes keys only ever reads.
pub fn set_layout(name: &str) -> bool {
    let Some(index) = LAYOUTS.iter().position(|layout| layout.name == name) else {
        return false;
    };
    RESOLVED.call_once(|| {});
    CURRENT.store(index, Ordering::Relaxed);
    true
}

/// The layout in force, resolving it on first use.
pub fn current_layout() -> &'static Layout {
    RESOLVED.call_once(resolve);
    LAYOUTS[CURRENT.load(Ordering::Relaxed)]
}

/// Resolve the layout from the boot parameter, then the config file, then the
/// built-in default.
///
/// The boot parameter wins so that a machine whose `/etc` cannot be edited,
/// because the layout makes the editor unusable, can still be recovered.
fn resolve() {
    if let Some(name) = boot_param("keymap")
        && let Some(index) = LAYOUTS.iter().position(|layout| layout.name == name)
    {
        CURRENT.store(index, Ordering::Relaxed);
        return;
    }
    if let Some(name) = configured_layout()
        && let Some(index) = LAYOUTS.iter().position(|layout| layout.name == name)
    {
        CURRENT.store(index, Ordering::Relaxed);
    }
}

/// The layout name recorded in [`CONFIG_PATH`], if the file has one.
pub fn configured_layout() -> Option<String> {
    config::read(CONFIG_PATH)
}

/// Update modifier state from a key event. Returns true if a modifier was toggled.
pub fn update_modifiers(mods: &mut Modifiers, code: u32, pressed: bool) -> bool {
    match code {
        LSHIFT | RSHIFT => {
            mods.shift = pressed;
            true
        }
        RALTGR => {
            mods.altgr = pressed;
            true
        }
        LCONTROL | RCONTROL => {
            mods.ctrl = pressed;
            true
        }
        CAPS_LOCK if pressed => {
            mods.caps_lock = !mods.caps_lock;
            true
        }
        _ => false,
    }
}

/// Map a keycode and modifier state to a character using the layout in force.
/// Returns None for keys that produce no character (arrows, function keys,
/// modifiers), and for a key the layout does not carry at all.
pub fn map_keycode(code: u32, mods: &Modifiers) -> Option<char> {
    map_keycode_in(current_layout(), code, mods)
}

/// Map a keycode and modifier state to a character using a named layout.
pub fn map_keycode_in(layout: &Layout, code: u32, mods: &Modifiers) -> Option<char> {
    if let Some(key) = layout.key(code) {
        // Ctrl+letter is the control character, taken from what the key
        // produces on this layout rather than from its position, so a layout
        // that moves a letter moves its control code with it.
        if mods.ctrl && key.base.is_ascii_alphabetic() {
            let ordinal = key.base.to_ascii_lowercase() as u8 - b'a';
            return Some((ordinal + 1) as char);
        }
        if mods.altgr && key.altgr != '\0' {
            return Some(key.altgr);
        }
        let shifted = if key.caps {
            mods.shift ^ mods.caps_lock
        } else {
            mods.shift
        };
        return Some(if shifted { key.shift } else { key.base });
    }

    // Keys that carry the same character whatever the layout.
    match code {
        ESCAPE => Some('\x1B'),
        BACKSPACE => Some('\x08'),
        TAB => Some('\t'),
        RETURN | NUMPAD_ENTER => Some('\n'),
        SPACEBAR => Some(' '),
        DELETE => Some('\x7F'),
        NUMPAD_DIVIDE => Some('/'),
        NUMPAD_MULTIPLY => Some('*'),
        NUMPAD_SUBTRACT => Some('-'),
        NUMPAD_ADD => Some('+'),
        NUMPAD0 => Some('0'),
        NUMPAD1 => Some('1'),
        NUMPAD2 => Some('2'),
        NUMPAD3 => Some('3'),
        NUMPAD4 => Some('4'),
        NUMPAD5 => Some('5'),
        NUMPAD6 => Some('6'),
        NUMPAD7 => Some('7'),
        NUMPAD8 => Some('8'),
        NUMPAD9 => Some('9'),
        NUMPAD_PERIOD => Some('.'),
        _ => None,
    }
}
