//! Window and shared memory syscall wrappers for the EDOS window server.

use std::arch::asm;

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
}

/// Entry in the window list returned by window_list syscall.
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
        }
    }
}

/// Window property constants (for window_set/window_get).
pub mod property {
    /// Window visibility (0 = hidden, 1 = visible).
    pub const VISIBLE: u64 = 0;
    /// Window X position.
    pub const X: u64 = 1;
    /// Window Y position.
    pub const Y: u64 = 2;
    /// Window width.
    pub const WIDTH: u64 = 3;
    /// Window height.
    pub const HEIGHT: u64 = 4;
    /// Title string pointer (for set only).
    pub const TITLE_PTR: u64 = 5;
    /// Shared memory buffer ID for window contents.
    pub const BUFFER_SHM: u64 = 6;
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

// Protection flags for shm_map
pub const PROT_READ: u64 = 0x1;
pub const PROT_WRITE: u64 = 0x2;
pub const PROT_EXEC: u64 = 0x4;

/// Raw syscall with 4 arguments.
#[inline(always)]
unsafe fn syscall4(num: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            in("r10") arg4,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Raw syscall with 3 arguments.
#[inline(always)]
unsafe fn syscall3(num: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Raw syscall with 2 arguments.
#[inline(always)]
unsafe fn syscall2(num: u64, arg1: u64, arg2: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            in("rsi") arg2,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Raw syscall with 1 argument.
#[inline(always)]
unsafe fn syscall1(num: u64, arg1: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

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
    if is_error(result) {
        Err(-1)
    } else {
        Ok(())
    }
}

/// Set a window property.
pub fn window_set(id: WindowId, prop: u64, value: u64) -> Result<(), i64> {
    let result = unsafe { syscall3(SYS_WINDOW_SET, id, prop, value) };
    if is_error(result) {
        Err(-1)
    } else {
        Ok(())
    }
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
    if result as i64 == -1 {
        Err(-1)
    } else {
        Ok(())
    }
}

/// Destroy a shared memory region.
///
/// # Arguments
/// * `shm_id` - Shared memory ID
pub fn shm_destroy(shm_id: u64) -> Result<(), i64> {
    let result = unsafe { syscall1(SYS_SHM_DESTROY, shm_id) };
    if result as i64 == -1 {
        Err(-1)
    } else {
        Ok(())
    }
}

/// Helper struct for managing a window with its shared memory buffer.
pub struct Window {
    pub id: WindowId,
    pub width: u32,
    pub height: u32,
    pub shm_id: Option<u64>,
    pub buffer: Option<*mut u32>,
}

impl Window {
    /// Create a new window with an attached buffer.
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Result<Self, i64> {
        let id = window_create(x, y, width, height)?;

        // Create shared memory for the window buffer
        let buffer_size = (width as usize) * (height as usize) * 4; // 4 bytes per pixel
        let shm_id = shm_create(buffer_size)?;

        // Map the shared memory
        let buffer_ptr = shm_map(shm_id, PROT_READ | PROT_WRITE)?;

        // Attach buffer to window
        window_set(id, property::BUFFER_SHM, shm_id)?;

        Ok(Self {
            id,
            width,
            height,
            shm_id: Some(shm_id),
            buffer: Some(buffer_ptr as *mut u32),
        })
    }

    /// Get a mutable slice to the window buffer.
    pub fn buffer_mut(&mut self) -> Option<&mut [u32]> {
        self.buffer.map(|ptr| unsafe {
            std::slice::from_raw_parts_mut(ptr, (self.width * self.height) as usize)
        })
    }

    /// Get a slice to the window buffer.
    pub fn buffer(&self) -> Option<&[u32]> {
        self.buffer.map(|ptr| unsafe {
            std::slice::from_raw_parts(ptr, (self.width * self.height) as usize)
        })
    }

    /// Set the window title.
    pub fn set_title(&self, title: &str) -> Result<(), i64> {
        let mut title_bytes: Vec<u8> = title.bytes().collect();
        title_bytes.push(0); // null terminator
        window_set(self.id, property::TITLE_PTR, title_bytes.as_ptr() as u64)
    }

    /// Show the window.
    pub fn show(&self) -> Result<(), i64> {
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

    /// Fill the buffer with a color.
    pub fn fill(&mut self, color: u32) {
        if let Some(buffer) = self.buffer_mut() {
            for pixel in buffer.iter_mut() {
                *pixel = color;
            }
        }
    }

    /// Set a pixel in the buffer.
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
        // Unmap and destroy shared memory
        if let Some(ptr) = self.buffer.take() {
            let _ = shm_unmap(ptr as *mut u8);
        }
        if let Some(shm_id) = self.shm_id.take() {
            let _ = shm_destroy(shm_id);
        }
        // Destroy the window
        let _ = window_destroy(self.id);
    }
}

/// Get the current mouse position by reading from /dev/mouse.
/// Returns (x, y) coordinates.
pub fn get_mouse_position() -> Option<(i32, i32)> {
    get_mouse_state().map(|(x, y, _)| (x, y))
}

/// Get the current mouse state (position and buttons) by reading from /dev/mouse.
/// Returns (x, y, buttons) where buttons is a bitmask (bit 0 = left, bit 1 = right, bit 2 = middle).
pub fn get_mouse_state() -> Option<(i32, i32, u8)> {
    use std::fs::File;
    use std::io::Read;

    // MouseState structure from kernel: x (i32), y (i32), buttons (u8)
    let mut file = File::open("/dev/mouse").ok()?;
    let mut buf = [0u8; 9]; // 4 + 4 + 1 bytes
    file.read_exact(&mut buf).ok()?;

    let x = i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let y = i32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let buttons = buf[8];

    Some((x, y, buttons))
}
