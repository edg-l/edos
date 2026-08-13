//! Window syscalls for the window server.

use crate::debug::lock_order::RANK_WINDOW_REGISTRY;
use crate::ranked_write;
use alloc::string::String;

use crate::{
    util::uaccess::{access_ok, try_copy_from_user, try_copy_to_user, try_read_user},
    window::{
        WindowEvent, clipboard,
        input::{get_or_create_event_queue, poll_events, remove_event_queue, send_event},
        registry::{Frame, ReadSite, WINDOW_REGISTRY, WindowId, property, read_tracked},
        shell,
    },
};

use super::Errno;
use crate::thread::scheduler::current_thread_info;

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
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    // Validate dimensions
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    }

    let pid = info.lock().pid;

    // Create the window
    let (window_id, unfocused) = ranked_write!(
        RANK_WINDOW_REGISTRY,
        "sys_window_create",
        WINDOW_REGISTRY
    )
    .create_window(pid, x as i32, y as i32, width as u32, height as u32);

    // Create event queue for the window
    get_or_create_event_queue(window_id);

    // Outside the registry lock: a new window takes focus, and the window that
    // had it has to be told, or it keeps painting a focused caret.
    if let Some(previous) = unfocused {
        send_event(previous, WindowEvent::focus_lost());
    }
    send_event(window_id, WindowEvent::focus_gained());

    window_id
}

/// Destroy a window.
///
/// Arguments:
/// - rdi: window ID
///
/// Returns: 0 on success, !0 on error (sets errno).
pub fn sys_window_destroy(window_id: WindowId) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let pid = info.lock().pid;

    // Check ownership and destroy in a single write lock to avoid TOCTOU.
    let refocused = {
        let mut registry =
            ranked_write!(RANK_WINDOW_REGISTRY, "sys_window_destroy", WINDOW_REGISTRY);
        if let Some(window) = registry.get_window(window_id) {
            if window.pid != pid {
                info.lock().errno = Errno::EPERM;
                return !0u64;
            }
        } else {
            info.lock().errno = Errno::ENOENT;
            return !0u64;
        }
        registry.destroy_window(window_id)
    };

    // Remove event queue after the window is destroyed.
    remove_event_queue(window_id);

    // Outside the registry lock: whoever inherits focus has to be told, or it
    // goes on treating the keyboard as somebody else's.
    if let Some(new_focus) = refocused {
        send_event(new_focus, WindowEvent::focus_gained());
    }

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
    let info = current_thread_info();
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

    // Settled before the registry is touched: the two locks must not be
    // co-held, and this answer does not depend on the window.
    let caller_is_shell = shell::is_shell(pid);

    let mut registry = ranked_write!(RANK_WINDOW_REGISTRY, "sys_window_set", WINDOW_REGISTRY);

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
    // Position, size, frame and minimized state are management operations: a
    // compositor and a panel perform them on windows belonging to other
    // processes, so ownership cannot be the test. The shell privilege is,
    // which init hands out (see `window::shell`).
    let management = matches!(
        prop,
        property::X
            | property::Y
            | property::WIDTH
            | property::HEIGHT
            | property::FRAME
            | property::MINIMIZED
    );

    if window.pid != pid && !(management && caller_is_shell) {
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
        property::MINIMIZED => {
            // Handled below: it moves focus, which needs the whole registry
            // rather than the one window borrowed here.
        }
        property::FRAME => {
            let Some(frame) = Frame::from_packed(value) else {
                info.lock().errno = Errno::EINVAL;
                return !0u64;
            };
            window.frame = frame;
        }
        _ => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    }

    let refocused = match prop {
        property::FLAGS => registry.release_dock_focus(window_id),
        property::MINIMIZED => registry.set_minimized(window_id, value != 0),
        _ => None,
    };
    drop(registry);

    if let Some(new_focus) = refocused
        && new_focus != window_id
    {
        send_event(window_id, WindowEvent::focus_lost());
        send_event(new_focus, WindowEvent::focus_gained());
    } else if refocused == Some(window_id) {
        send_event(window_id, WindowEvent::focus_gained());
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
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let registry = read_tracked(ReadSite::SysWindowGet);

    if let Some(window) = registry.get_window(window_id) {
        match prop {
            property::VISIBLE => window.visible as u64,
            property::X => window.x as u64,
            property::Y => window.y as u64,
            property::WIDTH => window.width as u64,
            property::HEIGHT => window.height as u64,
            property::BUFFER_SHM => window.buffer_shm_id.unwrap_or(0),
            property::FLAGS => window.flags,
            property::MINIMIZED => window.minimized as u64,
            property::FRAME => window.frame.packed(),
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
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let pid = info.lock().pid;

    // Check ownership
    {
        let registry = read_tracked(ReadSite::SysWindowPoll);
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
///     flags: u64,
///     frame: u64,
///     damaged: u32,
///     focused: u32,
///     minimized: u32,
///     title: [u8; TITLE_MAX],
/// }
/// ```
pub fn sys_window_list(buffer_ptr: *mut u8, max: u64) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    // `max` is the caller's claim about how many entries its buffer holds.
    // Refuse one that no user buffer could satisfy, rather than trusting it
    // because the window list happened to be shorter. Checked before the
    // registry lock so the error path does not run under it.
    if !buffer_ptr.is_null() && max != 0 {
        let declared_bytes = (max as usize).checked_mul(size_of::<WindowListEntry>());
        if !declared_bytes.is_some_and(|bytes| access_ok(buffer_ptr as u64, bytes)) {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
    }

    // Snapshot under the lock, copy to userspace outside it. `try_copy_to_user`
    // can demand-fault, and a demand fault can park on a page fill; parking
    // while holding this spin lock leaves every other CPU spinning on it for
    // the duration of the I/O.
    let (entries, total_count) = {
        let registry = read_tracked(ReadSite::SysWindowList);
        let windows = registry.listed_windows_sorted();
        let focused = registry.focused_window();
        let total_count = windows.len();

        if buffer_ptr.is_null() || max == 0 {
            // Just return the count
            return total_count as u64;
        }

        let copy_count = total_count.min(max as usize);
        let entries: alloc::vec::Vec<WindowListEntry> = windows
            .iter()
            .take(copy_count)
            .map(|window| {
                let mut title = [0u8; TITLE_MAX];
                let title_bytes = window.title.as_bytes();
                let copy_len = title_bytes.len().min(TITLE_MAX - 1);
                title[..copy_len].copy_from_slice(&title_bytes[..copy_len]);

                WindowListEntry {
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
                    frame: window.frame.packed(),
                    damaged: window.damaged as u32,
                    focused: (focused == Some(window.id)) as u32,
                    minimized: window.minimized as u32,
                    title,
                }
            })
            .collect();

        (entries, total_count)
    };

    let entry_size = core::mem::size_of::<WindowListEntry>();
    for (i, entry) in entries.iter().enumerate() {
        let entry_bytes = unsafe {
            core::slice::from_raw_parts(entry as *const WindowListEntry as *const u8, entry_size)
        };

        if !unsafe {
            try_copy_to_user(
                buffer_ptr.add(i * entry_size),
                entry_bytes.as_ptr(),
                entry_size,
            )
        } {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
    }

    let listed_ids: alloc::vec::Vec<u64> = entries.iter().map(|e| e.id).collect();
    {
        let mut registry = ranked_write!(RANK_WINDOW_REGISTRY, "sys_window_list", WINDOW_REGISTRY);
        for id in listed_ids {
            if let Some(w) = registry.get_window_mut(id) {
                w.damaged = false;
            }
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
    /// The frame the window's manager last reported, packed as four u16 edges.
    /// Reported back so a manager can skip rewriting a frame it already set.
    pub frame: u64,
    /// Set when the client has called SYS_WINDOW_DAMAGE since last list query.
    pub damaged: u32,
    /// Set for the window that currently holds input focus. The registry is the
    /// single source of truth: clients must not re-derive focus from `z_order`.
    pub focused: u32,
    /// Set while the window is put away. It stays in this list so the panel can
    /// offer a way back; the compositor skips it.
    pub minimized: u32,
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
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    if event_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    // A window's own process may post to itself; anyone else needs the shell
    // privilege, because this is how focus is moved and how a close request is
    // delivered, and neither should be available to any process that asks.
    {
        let pid = info.lock().pid;
        let caller_is_shell = shell::is_shell(pid);
        let registry = read_tracked(ReadSite::SysWindowSendEvent);
        let Some(window) = registry.get_window(window_id) else {
            info.lock().errno = Errno::ENOENT;
            return !0u64;
        };
        if window.pid != pid && !caller_is_shell {
            info.lock().errno = Errno::EPERM;
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
        let mut registry = ranked_write!(
            RANK_WINDOW_REGISTRY,
            "sys_window_send_event",
            WINDOW_REGISTRY
        );
        registry.set_focused(window_id);
    } else if event.event_type == crate::window::input::WindowEventType::FocusLost as u32 {
        let mut registry = ranked_write!(
            RANK_WINDOW_REGISTRY,
            "sys_window_send_event",
            WINDOW_REGISTRY
        );
        if registry.focused_window() == Some(window_id) {
            registry.clear_focus();
        }
    }

    // Send the event to the window's event queue
    crate::window::input::send_event(window_id, event);

    0
}

/// Appoint another process as part of the shell, so it may manage windows it
/// does not own.
///
/// Arguments:
/// - rdi: pid to appoint
///
/// Returns: 0 on success, !0 on error (sets errno).
///
/// Only a process that already holds the privilege may grant it, and the
/// kernel seeds exactly one holder: `bin/edos-init`, the only process it
/// starts. What a session consists of is init's policy, so this is init's call
/// to make rather than a race between whoever starts first.
pub fn sys_window_grant_shell(target_pid: u64) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;
    let pid = info.lock().pid;

    if !shell::is_shell(pid) {
        info.lock().errno = Errno::EPERM;
        return !0u64;
    }
    if target_pid == 0 || !shell::grant(target_pid) {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    }
    0
}

/// Mark a window as damaged (client has repainted its buffer).
///
/// Arguments:
/// - rdi: window ID
///
/// Returns: 0 on success, !0 on error.
pub fn sys_window_damage(window_id: WindowId) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;
    let pid = info.lock().pid;

    let mut registry = ranked_write!(RANK_WINDOW_REGISTRY, "sys_window_damage", WINDOW_REGISTRY);
    let Some(w) = registry.get_window_mut(window_id) else {
        info.lock().errno = Errno::ENOENT;
        return !0u64;
    };
    // Only the window's own process may declare it repainted. Otherwise any
    // process can make the compositor redraw the screen every frame.
    if w.pid != pid {
        info.lock().errno = Errno::EPERM;
        return !0u64;
    }
    w.damaged = true;
    0
}

/// Replace the contents of a clipboard buffer.
///
/// Arguments:
/// - rdi: which buffer (0 clipboard, 1 primary selection)
/// - rsi: pointer to the bytes
/// - rdx: how many bytes
///
/// Returns: 0 on success, !0 on error (sets errno).
pub fn sys_clipboard_set(which: u64, buffer_ptr: *const u8, len: usize) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    if !clipboard::is_valid(which) {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    }
    if len > clipboard::MAX_LEN {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    }

    // An empty set clears the buffer, and takes no user pointer with it.
    let mut bytes = alloc::vec![0u8; len];
    if len != 0 {
        if buffer_ptr.is_null() || !access_ok(buffer_ptr as u64, len) {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
        // Copied before the lock is taken: a copy from a user pointer can
        // demand-fault, and parking on a page fill under a spin lock leaves
        // every other CPU spinning on it for the duration of the I/O.
        if !unsafe { try_copy_from_user(bytes.as_mut_ptr(), buffer_ptr, len) } {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
    }

    clipboard::set(which, bytes);
    0
}

/// Read a clipboard buffer.
///
/// Arguments:
/// - rdi: which buffer (0 clipboard, 1 primary selection)
/// - rsi: pointer to the destination, or null to ask only for the length
/// - rdx: how many bytes the destination holds
///
/// Returns: the full length of the buffer, which may exceed what was copied,
/// so a caller can size its destination with a null first call. !0 on error.
pub fn sys_clipboard_get(which: u64, buffer_ptr: *mut u8, len: usize) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    if !clipboard::is_valid(which) {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    }
    if !buffer_ptr.is_null() && len != 0 && !access_ok(buffer_ptr as u64, len) {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    let bytes = clipboard::get(which);
    let copy_len = bytes.len().min(len);
    if !buffer_ptr.is_null() && copy_len != 0 {
        // Outside the clipboard lock, which `clipboard::get` has already
        // released, for the reason `sys_window_list` copies outside the
        // registry lock.
        if !unsafe { try_copy_to_user(buffer_ptr, bytes.as_ptr(), copy_len) } {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
    }
    bytes.len() as u64
}
