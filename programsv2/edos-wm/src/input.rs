//! Keyboard and mouse input handling for the window manager.

use std::io::Read;

use edos_render::window::{WindowListEntry, flags::FLAG_DOCK, read_mouse_state};

/// Raw scancode for Left Alt key (extended HID-style code from keyboard driver).
pub const RAW_LALT: u32 = 0x8000_005F;

/// Raw scancode for F4 key (extended HID-style code from keyboard driver).
pub const RAW_F4: u32 = 0x8000_0004;

/// Raw key code for Tab (ASCII-range code from keyboard driver).
pub const RAW_TAB: u32 = 0x09;

/// Size of the keyboard read buffer in bytes (4 bytes per key event).
const KBD_BUF_SIZE: usize = 64;

/// Actions produced by keyboard input processing.
pub enum InputAction {
    /// No actionable keyboard input this frame.
    None,
    /// Alt+F4 pressed while the given window was focused.
    AltF4 { focused_id: u64 },
    /// Alt+Tab cycled focus to the given window.
    AltTab { next_id: u64 },
}

/// Manages input device state (mouse + keyboard).
pub struct InputState {
    mouse_file: std::fs::File,
    kbd_file: Option<std::fs::File>,
    alt_held: bool,
    last_mouse_buttons: u8,
}

impl InputState {
    /// Open input devices. Panics if /dev/mouse is unavailable.
    pub fn new() -> Self {
        Self {
            mouse_file: std::fs::File::open("/dev/mouse").expect("failed to open /dev/mouse"),
            kbd_file: std::fs::File::open("/dev/kbd").ok(),
            alt_held: false,
            last_mouse_buttons: 0,
        }
    }

    /// Read current mouse position and button state.
    /// Returns (x, y, buttons), defaulting to (0, 0, 0) on error.
    pub fn read_mouse(&mut self) -> (i32, i32, u8) {
        read_mouse_state(&mut self.mouse_file).unwrap_or((0, 0, 0))
    }

    /// Detect a left button press (0->1 transition). Updates internal state.
    pub fn detect_left_press(&mut self, buttons: u8) -> bool {
        let pressed = (buttons & 0x01) != 0 && (self.last_mouse_buttons & 0x01) == 0;
        self.last_mouse_buttons = buttons;
        pressed
    }

    /// Check if left button is currently held.
    pub fn left_held(buttons: u8) -> bool {
        (buttons & 0x01) != 0
    }

    /// Read buffered keyboard events and return an action if a WM shortcut was triggered.
    ///
    /// `focused_window_id` is needed for Alt+F4.
    /// `windows` is the current window list slice for Alt+Tab cycling.
    pub fn read_keyboard(
        &mut self,
        focused_window_id: Option<u64>,
        windows: &[WindowListEntry],
    ) -> InputAction {
        let kbd = match self.kbd_file {
            Some(ref mut f) => f,
            None => return InputAction::None,
        };

        let mut kbd_buf = [0u8; KBD_BUF_SIZE];
        let n = match kbd.read(&mut kbd_buf) {
            Ok(n) => n,
            Err(_) => return InputAction::None,
        };

        let mut i = 0;
        while i + 4 <= n {
            let key =
                u32::from_le_bytes([kbd_buf[i], kbd_buf[i + 1], kbd_buf[i + 2], kbd_buf[i + 3]]);
            i += 4;

            if key == RAW_LALT {
                self.alt_held = true;
                continue;
            }

            if self.alt_held {
                if key == RAW_F4 {
                    self.alt_held = false;
                    if let Some(fid) = focused_window_id {
                        return InputAction::AltF4 { focused_id: fid };
                    }
                    continue;
                }

                if key == RAW_TAB {
                    self.alt_held = false;
                    // Cycle focus to next visible non-dock window
                    let visible: Vec<u64> = windows
                        .iter()
                        .filter(|w| w.visible != 0 && (w.flags & FLAG_DOCK) == 0)
                        .map(|w| w.id)
                        .collect();

                    if !visible.is_empty() {
                        let current_idx = focused_window_id
                            .and_then(|fid| visible.iter().position(|&id| id == fid))
                            .unwrap_or(0);
                        let next_idx = (current_idx + 1) % visible.len();
                        return InputAction::AltTab {
                            next_id: visible[next_idx],
                        };
                    }
                    continue;
                }

                // Unrecognized key while Alt held: clear Alt state
                self.alt_held = false;
            }
        }

        InputAction::None
    }
}
