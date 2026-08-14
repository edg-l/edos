//! Keyboard and mouse input handling for the window manager.

use std::io::Read;

use edos_lib::io::klog_dump;
use edos_render::window::{
    GRAB_MOD_ALT, GRAB_MOD_CTRL, GRAB_MOD_SHIFT, WindowListEntry, flags::FLAG_DOCK,
    read_mouse_state, window_grab_key,
};

/// KeyCode for Left Alt (pc_keyboard::KeyCode::LAlt = 95).
pub const RAW_LALT: u32 = 95;

/// KeyCode for F4 (pc_keyboard::KeyCode::F4 = 4).
pub const RAW_F4: u32 = 4;

/// KeyCode for Tab (pc_keyboard::KeyCode::Tab = 38).
pub const RAW_TAB: u32 = 38;

/// KeyCode for Left Control (pc_keyboard::KeyCode::LControl = 93).
pub const RAW_LCTRL: u32 = 93;

/// KeyCode for Right Control (pc_keyboard::KeyCode::RControl = 100).
pub const RAW_RCTRL: u32 = 100;

/// KeyCode for Left Shift (pc_keyboard::KeyCode::LShift = 76).
pub const RAW_LSHIFT: u32 = 76;

/// KeyCode for Right Shift (pc_keyboard::KeyCode::RShift = 87).
pub const RAW_RSHIFT: u32 = 87;

/// KeyCode for W (pc_keyboard::KeyCode::W = 40).
pub const RAW_W: u32 = 40;

/// Bit 31 set in /dev/kbd encoding means key release.
const KEY_RELEASE_BIT: u32 = 0x8000_0000;

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
    /// Ctrl+Alt+W: copy the window registry into the kernel log.
    DumpWindows,
}

/// Ask the kernel to withhold the window manager's own chords from whichever
/// window has focus. Without this both the focused window and this process see
/// the same key, and the window has no way to tell the chord was consumed.
fn claim_shortcuts() {
    let mut claimed = String::new();
    for (code, mods, name) in CLAIMED_CHORDS {
        match window_grab_key(code, mods) {
            Ok(()) => {
                if !claimed.is_empty() {
                    claimed.push_str(", ");
                }
                claimed.push_str(name);
            }
            Err(_) => {
                klog_dump("edos-wm", [format!("key grab refused for {name}")].iter());
                return;
            }
        }
    }
    klog_dump("edos-wm", [format!("key grabs: {claimed}")].iter());
}

/// Manages input device state (mouse + keyboard).
pub struct InputState {
    mouse_file: std::fs::File,
    kbd_file: Option<std::fs::File>,
    /// Modifiers held, as [`GRAB_MOD_ALT`] and friends, so the mask compared
    /// here is the same one the kernel matches a grab against.
    mods: u32,
    last_mouse_buttons: u8,
}

/// The chords the window manager acts on, and therefore the ones the focused
/// window must not also receive.
///
/// The mask is matched exactly, the way the kernel matches a grab. A subset
/// match here would act on a chord the kernel did not withhold — Shift+Alt+Tab
/// would cycle windows *and* send the focused program a Tab, which is the
/// defect the grab exists to remove.
const CLAIMED_CHORDS: [(u32, u32, &str); 3] = [
    (RAW_F4, GRAB_MOD_ALT, "Alt+F4"),
    (RAW_TAB, GRAB_MOD_ALT, "Alt+Tab"),
    (RAW_W, GRAB_MOD_CTRL | GRAB_MOD_ALT, "Ctrl+Alt+W"),
];

/// The modifier bit `key` stands for, if it is a modifier at all. Mirrors the
/// kernel's own table in `window/grab.rs`: AltGr is not Alt, since it selects a
/// character rather than qualifying one.
fn modifier_bit(key: u32) -> Option<u32> {
    match key {
        RAW_LSHIFT | RAW_RSHIFT => Some(GRAB_MOD_SHIFT),
        RAW_LCTRL | RAW_RCTRL => Some(GRAB_MOD_CTRL),
        RAW_LALT => Some(GRAB_MOD_ALT),
        _ => None,
    }
}

impl InputState {
    /// Open input devices. Panics if /dev/mouse is unavailable.
    pub fn new() -> Self {
        claim_shortcuts();
        Self {
            mouse_file: std::fs::File::open("/dev/mouse").expect("failed to open /dev/mouse"),
            kbd_file: std::fs::File::open("/dev/kbd").ok(),
            mods: 0,
            last_mouse_buttons: 0,
        }
    }

    /// Read current mouse position and button state.
    /// Returns (x, y, buttons), defaulting to (0, 0, 0) on error.
    pub fn read_mouse(&mut self) -> (i32, i32, u8) {
        read_mouse_state(&mut self.mouse_file).unwrap_or((0, 0, 0))
    }

    /// Detect a right button press (0->1 transition).
    ///
    /// Read before `detect_left_press`, which is the call that advances the
    /// stored button state for the frame.
    pub fn right_pressed(&self, buttons: u8) -> bool {
        (buttons & 0x02) != 0 && (self.last_mouse_buttons & 0x02) == 0
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
            let raw =
                u32::from_le_bytes([kbd_buf[i], kbd_buf[i + 1], kbd_buf[i + 2], kbd_buf[i + 3]]);
            i += 4;

            let is_release = raw & KEY_RELEASE_BIT != 0;
            let key = raw & !KEY_RELEASE_BIT;

            // Track modifier press/release
            if let Some(bit) = modifier_bit(key) {
                if is_release {
                    self.mods &= !bit;
                } else {
                    self.mods |= bit;
                }
                continue;
            }

            // Only process key-down events for shortcuts
            if is_release {
                continue;
            }

            let mods = self.mods;
            if !CLAIMED_CHORDS
                .iter()
                .any(|&(code, chord, _)| code == key && chord == mods)
            {
                continue;
            }

            // A chord fires once per press of its modifier: dropping the
            // modifier state here is what keeps Alt held across a repeat from
            // cycling on every frame.
            self.mods = 0;

            match key {
                RAW_W => return InputAction::DumpWindows,
                RAW_F4 => {
                    if let Some(fid) = focused_window_id {
                        return InputAction::AltF4 { focused_id: fid };
                    }
                }
                RAW_TAB => {
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
                }
                _ => {}
            }
        }

        InputAction::None
    }
}
