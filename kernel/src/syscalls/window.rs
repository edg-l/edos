//! Window syscalls for the window server.

use alloc::string::String;

use crate::{
    thread::scheduler::sched,
    util::uaccess::{try_copy_to_user, try_read_user},
    window::{
        WindowEvent,
        input::{get_or_create_event_queue, poll_events, remove_event_queue},
        registry::{WINDOW_REGISTRY, WindowId, property},
    },
};

use super::Errno;

/// Create a new window.
///
/// Arguments:
/// - rdi: x position
/// - rsi: y position
/// - rdx: width
/// - r10: height
///
/// Returns: window ID on success, !0 on error (sets errno).
pub fn sys_window_create(x: i64, y: i64, width: u64, height: u64) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    // Validate dimensions
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    }

    let pid = info.lock().pid;

    // Create the window
    let window_id =
        WINDOW_REGISTRY
            .write()
            .create_window(pid, x as i32, y as i32, width as u32, height as u32);

    // Create event queue for the window
    get_or_create_event_queue(window_id);

    window_id
}

/// Destroy a window.
///
/// Arguments:
/// - rdi: window ID
///
/// Returns: 0 on success, !0 on error (sets errno).
pub fn sys_window_destroy(window_id: WindowId) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    let pid = info.lock().pid;

    // Check ownership and destroy in a single write lock to avoid TOCTOU.
    {
        let mut registry = WINDOW_REGISTRY.write();
        if let Some(window) = registry.get_window(window_id) {
            if window.pid != pid {
                info.lock().errno = Errno::EPERM;
                return !0u64;
            }
        } else {
            info.lock().errno = Errno::ENOENT;
            return !0u64;
        }
        if !registry.destroy_window(window_id) {
            info.lock().errno = Errno::ENOENT;
            return !0u64;
        }
    }

    // Remove event queue after the window is destroyed.
    remove_event_queue(window_id);

    0
}

/// Set a window property.
///
/// Arguments:
/// - rdi: window ID
/// - rsi: property ID
/// - rdx: value (or pointer for string properties)
///
/// Returns: 0 on success, !0 on error (sets errno).
pub fn sys_window_set(window_id: WindowId, prop: u64, value: u64) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    let pid = info.lock().pid;

    // For TITLE_PTR: read user memory and allocate BEFORE taking the write lock,
    // so we don't do slow user reads or heap allocation under spinlock.
    let mut title_string: Option<String> = None;
    if prop == property::TITLE_PTR {
        let ptr = value as *const u8;
        if ptr.is_null() {
            title_string = Some(String::new());
        } else {
            let mut title_bytes = [0u8; 256];
            for (i, byte) in title_bytes.iter_mut().enumerate() {
                if let Some(b) = unsafe { try_read_user(ptr.add(i)) } {
                    if b == 0 {
                        break;
                    }
                    *byte = b;
                } else {
                    info.lock().errno = Errno::EFAULT;
                    return !0u64;
                }
            }
            let len = title_bytes.iter().position(|&b| b == 0).unwrap_or(256);
            title_string = Some(String::from_utf8_lossy(&title_bytes[..len]).into_owned());
        }
    }

    let mut registry = WINDOW_REGISTRY.write();

    // Check window exists
    let window = match registry.get_window_mut(window_id) {
        Some(w) => w,
        None => {
            info.lock().errno = Errno::ENOENT;
            return !0u64;
        }
    };

    // X, Y, WIDTH, HEIGHT can be set by any process (for window manager)
    // Other properties require ownership
    let requires_ownership = !matches!(
        prop,
        property::X | property::Y | property::WIDTH | property::HEIGHT
    );

    if requires_ownership && window.pid != pid {
        info.lock().errno = Errno::EPERM;
        return !0u64;
    }

    match prop {
        property::VISIBLE => {
            window.visible = value != 0;
        }
        property::X => {
            window.x = value as i32;
        }
        property::Y => {
            window.y = value as i32;
        }
        property::WIDTH => {
            if value == 0 || value > 16384 {
                info.lock().errno = Errno::EINVAL;
                return !0u64;
            }
            window.width = value as u32;
        }
        property::HEIGHT => {
            if value == 0 || value > 16384 {
                info.lock().errno = Errno::EINVAL;
                return !0u64;
            }
            window.height = value as u32;
        }
        property::TITLE_PTR => {
            window.title = title_string.unwrap();
        }
        property::BUFFER_SHM => {
            window.buffer_shm_id = if value == 0 { None } else { Some(value) };
        }
        property::FLAGS => {
            window.flags = value;
        }
        _ => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    }

    0
}

/// Get a window property.
///
/// Arguments:
/// - rdi: window ID
/// - rsi: property ID
///
/// Returns: property value on success, !0 on error (sets errno).
pub fn sys_window_get(window_id: WindowId, prop: u64) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    let registry = WINDOW_REGISTRY.read();

    if let Some(window) = registry.get_window(window_id) {
        match prop {
            property::VISIBLE => window.visible as u64,
            property::X => window.x as u64,
            property::Y => window.y as u64,
            property::WIDTH => window.width as u64,
            property::HEIGHT => window.height as u64,
            property::BUFFER_SHM => window.buffer_shm_id.unwrap_or(0),
            property::FLAGS => window.flags,
            property::TITLE_PTR => {
                // Can't return a string through u64; titles are available
                // via the WindowListEntry.title field in sys_window_list.
                info.lock().errno = Errno::EINVAL;
                !0u64
            }
            _ => {
                info.lock().errno = Errno::EINVAL;
                !0u64
            }
        }
    } else {
        info.lock().errno = Errno::ENOENT;
        !0u64
    }
}

/// Poll events for a window.
///
/// Arguments:
/// - rdi: window ID
/// - rsi: pointer to event buffer (array of WindowEvent)
/// - rdx: maximum number of events
///
/// Returns: number of events copied, or !0 on error (sets errno).
pub fn sys_window_poll(window_id: WindowId, events_ptr: *mut WindowEvent, max: u64) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    let pid = info.lock().pid;

    // Check ownership
    {
        let registry = WINDOW_REGISTRY.read();
        if let Some(window) = registry.get_window(window_id) {
            if window.pid != pid {
                info.lock().errno = Errno::EPERM;
                return !0u64;
            }
        } else {
            info.lock().errno = Errno::ENOENT;
            return !0u64;
        }
    }

    if events_ptr.is_null() || max == 0 {
        return 0;
    }

    let events = poll_events(window_id, max as usize);
    let count = events.len();

    if count > 0 {
        let event_size = core::mem::size_of::<WindowEvent>();
        let bytes_to_copy = count * event_size;

        if !unsafe {
            try_copy_to_user(
                events_ptr as *mut u8,
                events.as_ptr() as *const u8,
                bytes_to_copy,
            )
        } {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
    }

    count as u64
}

/// List all windows (for compositor use).
///
/// Arguments:
/// - rdi: pointer to buffer (array of WindowListEntry)
/// - rsi: maximum number of entries
///
/// Returns: number of windows, or !0 on error (sets errno).
///
/// The buffer receives WindowListEntry structs:
/// ```
/// struct WindowListEntry {
///     id: u64,
///     pid: u64,
///     x: i32,
///     y: i32,
///     width: u32,
///     height: u32,
///     z_order: u32,
///     visible: u32,
///     buffer_shm_id: u64,
/// }
/// ```
pub fn sys_window_list(buffer_ptr: *mut u8, max: u64) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    let registry = WINDOW_REGISTRY.read();
    let windows = registry.visible_windows_sorted();
    let total_count = windows.len();

    if buffer_ptr.is_null() || max == 0 {
        // Just return the count
        return total_count as u64;
    }

    let copy_count = total_count.min(max as usize);
    let entry_size = core::mem::size_of::<WindowListEntry>();

    for (i, window) in windows.iter().take(copy_count).enumerate() {
        let offset = i * entry_size;

        // Build entry inline
        let mut title = [0u8; TITLE_MAX];
        let title_bytes = window.title.as_bytes();
        let copy_len = title_bytes.len().min(TITLE_MAX - 1);
        title[..copy_len].copy_from_slice(&title_bytes[..copy_len]);

        let entry = WindowListEntry {
            id: window.id,
            pid: window.pid,
            x: window.x,
            y: window.y,
            width: window.width,
            height: window.height,
            z_order: window.z_order,
            visible: window.visible as u32,
            buffer_shm_id: window.buffer_shm_id.unwrap_or(0),
            flags: window.flags,
            title,
        };

        let entry_bytes = unsafe {
            core::slice::from_raw_parts(&entry as *const WindowListEntry as *const u8, entry_size)
        };

        if !unsafe { try_copy_to_user(buffer_ptr.add(offset), entry_bytes.as_ptr(), entry_size) } {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
    }

    total_count as u64
}

/// Maximum title length in WindowListEntry (including null terminator).
pub const TITLE_MAX: usize = 64;

/// Entry in the window list returned by sys_window_list.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
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

/// Send an event to a window.
///
/// This allows the window manager to send events (like CloseRequested)
/// to windows it doesn't own.
///
/// Arguments:
/// - rdi: window ID
/// - rsi: pointer to WindowEvent
///
/// Returns: 0 on success, !0 on error (sets errno).
pub fn sys_window_send_event(window_id: WindowId, event_ptr: *const WindowEvent) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if event_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    // Check that the target window exists
    // NOTE: no ownership check -- the WM and taskbar send events to windows they
    // don't own. Requires a WM privilege system to restrict properly (see M4).
    {
        let registry = WINDOW_REGISTRY.read();
        if registry.get_window(window_id).is_none() {
            info.lock().errno = Errno::ENOENT;
            return !0u64;
        }
    }

    // Read the event from user space
    let event: WindowEvent = match unsafe { try_read_user(event_ptr) } {
        Some(e) => e,
        None => {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
    };

    // Update kernel focus state to match WM focus decisions.
    if event.event_type == crate::window::input::WindowEventType::FocusGained as u32 {
        let mut registry = WINDOW_REGISTRY.write();
        registry.set_focused(window_id);
    } else if event.event_type == crate::window::input::WindowEventType::FocusLost as u32 {
        let mut registry = WINDOW_REGISTRY.write();
        if registry.focused_window() == Some(window_id) {
            registry.clear_focus();
        }
    }

    // Send the event to the window's event queue
    crate::window::input::send_event(window_id, event);

    0
}
