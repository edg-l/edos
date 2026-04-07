//! Window and shared memory syscall wrappers for the EDOS window server.

use edos_lib::sys::{syscall1, syscall2, syscall3, syscall4};

/// Window identifier type.
pub type WindowId = u64;

/// Window event types (mirrors kernel WindowEventType).
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
    /// Convert from raw u32 value.
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

/// A window event (mirrors kernel WindowEvent).
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

impl Default for WindowEvent {
    fn default() -> Self {
        Self {
            event_type: 0,
            x: 0,
            y: 0,
            code: 0,
            data: 0,
        }
    }
}

impl WindowEvent {
    /// Get the event type as an enum.
    pub fn event_type(&self) -> Option<WindowEventType> {
        WindowEventType::from_u32(self.event_type)
    }

    /// Check if this is a mouse button press.
    pub fn is_button_press(&self) -> bool {
        self.event_type == WindowEventType::MouseButton as u32 && self.data != 0
    }

    /// Check if this is a mouse button release.
    pub fn is_button_release(&self) -> bool {
        self.event_type == WindowEventType::MouseButton as u32 && self.data == 0
    }

    /// Get the character for Character events.
    pub fn character(&self) -> Option<char> {
        if self.event_type == WindowEventType::Character as u32 {
            char::from_u32(self.code)
        } else {
            None
        }
    }

    /// Create a close requested event.
    pub fn close_requested() -> Self {
        Self {
            event_type: WindowEventType::CloseRequested as u32,
            x: 0,
            y: 0,
            code: 0,
            data: 0,
        }
    }
}

/// Maximum title length in WindowListEntry (including null terminator).
pub const TITLE_MAX: usize = 64;

/// Entry in the window list returned by window_list syscall.
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
    pub flags: u64,
    pub title: [u8; TITLE_MAX],
}

impl WindowListEntry {
    /// Get the title as a string slice.
    pub fn title_str(&self) -> &str {
        let len = self.title.iter().position(|&b| b == 0).unwrap_or(TITLE_MAX);
        core::str::from_utf8(&self.title[..len]).unwrap_or("")
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
            flags: 0,
            title: [0; TITLE_MAX],
        }
    }
}

/// Window property constants (for window_set/window_get).
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
    /// Title string pointer (for set only).
    pub const TITLE_PTR: u64 = 6;
    /// Shared memory buffer ID for window contents.
    pub const BUFFER_SHM: u64 = 7;
    /// Window flags.
    pub const FLAGS: u64 = 8;
}

/// Window flags.
pub mod flags {
    /// Dock window: no decorations, not draggable/resizable.
    pub const FLAG_DOCK: u64 = 1;
}

// Syscall numbers
const SYS_SHM_CREATE: u64 = 215;
const SYS_SHM_MAP: u64 = 216;
const SYS_SHM_UNMAP: u64 = 217;
const SYS_SHM_DESTROY: u64 = 218;
const SYS_WINDOW_CREATE: u64 = 219;
const SYS_WINDOW_DESTROY: u64 = 220;
const SYS_WINDOW_SET: u64 = 221;
const SYS_WINDOW_GET: u64 = 222;
const SYS_WINDOW_POLL: u64 = 223;
const SYS_WINDOW_LIST: u64 = 224;
const SYS_WINDOW_SEND_EVENT: u64 = 225;

// Protection flags for shm_map
pub const PROT_READ: u64 = 0x1;
pub const PROT_WRITE: u64 = 0x2;
pub const PROT_EXEC: u64 = 0x4;

/// Check if a syscall result indicates an error.
#[inline]
fn is_error(result: u64) -> bool {
    result == !0u64
}

/// Create a new window.
///
/// # Arguments
/// * `x` - X position
/// * `y` - Y position
/// * `width` - Window width
/// * `height` - Window height
///
/// # Returns
/// Window ID on success, or error code.
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
    if is_error(result) {
        Err(-1)
    } else {
        Ok(result)
    }
}

/// Destroy a window.
pub fn window_destroy(id: WindowId) -> Result<(), i64> {
    let result = unsafe { syscall1(SYS_WINDOW_DESTROY, id) };
    if is_error(result) { Err(-1) } else { Ok(()) }
}

/// Set a window property.
pub fn window_set(id: WindowId, prop: u64, value: u64) -> Result<(), i64> {
    let result = unsafe { syscall3(SYS_WINDOW_SET, id, prop, value) };
    if is_error(result) { Err(-1) } else { Ok(()) }
}

/// Get a window property.
pub fn window_get(id: WindowId, prop: u64) -> Result<u64, i64> {
    let result = unsafe { syscall2(SYS_WINDOW_GET, id, prop) };
    if is_error(result) {
        Err(-1)
    } else {
        Ok(result)
    }
}

/// Poll events for a window.
///
/// # Arguments
/// * `id` - Window ID
/// * `events` - Buffer to receive events
///
/// # Returns
/// Number of events received.
pub fn window_poll(id: WindowId, events: &mut [WindowEvent]) -> Result<usize, i64> {
    let result = unsafe {
        syscall3(
            SYS_WINDOW_POLL,
            id,
            events.as_mut_ptr() as u64,
            events.len() as u64,
        )
    };
    if is_error(result) {
        Err(-1)
    } else {
        Ok(result as usize)
    }
}

/// List all windows.
///
/// # Arguments
/// * `buffer` - Buffer to receive window list entries
///
/// # Returns
/// Total number of windows (may be more than buffer size).
pub fn window_list(buffer: &mut [WindowListEntry]) -> Result<usize, i64> {
    let result = unsafe {
        syscall2(
            SYS_WINDOW_LIST,
            buffer.as_mut_ptr() as u64,
            buffer.len() as u64,
        )
    };
    if is_error(result) {
        Err(-1)
    } else {
        Ok(result as usize)
    }
}

/// Send an event to a window.
///
/// This allows the window manager to send events (like CloseRequested)
/// to windows it doesn't own.
///
/// # Arguments
/// * `id` - Window ID
/// * `event` - Event to send
pub fn window_send_event(id: WindowId, event: &WindowEvent) -> Result<(), i64> {
    let result = unsafe {
        syscall2(
            SYS_WINDOW_SEND_EVENT,
            id,
            event as *const WindowEvent as u64,
        )
    };
    if is_error(result) { Err(-1) } else { Ok(()) }
}

/// Create a shared memory region.
///
/// # Arguments
/// * `size` - Size of the region in bytes
///
/// # Returns
/// Shared memory ID on success.
pub fn shm_create(size: usize) -> Result<u64, i64> {
    let result = unsafe { syscall1(SYS_SHM_CREATE, size as u64) };
    // shm_create returns -1 (i64) on error, which is 0xFFFFFFFFFFFFFFFF as u64
    if result as i64 == -1 {
        Err(-1)
    } else {
        Ok(result)
    }
}

/// Map a shared memory region into this process.
///
/// # Arguments
/// * `shm_id` - Shared memory ID
/// * `prot` - Protection flags (PROT_READ, PROT_WRITE, PROT_EXEC)
///
/// # Returns
/// Pointer to the mapped memory.
pub fn shm_map(shm_id: u64, prot: u64) -> Result<*mut u8, i64> {
    let result = unsafe { syscall3(SYS_SHM_MAP, shm_id, 0, prot) };
    if is_error(result) {
        Err(-1)
    } else {
        Ok(result as *mut u8)
    }
}

/// Unmap a shared memory region.
///
/// # Arguments
/// * `addr` - Address of the mapped region
pub fn shm_unmap(addr: *mut u8) -> Result<(), i64> {
    let result = unsafe { syscall1(SYS_SHM_UNMAP, addr as u64) };
    if result as i64 == -1 { Err(-1) } else { Ok(()) }
}

/// Destroy a shared memory region.
///
/// # Arguments
/// * `shm_id` - Shared memory ID
pub fn shm_destroy(shm_id: u64) -> Result<(), i64> {
    let result = unsafe { syscall1(SYS_SHM_DESTROY, shm_id) };
    if result as i64 == -1 { Err(-1) } else { Ok(()) }
}

/// Helper struct for managing a window with double-buffered shared memory.
///
/// Two shm buffers are maintained: the client draws to the back buffer, then
/// calls `swap_buffers()` to atomically make it visible to the compositor.
pub struct Window {
    pub id: WindowId,
    pub width: u32,
    pub height: u32,
    /// (shm_id, mapped_ptr) for each of the two buffers.
    buffers: [(u64, *mut u32); 2],
    /// Index into `buffers` of the buffer currently being drawn to.
    back_index: usize,
}

fn alloc_buffer(width: u32, height: u32) -> Result<(u64, *mut u32), i64> {
    let size = (width as usize) * (height as usize) * 4;
    let shm_id = shm_create(size)?;
    let ptr = shm_map(shm_id, PROT_READ | PROT_WRITE)?;
    Ok((shm_id, ptr as *mut u32))
}

fn free_buffer(shm_id: u64, ptr: *mut u32) {
    let _ = shm_unmap(ptr as *mut u8);
    let _ = shm_destroy(shm_id);
}

impl Window {
    /// Create a new window with two attached shm buffers.
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Result<Self, i64> {
        let id = window_create(x, y, width, height)?;

        let buf0 = alloc_buffer(width, height)?;
        let buf1 = alloc_buffer(width, height).map_err(|e| {
            free_buffer(buf0.0, buf0.1);
            e
        })?;

        // Point the compositor at buffer 0 initially.
        window_set(id, property::BUFFER_SHM, buf0.0)?;

        Ok(Self {
            id,
            width,
            height,
            buffers: [buf0, buf1],
            back_index: 1,
        })
    }

    /// Get a mutable slice to the back (draw) buffer.
    pub fn buffer_mut(&mut self) -> Option<&mut [u32]> {
        let ptr = self.buffers[self.back_index].1;
        Some(unsafe { std::slice::from_raw_parts_mut(ptr, (self.width * self.height) as usize) })
    }

    /// Get a slice to the back (draw) buffer.
    pub fn buffer(&self) -> Option<&[u32]> {
        let ptr = self.buffers[self.back_index].1;
        Some(unsafe { std::slice::from_raw_parts(ptr, (self.width * self.height) as usize) })
    }

    /// Swap back and front buffers: makes the current back buffer visible to
    /// the compositor, then flips so the old front is now the back.
    pub fn swap_buffers(&mut self) {
        let back_shm_id = self.buffers[self.back_index].0;
        let _ = window_set(self.id, property::BUFFER_SHM, back_shm_id);
        self.back_index = 1 - self.back_index;
    }

    /// Set the window title (max 255 chars, truncated if longer).
    /// Uses a stack buffer so the pointer is valid for the synchronous syscall.
    pub fn set_title(&self, title: &str) -> Result<(), i64> {
        let mut buf = [0u8; 256];
        let len = title.len().min(255);
        buf[..len].copy_from_slice(&title.as_bytes()[..len]);
        buf[len] = 0;
        window_set(self.id, property::TITLE_PTR, buf.as_ptr() as u64)
    }

    /// Show the window and present the first frame.
    pub fn show(&mut self) -> Result<(), i64> {
        self.swap_buffers();
        window_set(self.id, property::VISIBLE, 1)
    }

    /// Hide the window.
    pub fn hide(&self) -> Result<(), i64> {
        window_set(self.id, property::VISIBLE, 0)
    }

    /// Move the window.
    pub fn set_position(&self, x: i32, y: i32) -> Result<(), i64> {
        window_set(self.id, property::X, x as i64 as u64)?;
        window_set(self.id, property::Y, y as i64 as u64)
    }

    /// Poll events for this window.
    pub fn poll_events(&self, events: &mut [WindowEvent]) -> Result<usize, i64> {
        window_poll(self.id, events)
    }

    /// Resize the window buffer to new dimensions.
    /// Allocates two new shm buffers and frees the old pair.
    pub fn resize(&mut self, new_width: u32, new_height: u32) -> Result<(), i64> {
        if new_width == 0 || new_height == 0 {
            return Err(-1);
        }

        let new_buf0 = alloc_buffer(new_width, new_height)?;
        let new_buf1 = alloc_buffer(new_width, new_height).map_err(|e| {
            free_buffer(new_buf0.0, new_buf0.1);
            e
        })?;

        // Point the compositor at the new buffer 0 before destroying the old ones.
        window_set(self.id, property::BUFFER_SHM, new_buf0.0)?;

        let old = self.buffers;
        self.buffers = [new_buf0, new_buf1];
        self.back_index = 1;
        self.width = new_width;
        self.height = new_height;

        free_buffer(old[0].0, old[0].1);
        free_buffer(old[1].0, old[1].1);

        Ok(())
    }

    /// Fill the back buffer with a solid color.
    pub fn fill(&mut self, color: u32) {
        if let Some(buffer) = self.buffer_mut() {
            for pixel in buffer.iter_mut() {
                *pixel = color;
            }
        }
    }

    /// Set a pixel in the back buffer.
    pub fn set_pixel(&mut self, x: u32, y: u32, color: u32) {
        if x < self.width && y < self.height {
            let width = self.width;
            if let Some(buffer) = self.buffer_mut() {
                buffer[(y * width + x) as usize] = color;
            }
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // Destroy window first so the WM stops referencing our SHM buffers,
        // then free the buffers. Reverse order causes the WM to read freed memory.
        let _ = window_destroy(self.id);
        free_buffer(self.buffers[0].0, self.buffers[0].1);
        free_buffer(self.buffers[1].0, self.buffers[1].1);
    }
}

/// Get the current mouse position by reading from /dev/mouse.
/// Returns (x, y) coordinates.
pub fn get_mouse_position() -> Option<(i32, i32)> {
    get_mouse_state().map(|(x, y, _)| (x, y))
}

/// Get the current mouse state (position and buttons) by reading from /dev/mouse.
/// Returns (x, y, buttons) where buttons is a bitmask (bit 0 = left, bit 1 = right, bit 2 = middle).
///
/// NOTE: Opens /dev/mouse on every call. For hot loops, use [`read_mouse_state`] with
/// a pre-opened file handle instead.
pub fn get_mouse_state() -> Option<(i32, i32, u8)> {
    use std::fs::File;
    let mut file = File::open("/dev/mouse").ok()?;
    read_mouse_state(&mut file)
}

/// Read mouse state from an already-opened /dev/mouse file handle.
/// Use this in hot loops (e.g. compositor) to avoid opening the device every frame.
pub fn read_mouse_state(file: &mut std::fs::File) -> Option<(i32, i32, u8)> {
    use std::io::Read;

    // MouseEvent structure from kernel is 16 bytes:
    // x (i32), y (i32), dx (i16), dy (i16), buttons (u8), scroll (i8), padding (2)
    let mut buf = [0u8; 16];
    file.read_exact(&mut buf).ok()?;

    let x = i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let y = i32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let buttons = buf[12]; // Correct offset for buttons field

    Some((x, y, buttons))
}
