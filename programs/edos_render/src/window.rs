//! Window and shared memory syscall wrappers for the EDOS window server.

use edos_lib::sys::{is_err, syscall1, syscall2, syscall3, syscall4, syscall5};

// The kernel writes these and this crate reads them, so neither side owns
// them: they live in `window-abi`, which both depend on. Re-exported here so a
// client still writes `edos_render::window::WindowEvent`.
pub use window_abi::{
    TITLE_MAX, WindowEvent, WindowEventType, WindowId, WindowListEntry, flags, focused_id, property,
};

// Syscall numbers (window-specific; SHM syscalls live in edos_lib::shm)
const SYS_WINDOW_CREATE: u64 = 219;
const SYS_WINDOW_DESTROY: u64 = 220;
const SYS_WINDOW_SET: u64 = 221;
const SYS_WINDOW_GET: u64 = 222;
const SYS_WINDOW_POLL: u64 = 223;
const SYS_WINDOW_LIST: u64 = 224;
const SYS_WINDOW_SEND_EVENT: u64 = 225;
const SYS_WINDOW_DAMAGE: u64 = 232;
const SYS_WINDOW_WAIT: u64 = 286;
const SYS_WINDOW_PRESENT: u64 = 287;
const SYS_WINDOW_GRAB_KEY: u64 = 288;

// Re-export SHM functions and constants from edos_lib
pub use edos_lib::shm::{
    PROT_EXEC, PROT_READ, PROT_WRITE, shm_create, shm_destroy, shm_map, shm_size, shm_unmap,
};

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

/// Mark a window as repainted, advancing its damage counter.
/// A compositor redraws the window when the counter differs from the value it
/// last drew; the kernel never clears it, so two readers cannot race.
/// Put a window away, or bring it back.
///
/// Minimizing the focused window moves focus to whatever is left, and the
/// kernel delivers the focus events; a restored window takes focus itself.
pub fn window_minimize(id: WindowId, minimized: bool) -> Result<(), i64> {
    window_set(id, property::MINIMIZED, minimized as u64)
}

/// Report the frame drawn around a window, so pointer events land in its
/// client area rather than in the decorations.
///
/// Set by whoever draws the frame, per window, whenever the frame changes. A
/// window nobody decorates keeps a frame of zero, which is the right answer:
/// it owns every pixel it was given.
pub fn set_frame(id: WindowId, left: u32, top: u32, right: u32, bottom: u32) -> Result<(), i64> {
    let packed = (left as u64 & 0xFFFF)
        | (top as u64 & 0xFFFF) << 16
        | (right as u64 & 0xFFFF) << 32
        | (bottom as u64 & 0xFFFF) << 48;
    window_set(id, property::FRAME, packed)
}

pub fn window_damage(id: WindowId) -> Result<(), i64> {
    window_damage_rect(id, 0, 0, 0, 0)
}

/// Report the region repainted, in window-local pixels.
///
/// A zero width or height means the whole window, which is what a client that
/// does not track its own damage should send. Reporting the actual region is
/// what lets the compositor transfer a changed line rather than a changed
/// window, so it is worth a client knowing what it drew.
///
/// Regions accumulate in the kernel until a compositor takes them, so several
/// calls between two frames are all honoured.
pub fn window_damage_rect(id: WindowId, x: u32, y: u32, w: u32, h: u32) -> Result<(), i64> {
    let result = unsafe {
        syscall5(
            SYS_WINDOW_DAMAGE,
            id,
            x as u64,
            y as u64,
            w as u64,
            h as u64,
        )
    };
    if is_error(result) { Err(-1) } else { Ok(()) }
}

/// List all windows.
///
/// # Arguments
/// * `buffer` - Buffer to receive window list entries
///
/// # Returns
/// Total number of windows (may be more than buffer size).
/// What a [`wait`] returned for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Woke {
    /// Events are queued: poll to collect them.
    pub events: bool,
    /// A frame was presented since the count that was passed in.
    pub frame: bool,
    /// The presented-frame count to pass to the next wait.
    pub frame_seq: u64,
}

impl Woke {
    /// Whether the wait ended with nothing to do, which is a timeout.
    pub fn idle(&self) -> bool {
        !self.events && !self.frame
    }
}

/// Block until the window has something to do, or `timeout_ms` elapses.
///
/// `seen_frame` is the `frame_seq` from the previous wait, or 0 to start. A
/// timeout of 0 waits indefinitely.
///
/// This does not collect the events; [`window_poll`] still does that. It only
/// replaces the sleep a client would otherwise guess the length of, which
/// either wakes with nothing to do or leaves an event sitting for the rest of
/// the interval.
pub fn wait(id: WindowId, seen_frame: u64, timeout_ms: u64) -> Result<Woke, i64> {
    let result = unsafe { syscall3(SYS_WINDOW_WAIT, id, seen_frame, timeout_ms) };
    if is_err(result) {
        return Err(result as i64);
    }
    Ok(Woke {
        events: result & 1 != 0,
        frame: result & 2 != 0,
        frame_seq: result >> 32,
    })
}

/// Report that a frame has been put on the display, waking clients waiting for
/// one. Called by whatever owns the screen, once per presented frame.
pub fn present() {
    unsafe { syscall1(SYS_WINDOW_PRESENT, 0) };
}

pub fn window_list(buffer: &mut [WindowListEntry]) -> Result<usize, i64> {
    window_list_flags(buffer, 0)
}

/// Passed by the compositor to take each window's accumulated damage region.
/// Every other reader leaves the regions for it; see the kernel's
/// `WINDOW_LIST_CONSUME_DAMAGE`.
pub const WINDOW_LIST_CONSUME_DAMAGE: u64 = 1;

pub fn window_list_flags(buffer: &mut [WindowListEntry], flags: u64) -> Result<usize, i64> {
    let result = unsafe {
        syscall3(
            SYS_WINDOW_LIST,
            buffer.as_mut_ptr() as u64,
            buffer.len() as u64,
            flags,
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

/// Modifier bits for [`window_grab_key`]: Shift, either side.
pub const GRAB_MOD_SHIFT: u32 = 1 << 0;
/// Modifier bits for [`window_grab_key`]: Control, either side.
pub const GRAB_MOD_CTRL: u32 = 1 << 1;
/// Modifier bits for [`window_grab_key`]: Alt. AltGr is not Alt, since it
/// selects a character rather than qualifying one.
pub const GRAB_MOD_ALT: u32 = 1 << 2;

/// Claim a key chord, so the focused window never sees it.
///
/// The caller keeps reading the chord from `/dev/kbd` as before; what changes
/// is that the focused window stops receiving it. The modifier mask is matched
/// exactly, so Alt+Tab and Ctrl+Alt+Tab are separate claims. Requires the
/// window-shell privilege. Claims die with the process.
///
/// `code` is a `pc_keyboard` key code, the same encoding `/dev/kbd` carries.
pub fn window_grab_key(code: u32, mods: u32) -> Result<(), i64> {
    let result = unsafe { syscall3(SYS_WINDOW_GRAB_KEY, code as u64, mods as u64, 1) };
    if is_error(result) { Err(-1) } else { Ok(()) }
}

/// Release a claim made by [`window_grab_key`].
pub fn window_ungrab_key(code: u32, mods: u32) -> Result<(), i64> {
    let result = unsafe { syscall3(SYS_WINDOW_GRAB_KEY, code as u64, mods as u64, 0) };
    if is_error(result) { Err(-1) } else { Ok(()) }
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
        // No buffer is published here. A buffer nobody has drawn into is a
        // black rectangle, and the compositor draws the window's ground when a
        // window has no buffer at all; the first `swap_buffers` is what the
        // client means by "this is what I look like".

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

    /// Which of the two buffers `buffer_mut` currently hands out.
    ///
    /// A client that repaints only what changed needs this: the back buffer
    /// holds the frame from *two* frames ago, so what it has to redraw depends
    /// on which one it is about to draw into.
    pub fn back_index(&self) -> usize {
        self.back_index
    }

    /// Swap back and front buffers: makes the current back buffer visible to
    /// the compositor, then flips so the old front is now the back.
    pub fn swap_buffers(&mut self) {
        let back_shm_id = self.buffers[self.back_index].0;
        // Before the buffer, so a compositor never sees a buffer described by
        // the size of the one before it.
        let _ = window_set(
            self.id,
            property::BUFFER_SIZE,
            (self.width as u64) << 32 | self.height as u64,
        );
        let _ = window_set(self.id, property::BUFFER_SHM, back_shm_id);
        let _ = window_damage(self.id);
        self.back_index = 1 - self.back_index;
    }

    /// Swap buffers, reporting only `rect` as changed.
    ///
    /// Coordinates are relative to the window's own content, the same ones the
    /// client draws in. Reporting less than actually changed leaves stale
    /// pixels on screen, so a caller that is unsure wants [`swap_buffers`].
    pub fn swap_buffers_damaged(&mut self, x: i32, y: i32, w: u32, h: u32) {
        let back_shm_id = self.buffers[self.back_index].0;
        // Before the buffer, so a compositor never sees a buffer described by
        // the size of the one before it.
        let _ = window_set(
            self.id,
            property::BUFFER_SIZE,
            (self.width as u64) << 32 | self.height as u64,
        );
        let _ = window_set(self.id, property::BUFFER_SHM, back_shm_id);
        let x0 = x.max(0) as u32;
        let y0 = y.max(0) as u32;
        let w = (w as i32 + x.min(0)).max(0) as u32;
        let h = (h as i32 + y.min(0)).max(0) as u32;
        let _ = window_damage_rect(self.id, x0, y0, w, h);
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

        // Point the compositor at the new buffer 0 before destroying the old
        // ones, and at its size first: a buffer published under the previous
        // buffer's dimensions is read at a stride it was never written with.
        window_set(
            self.id,
            property::BUFFER_SIZE,
            (new_width as u64) << 32 | new_height as u64,
        )?;
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
