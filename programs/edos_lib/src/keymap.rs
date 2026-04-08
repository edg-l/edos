//! Spanish 105-key (ISO) keyboard layout mapping.
//!
//! Maps raw KeyCode values (from kernel window events) to Unicode characters,
//! taking modifier state into account. The kernel sends only raw keycodes;
//! character decoding is done here in userspace.

/// Modifier state tracked from KeyPress/KeyRelease events.
#[derive(Default)]
pub struct Modifiers {
    pub shift: bool,
    pub altgr: bool,
    pub caps_lock: bool,
    pub ctrl: bool,
}

impl Modifiers {
    /// Returns true if effective shift is active (shift XOR caps_lock for letters).
    fn is_caps(&self) -> bool {
        self.shift ^ self.caps_lock
    }
}

// KeyCode numeric values (pc_keyboard repr(u8) enum order).
// Only the ones we need for the layout mapping.
// Values match pc_keyboard::KeyCode repr(u8) enum order.
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

/// Update modifier state from a key event. Returns true if a modifier was toggled.
pub fn update_modifiers(mods: &mut Modifiers, code: u32, pressed: bool) -> bool {
    match code {
        keycode::LSHIFT | keycode::RSHIFT => {
            mods.shift = pressed;
            true
        }
        keycode::RALTGR => {
            mods.altgr = pressed;
            true
        }
        keycode::LCONTROL | keycode::RCONTROL => {
            mods.ctrl = pressed;
            true
        }
        keycode::CAPS_LOCK if pressed => {
            mods.caps_lock = !mods.caps_lock;
            true
        }
        _ => false,
    }
}

/// Map a keycode + modifier state to a Unicode character (Spanish 105-key layout).
/// Returns None for non-character keys (arrows, function keys, modifiers, etc.).
pub fn map_keycode(code: u32, mods: &Modifiers) -> Option<char> {
    use keycode::*;

    // Ctrl+letter generates control characters 0x01-0x1A.
    if mods.ctrl {
        let ctrl_char = match code {
            A => Some('\x01'),
            B => Some('\x02'),
            C => Some('\x03'),
            D => Some('\x04'),
            E => Some('\x05'),
            F => Some('\x06'),
            G => Some('\x07'),
            H => Some('\x08'),
            I => Some('\x09'),
            J => Some('\x0A'),
            K => Some('\x0B'),
            L => Some('\x0C'),
            M => Some('\x0D'),
            N => Some('\x0E'),
            O => Some('\x0F'),
            P => Some('\x10'),
            Q => Some('\x11'),
            R => Some('\x12'),
            S => Some('\x13'),
            T => Some('\x14'),
            U => Some('\x15'),
            V => Some('\x16'),
            W => Some('\x17'),
            X => Some('\x18'),
            Y => Some('\x19'),
            Z => Some('\x1A'),
            _ => None,
        };
        if ctrl_char.is_some() {
            return ctrl_char;
        }
    }

    match code {
        ESCAPE => Some('\x1B'),
        BACKSPACE => Some('\x08'),
        TAB => Some('\t'),
        RETURN | NUMPAD_ENTER => Some('\n'),
        SPACEBAR => Some(' '),
        DELETE => Some('\x7F'),

        // Number row
        OEM8 => Some(if mods.altgr {
            '\\'
        } else if mods.shift {
            '\u{00AA}'
        } else {
            '\u{00BA}'
        }),
        KEY1 => Some(if mods.altgr {
            '|'
        } else if mods.shift {
            '!'
        } else {
            '1'
        }),
        KEY2 => Some(if mods.altgr {
            '@'
        } else if mods.shift {
            '"'
        } else {
            '2'
        }),
        KEY3 => Some(if mods.altgr {
            '#'
        } else if mods.shift {
            '\u{00B7}'
        } else {
            '3'
        }),
        KEY4 => Some(if mods.altgr {
            '~'
        } else if mods.shift {
            '$'
        } else {
            '4'
        }),
        KEY5 => Some(if mods.shift { '%' } else { '5' }),
        KEY6 => Some(if mods.altgr {
            '\u{00AC}'
        } else if mods.shift {
            '&'
        } else {
            '6'
        }),
        KEY7 => Some(if mods.shift { '/' } else { '7' }),
        KEY8 => Some(if mods.shift { '(' } else { '8' }),
        KEY9 => Some(if mods.shift { ')' } else { '9' }),
        KEY0 => Some(if mods.shift { '=' } else { '0' }),
        OEM_MINUS => Some(if mods.shift { '?' } else { '\'' }),
        OEM_PLUS => Some(if mods.shift { '\u{00BF}' } else { '\u{00A1}' }),

        // QWERTY row
        Q => Some(if mods.is_caps() { 'Q' } else { 'q' }),
        W => Some(if mods.is_caps() { 'W' } else { 'w' }),
        E => Some(if mods.altgr {
            '\u{20AC}'
        } else if mods.is_caps() {
            'E'
        } else {
            'e'
        }),
        R => Some(if mods.is_caps() { 'R' } else { 'r' }),
        T => Some(if mods.is_caps() { 'T' } else { 't' }),
        Y => Some(if mods.is_caps() { 'Y' } else { 'y' }),
        U => Some(if mods.is_caps() { 'U' } else { 'u' }),
        I => Some(if mods.is_caps() { 'I' } else { 'i' }),
        O => Some(if mods.is_caps() { 'O' } else { 'o' }),
        P => Some(if mods.is_caps() { 'P' } else { 'p' }),
        OEM4 => Some(if mods.altgr {
            '['
        } else if mods.shift {
            '^'
        } else {
            '`'
        }),
        OEM6 => Some(if mods.altgr {
            ']'
        } else if mods.shift {
            '*'
        } else {
            '+'
        }),

        // Home row
        A => Some(if mods.is_caps() { 'A' } else { 'a' }),
        S => Some(if mods.is_caps() { 'S' } else { 's' }),
        D => Some(if mods.is_caps() { 'D' } else { 'd' }),
        F => Some(if mods.is_caps() { 'F' } else { 'f' }),
        G => Some(if mods.is_caps() { 'G' } else { 'g' }),
        H => Some(if mods.is_caps() { 'H' } else { 'h' }),
        J => Some(if mods.is_caps() { 'J' } else { 'j' }),
        K => Some(if mods.is_caps() { 'K' } else { 'k' }),
        L => Some(if mods.is_caps() { 'L' } else { 'l' }),
        OEM1 => Some(if mods.is_caps() {
            '\u{00D1}'
        } else {
            '\u{00F1}'
        }),
        OEM3 => Some(if mods.altgr {
            '{'
        } else if mods.shift {
            '\u{00A8}'
        } else {
            '\u{00B4}'
        }),
        OEM7 => Some(if mods.altgr {
            '}'
        } else if mods.is_caps() {
            '\u{00C7}'
        } else {
            '\u{00E7}'
        }),

        // Bottom row
        OEM5 => Some(if mods.altgr {
            '\\'
        } else if mods.shift {
            '>'
        } else {
            '<'
        }),
        Z => Some(if mods.is_caps() { 'Z' } else { 'z' }),
        X => Some(if mods.is_caps() { 'X' } else { 'x' }),
        C => Some(if mods.is_caps() { 'C' } else { 'c' }),
        V => Some(if mods.is_caps() { 'V' } else { 'v' }),
        B => Some(if mods.is_caps() { 'B' } else { 'b' }),
        N => Some(if mods.is_caps() { 'N' } else { 'n' }),
        M => Some(if mods.is_caps() { 'M' } else { 'm' }),
        OEM_COMMA => Some(if mods.shift { ';' } else { ',' }),
        OEM_PERIOD => Some(if mods.shift { ':' } else { '.' }),
        OEM2 => Some(if mods.shift { '_' } else { '-' }),

        // Numpad
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
