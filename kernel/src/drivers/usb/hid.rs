use pc_keyboard::{KeyCode, KeyEvent, KeyState};

use crate::drivers::mouse::{MOUSE_BROADCAST, MouseEvent, apply_relative_move};

/// Map USB HID keyboard usage code (page 0x07) to pc_keyboard::KeyCode.
/// Returns None for unmapped/reserved codes.
pub fn usb_hid_to_keycode(usage: u8) -> Option<KeyCode> {
    match usage {
        0x04 => Some(KeyCode::A),
        0x05 => Some(KeyCode::B),
        0x06 => Some(KeyCode::C),
        0x07 => Some(KeyCode::D),
        0x08 => Some(KeyCode::E),
        0x09 => Some(KeyCode::F),
        0x0A => Some(KeyCode::G),
        0x0B => Some(KeyCode::H),
        0x0C => Some(KeyCode::I),
        0x0D => Some(KeyCode::J),
        0x0E => Some(KeyCode::K),
        0x0F => Some(KeyCode::L),
        0x10 => Some(KeyCode::M),
        0x11 => Some(KeyCode::N),
        0x12 => Some(KeyCode::O),
        0x13 => Some(KeyCode::P),
        0x14 => Some(KeyCode::Q),
        0x15 => Some(KeyCode::R),
        0x16 => Some(KeyCode::S),
        0x17 => Some(KeyCode::T),
        0x18 => Some(KeyCode::U),
        0x19 => Some(KeyCode::V),
        0x1A => Some(KeyCode::W),
        0x1B => Some(KeyCode::X),
        0x1C => Some(KeyCode::Y),
        0x1D => Some(KeyCode::Z),
        0x1E => Some(KeyCode::Key1),
        0x1F => Some(KeyCode::Key2),
        0x20 => Some(KeyCode::Key3),
        0x21 => Some(KeyCode::Key4),
        0x22 => Some(KeyCode::Key5),
        0x23 => Some(KeyCode::Key6),
        0x24 => Some(KeyCode::Key7),
        0x25 => Some(KeyCode::Key8),
        0x26 => Some(KeyCode::Key9),
        0x27 => Some(KeyCode::Key0),
        0x28 => Some(KeyCode::Return),
        0x29 => Some(KeyCode::Escape),
        0x2A => Some(KeyCode::Backspace),
        0x2B => Some(KeyCode::Tab),
        0x2C => Some(KeyCode::Spacebar),
        0x2D => Some(KeyCode::OemMinus),
        0x2E => Some(KeyCode::OemPlus),
        0x2F => Some(KeyCode::Oem4),
        0x30 => Some(KeyCode::Oem6),
        0x31 => Some(KeyCode::Oem5),
        0x33 => Some(KeyCode::Oem1),
        0x34 => Some(KeyCode::Oem7),
        0x35 => Some(KeyCode::Oem3),
        0x36 => Some(KeyCode::OemComma),
        0x37 => Some(KeyCode::OemPeriod),
        0x38 => Some(KeyCode::Oem2),
        0x39 => Some(KeyCode::CapsLock),
        0x3A => Some(KeyCode::F1),
        0x3B => Some(KeyCode::F2),
        0x3C => Some(KeyCode::F3),
        0x3D => Some(KeyCode::F4),
        0x3E => Some(KeyCode::F5),
        0x3F => Some(KeyCode::F6),
        0x40 => Some(KeyCode::F7),
        0x41 => Some(KeyCode::F8),
        0x42 => Some(KeyCode::F9),
        0x43 => Some(KeyCode::F10),
        0x44 => Some(KeyCode::F11),
        0x45 => Some(KeyCode::F12),
        0x46 => Some(KeyCode::PrintScreen),
        0x47 => Some(KeyCode::ScrollLock),
        0x49 => Some(KeyCode::Insert),
        0x4A => Some(KeyCode::Home),
        0x4B => Some(KeyCode::PageUp),
        0x4C => Some(KeyCode::Delete),
        0x4D => Some(KeyCode::End),
        0x4E => Some(KeyCode::PageDown),
        0x4F => Some(KeyCode::ArrowRight),
        0x50 => Some(KeyCode::ArrowLeft),
        0x51 => Some(KeyCode::ArrowDown),
        0x52 => Some(KeyCode::ArrowUp),
        0x53 => Some(KeyCode::NumpadLock),
        0x54 => Some(KeyCode::NumpadDivide),
        0x55 => Some(KeyCode::NumpadMultiply),
        0x56 => Some(KeyCode::NumpadSubtract),
        0x57 => Some(KeyCode::NumpadAdd),
        0x58 => Some(KeyCode::NumpadEnter),
        0x59 => Some(KeyCode::Numpad1),
        0x5A => Some(KeyCode::Numpad2),
        0x5B => Some(KeyCode::Numpad3),
        0x5C => Some(KeyCode::Numpad4),
        0x5D => Some(KeyCode::Numpad5),
        0x5E => Some(KeyCode::Numpad6),
        0x5F => Some(KeyCode::Numpad7),
        0x60 => Some(KeyCode::Numpad8),
        0x61 => Some(KeyCode::Numpad9),
        0x62 => Some(KeyCode::Numpad0),
        0x63 => Some(KeyCode::NumpadPeriod),
        0xE0 => Some(KeyCode::LControl),
        0xE1 => Some(KeyCode::LShift),
        0xE2 => Some(KeyCode::LAlt),
        0xE3 => Some(KeyCode::LWin),
        0xE4 => Some(KeyCode::RControl),
        0xE5 => Some(KeyCode::RShift),
        0xE6 => Some(KeyCode::RAltGr),
        0xE7 => Some(KeyCode::RWin),
        _ => None,
    }
}

/// Fixed-capacity collection of keyboard events from one report comparison.
///
/// Maximum events per report: 8 modifiers + 6 keys = 14 key events.
pub struct KeyEvents {
    events: [KeyEvent; 14],
    count: usize,
}

impl KeyEvents {
    fn new() -> Self {
        Self {
            // KeyEvent does not implement Copy or Default, so initialise with from_fn.
            events: core::array::from_fn(|_| KeyEvent::new(KeyCode::A, KeyState::Up)),
            count: 0,
        }
    }

    fn push(&mut self, event: KeyEvent) {
        if self.count < self.events.len() {
            self.events[self.count] = event;
            self.count += 1;
        }
    }

    /// Returns the populated slice of key events.
    pub fn as_slice(&self) -> &[KeyEvent] {
        &self.events[..self.count]
    }
}

/// Process an 8-byte USB HID boot keyboard report.
///
/// Compares with previous report to generate key down/up events.
///
/// Boot report format:
///   byte 0: modifier keys (bit 0=LCtrl, 1=LShift, 2=LAlt, 3=LGui, 4=RCtrl, 5=RShift, 6=RAlt, 7=RGui)
///   byte 1: reserved
///   bytes 2-7: up to 6 simultaneous key usage codes (0 = no key)
pub fn process_boot_keyboard_report(prev: &[u8; 8], current: &[u8; 8]) -> KeyEvents {
    let mut events = KeyEvents::new();

    // Modifier key bit-to-usage-code mapping
    let modifier_map: [(u8, u8); 8] = [
        (0, 0xE0), // LCtrl
        (1, 0xE1), // LShift
        (2, 0xE2), // LAlt
        (3, 0xE3), // LGui
        (4, 0xE4), // RCtrl
        (5, 0xE5), // RShift
        (6, 0xE6), // RAlt
        (7, 0xE7), // RGui
    ];

    for &(bit, usage) in &modifier_map {
        let was_pressed = prev[0] & (1 << bit) != 0;
        let is_pressed = current[0] & (1 << bit) != 0;

        if is_pressed && !was_pressed {
            if let Some(code) = usb_hid_to_keycode(usage) {
                events.push(KeyEvent::new(code, KeyState::Down));
            }
        } else if !is_pressed && was_pressed {
            if let Some(code) = usb_hid_to_keycode(usage) {
                events.push(KeyEvent::new(code, KeyState::Up));
            }
        }
    }

    // Keys in current but not in prev = newly pressed
    for &key in &current[2..8] {
        if key == 0 {
            continue;
        }
        if !prev[2..8].contains(&key) {
            if let Some(code) = usb_hid_to_keycode(key) {
                events.push(KeyEvent::new(code, KeyState::Down));
            }
        }
    }

    // Keys in prev but not in current = released
    for &key in &prev[2..8] {
        if key == 0 {
            continue;
        }
        if !current[2..8].contains(&key) {
            if let Some(code) = usb_hid_to_keycode(key) {
                events.push(KeyEvent::new(code, KeyState::Up));
            }
        }
    }

    events
}

/// Process a USB HID boot mouse report (3-4 bytes) and broadcast a `MouseEvent`.
///
/// Boot mouse report format:
///   byte 0: button state (bit 0=left, bit 1=right, bit 2=middle)
///   byte 1: X displacement (signed, relative)
///   byte 2: Y displacement (signed, relative)
///   byte 3: scroll wheel delta (signed, optional — present when report_len >= 4)
///
/// Returns `Some(MouseEvent)` if the report was valid and an event was broadcast,
/// `None` if the report is too short to be useful.
pub fn process_boot_mouse_report(report: &[u8], report_len: usize) -> Option<MouseEvent> {
    if report_len < 3 {
        return None;
    }

    let buttons = report[0] & 0x07;
    let dx = report[1] as i8 as i16;
    let dy = report[2] as i8 as i16;
    let scroll = if report_len >= 4 { report[3] as i8 } else { 0 };

    let event = apply_relative_move(dx, dy, buttons, scroll);
    MOUSE_BROADCAST.broadcast(event);
    Some(event)
}
