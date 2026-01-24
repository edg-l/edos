//! Window management syscall wrappers.

use crate::sys::{
    calls::{syscall1, syscall2, syscall3, syscall4},
    constants::*,
};

/// Window ID type.
pub type WindowId = u64;

/// Window event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum WindowEventType {
    MouseMove = 1,
    MouseButton = 2,
    MouseScroll = 3,
    KeyPress = 4,
    KeyRelease = 5,
    Character = 6,
    FocusGained = 7,
    FocusLost = 8,
    CloseRequested = 9,
    Resize = 10,
}

impl WindowEventType {
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
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

/// A window event.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct WindowEvent {
    /// Event type.
    pub event_type: u32,
    /// X coordinate (for mouse events, relative to window).
    pub x: i32,
    /// Y coordinate (for mouse events, relative to window).
    pub y: i32,
    /// Key/button code or character.
    pub code: u32,
    /// Additional data (e.g., pressed state for buttons).
    pub data: u32,
}

impl WindowEvent {
    /// Get the event type as an enum.
    pub fn kind(&self) -> Option<WindowEventType> {
        WindowEventType::from_u32(self.event_type)
    }

    /// For MouseButton events, returns whether the button is pressed.
    pub fn is_pressed(&self) -> bool {
        self.data != 0
    }

    /// For Character events, returns the character.
    pub fn character(&self) -> Option<char> {
        char::from_u32(self.code)
    }
}

/// Entry in the window list returned by window_list.
#[derive(Debug, Clone, Copy)]
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
}

/// Create a new window.
///
/// Returns the window ID on success, or an error.
pub fn window_create(x: i32, y: i32, width: u32, height: u32) -> Result<WindowId, i64> {
    let result = unsafe {
        syscall4(
            SYS_WINDOW_CREATE,
            x as i64 as u64,
            y as i64 as u64,
            width as u64,
            height as u64,
        )
    };
    if result == !0u64 {
        Err(crate::errno() as i64)
    } else {
        Ok(result)
    }
}

/// Destroy a window.
pub fn window_destroy(id: WindowId) -> Result<(), i64> {
    let result = unsafe { syscall1(SYS_WINDOW_DESTROY, id) };
    if result == !0u64 {
        Err(crate::errno() as i64)
    } else {
        Ok(())
    }
}

/// Set a window property.
pub fn window_set(id: WindowId, property: u64, value: u64) -> Result<(), i64> {
    let result = unsafe { syscall3(SYS_WINDOW_SET, id, property, value) };
    if result == !0u64 {
        Err(crate::errno() as i64)
    } else {
        Ok(())
    }
}

/// Get a window property.
pub fn window_get(id: WindowId, property: u64) -> Result<u64, i64> {
    let result = unsafe { syscall2(SYS_WINDOW_GET, id, property) };
    if result == !0u64 {
        let err = crate::errno();
        if err != crate::sys::Errno::Clear {
            return Err(err as i64);
        }
    }
    Ok(result)
}

/// Poll events for a window.
///
/// Returns the number of events written to the buffer.
pub fn window_poll(id: WindowId, events: &mut [WindowEvent]) -> Result<usize, i64> {
    if events.is_empty() {
        return Ok(0);
    }
    let result = unsafe {
        syscall3(
            SYS_WINDOW_POLL,
            id,
            events.as_mut_ptr() as u64,
            events.len() as u64,
        )
    };
    if result == !0u64 {
        Err(crate::errno() as i64)
    } else {
        Ok(result as usize)
    }
}

/// List all windows (for compositor).
///
/// If buffer is empty, returns just the count.
pub fn window_list(buffer: &mut [WindowListEntry]) -> Result<usize, i64> {
    let result = unsafe {
        syscall2(
            SYS_WINDOW_LIST,
            buffer.as_mut_ptr() as u64,
            buffer.len() as u64,
        )
    };
    if result == !0u64 {
        Err(crate::errno() as i64)
    } else {
        Ok(result as usize)
    }
}

/// Set window visibility.
pub fn window_set_visible(id: WindowId, visible: bool) -> Result<(), i64> {
    window_set(id, WINDOW_PROP_VISIBLE, visible as u64)
}

/// Set window position.
pub fn window_set_position(id: WindowId, x: i32, y: i32) -> Result<(), i64> {
    window_set(id, WINDOW_PROP_X, x as i64 as u64)?;
    window_set(id, WINDOW_PROP_Y, y as i64 as u64)
}

/// Set window size.
pub fn window_set_size(id: WindowId, width: u32, height: u32) -> Result<(), i64> {
    window_set(id, WINDOW_PROP_WIDTH, width as u64)?;
    window_set(id, WINDOW_PROP_HEIGHT, height as u64)
}

/// Set window title.
///
/// # Safety
/// The title string must be null-terminated and remain valid during the syscall.
pub unsafe fn window_set_title(id: WindowId, title: *const u8) -> Result<(), i64> {
    window_set(id, WINDOW_PROP_TITLE_PTR, title as u64)
}

/// Associate a shared memory buffer with the window.
pub fn window_set_buffer(id: WindowId, shm_id: u64) -> Result<(), i64> {
    window_set(id, WINDOW_PROP_BUFFER_SHM, shm_id)
}

/// Get the shared memory buffer ID for a window.
pub fn window_get_buffer(id: WindowId) -> Result<u64, i64> {
    window_get(id, WINDOW_PROP_BUFFER_SHM)
}
