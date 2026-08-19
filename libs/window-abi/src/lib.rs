//! The window system's kernel/userspace boundary: the structs that cross it
//! and the constants that name their fields.
//!
//! Everything here is read by the kernel's window registry and written by the
//! compositor, or the reverse. None of it can be checked at compile time from
//! one side alone: a `#[repr(C)]` struct that gains a field on one side and not
//! the other is read at the wrong offsets, and a property number that means one
//! thing to the kernel and another to a client sets the wrong field. Both were
//! written out twice — this crate is why they no longer can be.
//!
//! Nothing that needs a syscall belongs here. `edos_render::window` wraps the
//! calls and the kernel implements them; this is only the shapes they exchange.

#![no_std]

/// Window identifier type.
pub type WindowId = u64;

/// Window event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum WindowEventType {
    /// Mouse moved within the window.
    MouseMove = 1,
    /// Mouse button pressed or released.
    MouseButton = 2,
    /// Mouse scroll wheel.
    MouseScroll = 3,
    /// Key pressed (raw scancode).
    KeyPress = 4,
    /// Key released (raw scancode).
    KeyRelease = 5,
    /// Character typed (Unicode).
    Character = 6,
    /// Window gained focus.
    FocusGained = 7,
    /// Window lost focus.
    FocusLost = 8,
    /// Close was requested.
    CloseRequested = 9,
    /// Window was resized.
    Resize = 10,
}

impl WindowEventType {
    /// Convert from the raw `u32` an event carries.
    pub fn from_u32(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::MouseMove),
            2 => Some(Self::MouseButton),
            3 => Some(Self::MouseScroll),
            4 => Some(Self::KeyPress),
            5 => Some(Self::KeyRelease),
            6 => Some(Self::Character),
            7 => Some(Self::FocusGained),
            8 => Some(Self::FocusLost),
            9 => Some(Self::CloseRequested),
            10 => Some(Self::Resize),
            _ => None,
        }
    }
}

/// A window event, as the kernel queues it and a client reads it.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct WindowEvent {
    /// Event type, one of [`WindowEventType`].
    pub event_type: u32,
    /// X coordinate (for mouse events, relative to the window).
    pub x: i32,
    /// Y coordinate (for mouse events, relative to the window).
    pub y: i32,
    /// Key/button code or character.
    pub code: u32,
    /// Additional data, such as the pressed state for buttons.
    pub data: u32,
}

impl WindowEvent {
    /// The event type as an enum, or `None` for a number this build does not
    /// know.
    pub fn event_type(&self) -> Option<WindowEventType> {
        WindowEventType::from_u32(self.event_type)
    }

    /// A mouse move.
    pub fn mouse_move(x: i32, y: i32) -> Self {
        Self {
            event_type: WindowEventType::MouseMove as u32,
            x,
            y,
            ..Self::default()
        }
    }

    /// A mouse button going down or up.
    pub fn mouse_button(x: i32, y: i32, button: u8, pressed: bool) -> Self {
        Self {
            event_type: WindowEventType::MouseButton as u32,
            x,
            y,
            code: button as u32,
            data: pressed as u32,
        }
    }

    /// A scroll wheel step.
    pub fn mouse_scroll(x: i32, y: i32, delta: i8) -> Self {
        Self {
            event_type: WindowEventType::MouseScroll as u32,
            x,
            y,
            code: 0,
            data: delta as i32 as u32,
        }
    }

    /// A key going down.
    pub fn key_press(key: u32) -> Self {
        Self {
            event_type: WindowEventType::KeyPress as u32,
            code: key,
            ..Self::default()
        }
    }

    /// A key coming up.
    pub fn key_release(key: u32) -> Self {
        Self {
            event_type: WindowEventType::KeyRelease as u32,
            code: key,
            ..Self::default()
        }
    }

    /// Focus arriving.
    pub fn focus_gained() -> Self {
        Self {
            event_type: WindowEventType::FocusGained as u32,
            ..Self::default()
        }
    }

    /// Focus leaving.
    pub fn focus_lost() -> Self {
        Self {
            event_type: WindowEventType::FocusLost as u32,
            ..Self::default()
        }
    }

    /// The window manager asking the client to close.
    pub fn close_requested() -> Self {
        Self {
            event_type: WindowEventType::CloseRequested as u32,
            ..Self::default()
        }
    }

    /// The window's new size, which the client redraws into.
    ///
    /// Carried in `x` and `y`, not in `code`/`data`: that is what the window
    /// manager sends and what every client reads back. Nothing enforces it,
    /// which is why the constructor exists rather than each sender packing the
    /// fields itself.
    pub fn resize(width: u32, height: u32) -> Self {
        Self {
            event_type: WindowEventType::Resize as u32,
            x: width as i32,
            y: height as i32,
            ..Self::default()
        }
    }

    /// The size a `Resize` event carries.
    pub fn resize_size(&self) -> Option<(u32, u32)> {
        (self.event_type == WindowEventType::Resize as u32).then(|| (self.x as u32, self.y as u32))
    }

    /// Whether this is a mouse button press.
    pub fn is_button_press(&self) -> bool {
        self.event_type == WindowEventType::MouseButton as u32 && self.data != 0
    }

    /// Whether this is a mouse button release.
    pub fn is_button_release(&self) -> bool {
        self.event_type == WindowEventType::MouseButton as u32 && self.data == 0
    }

    /// The character a `Character` event carries.
    ///
    /// Nothing produces one today: the kernel sends `KeyPress`/`KeyRelease`
    /// and a client maps keycodes itself through `edos_lib::keymap`. The
    /// variant and this reader stay because the number is part of the ABI.
    pub fn character(&self) -> Option<char> {
        if self.event_type == WindowEventType::Character as u32 {
            char::from_u32(self.code)
        } else {
            None
        }
    }
}

/// Length of [`WindowListEntry::title`], including the terminator.
pub const TITLE_MAX: usize = 64;

/// One window, as the window-list syscall reports it.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct WindowListEntry {
    pub id: u64,
    pub pid: u64,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub z_order: u32,
    pub visible: u32,
    pub buffer_shm_id: u64,
    /// Pixel dimensions the client wrote `buffer_shm_id` at, or zero when it
    /// has not said. Read the buffer at these rather than at `width` and
    /// `height`: those are the manager's and change before the client has
    /// allocated to match, so reading at them shears the picture.
    pub buffer_width: u32,
    pub buffer_height: u32,
    pub flags: u64,
    /// The frame the window's manager last reported, packed as four u16 edges.
    /// Reported back so a manager can skip rewriting a frame it already set.
    pub frame: u64,
    /// The client's repaint count. A reader keeps the value it last acted on
    /// and redraws when it differs; the kernel never resets it.
    pub damage_seq: u32,
    /// The region the client reported repainting, in window-local pixels, or
    /// all zeroes when it reported none. Only meaningful to a caller that
    /// consumes damage.
    pub damage_x: u32,
    pub damage_y: u32,
    pub damage_w: u32,
    pub damage_h: u32,
    /// Set for the window that currently holds input focus. The kernel's
    /// registry is the single source of truth: clients must not re-derive
    /// focus from `z_order`, which also moves when a window is merely raised.
    pub focused: u32,
    /// Set while the window is put away. It stays in this list so a panel can
    /// offer a way back; the compositor skips it when drawing.
    pub minimized: u32,
    pub title: [u8; TITLE_MAX],
}

impl WindowListEntry {
    /// The title as a string, up to its terminator.
    pub fn title_str(&self) -> &str {
        let len = self.title.iter().position(|&b| b == 0).unwrap_or(TITLE_MAX);
        core::str::from_utf8(&self.title[..len]).unwrap_or("")
    }

    /// Whether this window holds input focus.
    pub fn is_focused(&self) -> bool {
        self.focused != 0
    }

    /// Whether the window is put away. A minimized window is still listed, so
    /// a compositor must check this before drawing it.
    pub fn is_minimized(&self) -> bool {
        self.minimized != 0
    }

    /// Whether the window occupies screen space right now.
    pub fn on_screen(&self) -> bool {
        self.visible != 0 && self.minimized == 0
    }
}

impl Default for WindowListEntry {
    fn default() -> Self {
        Self {
            id: 0,
            pid: 0,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            z_order: 0,
            visible: 0,
            buffer_shm_id: 0,
            buffer_width: 0,
            buffer_height: 0,
            flags: 0,
            frame: 0,
            damage_seq: 0,
            damage_x: 0,
            damage_y: 0,
            damage_w: 0,
            damage_h: 0,
            focused: 0,
            minimized: 0,
            title: [0; TITLE_MAX],
        }
    }
}

impl core::fmt::Debug for WindowListEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WindowListEntry")
            .field("id", &self.id)
            .field("title", &self.title_str())
            .finish()
    }
}

/// The id of the focused window in a list, if any.
pub fn focused_id(windows: &[WindowListEntry]) -> Option<u64> {
    windows.iter().find(|w| w.is_focused()).map(|w| w.id)
}

/// Window property numbers, for the window-set and window-get syscalls.
pub mod property {
    /// Window visibility (0 = hidden, 1 = visible).
    pub const VISIBLE: u64 = 1;
    /// Window X position.
    pub const X: u64 = 2;
    /// Window Y position.
    pub const Y: u64 = 3;
    /// Window width.
    pub const WIDTH: u64 = 4;
    /// Window height.
    pub const HEIGHT: u64 = 5;
    /// Title string pointer (set only).
    pub const TITLE_PTR: u64 = 6;
    /// Shared memory buffer id for the window's contents.
    pub const BUFFER_SHM: u64 = 7;
    /// Window flags, from [`super::flags`].
    pub const FLAGS: u64 = 8;
    /// Put the window away, or bring it back. Non-zero minimizes.
    pub const MINIMIZED: u64 = 9;
    /// Thickness of the window manager's frame around this window, packed as
    /// four u16 edges: left, top, right, bottom, low bits first. Set by
    /// whoever decorates the window, so pointer routing follows the frame that
    /// is actually drawn.
    pub const FRAME: u64 = 10;
    /// Pixel dimensions of the buffer named by [`BUFFER_SHM`], packed as two
    /// u32s: width in the high half, height in the low. Publish this before
    /// the buffer it describes.
    ///
    /// The buffer's stride is the client's, not the window manager's. A resize
    /// changes [`WIDTH`] the moment the manager decides it, while the client
    /// allocates its new buffer some frames later, so a compositor that reads
    /// the buffer at the window's current width reads it at a stride it was
    /// never written with and draws a sheared picture.
    pub const BUFFER_SIZE: u64 = 11;
}

/// Window flags, as [`property::FLAGS`] carries them.
pub mod flags {
    /// No title bar or border: the window owns every pixel it was given, and
    /// the compositor neither decorates it nor lets the pointer drag it.
    pub const FLAG_UNDECORATED: u64 = 1;
    /// Never holds keyboard focus. Chrome that paints no focus state, such as
    /// the panel, since input landing there is invisible to the user.
    pub const FLAG_NO_FOCUS: u64 = 2;
    /// A panel: undecorated and never focusable. The kernel never tests this
    /// combined value, it tests the two bits separately, which is the whole
    /// point — a menu is undecorated but must take focus, or it cannot be
    /// dismissed by focus loss. Kept as the name userspace sets.
    pub const FLAG_DOCK: u64 = FLAG_UNDECORATED | FLAG_NO_FOCUS;
}
