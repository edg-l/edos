//! Window syscalls for the window server.

use crate::debug::lock_order::RANK_WINDOW_REGISTRY;
use crate::ranked_write;
use alloc::string::String;

use crate::{
    util::uaccess::{access_ok, try_copy_from_user, try_copy_to_user, try_read_user},
    window::{
        WindowEvent, clipboard, grab,
        input::{
            frame_seq, get_or_create_event_queue, poll_events, present_frame, remove_event_queue,
            send_event,
        },
        registry::{DamageBox, Frame, ReadSite, WINDOW_REGISTRY, WindowId, property, read_tracked},
        shell,
    },
};

use super::Errno;
use crate::thread::scheduler::current_thread_info;
use core::time::Duration;

/// Create a window at `(x, y)` and answer with its id.
pub fn sys_window_create(x: i64, y: i64, width: u64, height: u64) -> Result<u64, Errno> {
    let info = current_thread_info();
    // Validate dimensions
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        return Err(Errno::EINVAL);
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

    Ok(window_id)
}

/// Destroy a window.
pub fn sys_window_destroy(window_id: WindowId) -> Result<u64, Errno> {
    let info = current_thread_info();
    let pid = info.lock().pid;

    // Check ownership and destroy in a single write lock to avoid TOCTOU.
    let refocused = {
        let mut registry =
            ranked_write!(RANK_WINDOW_REGISTRY, "sys_window_destroy", WINDOW_REGISTRY);
        if let Some(window) = registry.get_window(window_id) {
            if window.pid != pid {
                return Err(Errno::EPERM);
            }
        } else {
            return Err(Errno::ENOENT);
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

    Ok(0)
}

/// Set a window property. `value` is a pointer for the string properties.
pub fn sys_window_set(window_id: WindowId, prop: u64, value: u64) -> Result<u64, Errno> {
    let info = current_thread_info();
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
                    return Err(Errno::EFAULT);
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
            return Err(Errno::ENOENT);
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
        return Err(Errno::EPERM);
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
                return Err(Errno::EINVAL);
            }
            window.width = value as u32;
        }
        property::HEIGHT => {
            if value == 0 || value > 16384 {
                return Err(Errno::EINVAL);
            }
            window.height = value as u32;
        }
        property::TITLE_PTR => {
            window.title = title_string.unwrap();
        }
        property::BUFFER_SHM => {
            window.buffer_shm_id = if value == 0 { None } else { Some(value) };
        }
        property::BUFFER_SIZE => {
            window.buffer_size = ((value >> 32) as u32, value as u32);
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
                return Err(Errno::EINVAL);
            };
            window.frame = frame;
        }
        _ => {
            return Err(Errno::EINVAL);
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

    Ok(0)
}

/// Read a window property.
pub fn sys_window_get(window_id: WindowId, prop: u64) -> Result<u64, Errno> {
    let registry = read_tracked(ReadSite::SysWindowGet);

    let window = registry.get_window(window_id).ok_or(Errno::ENOENT)?;
    match prop {
        property::VISIBLE => Ok(window.visible as u64),
        property::X => Ok(window.x as u64),
        property::Y => Ok(window.y as u64),
        property::WIDTH => Ok(window.width as u64),
        property::HEIGHT => Ok(window.height as u64),
        property::BUFFER_SHM => Ok(window.buffer_shm_id.unwrap_or(0)),
        property::FLAGS => Ok(window.flags),
        property::MINIMIZED => Ok(window.minimized as u64),
        property::FRAME => Ok(window.frame.packed()),
        // A title is a string, so it cannot come back through this call at
        // all; it is the `title` field of a `sys_window_list` entry.
        property::TITLE_PTR => Err(Errno::EINVAL),
        _ => Err(Errno::EINVAL),
    }
}

/// Copy up to `max` pending events for a window and answer with how many.
pub fn sys_window_poll(
    window_id: WindowId,
    events_ptr: *mut WindowEvent,
    max: u64,
) -> Result<u64, Errno> {
    let info = current_thread_info();
    let pid = info.lock().pid;

    // Check ownership
    {
        let registry = read_tracked(ReadSite::SysWindowPoll);
        if let Some(window) = registry.get_window(window_id) {
            if window.pid != pid {
                return Err(Errno::EPERM);
            }
        } else {
            return Err(Errno::ENOENT);
        }
    }

    if events_ptr.is_null() || max == 0 {
        return Ok(0);
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
            return Err(Errno::EFAULT);
        }
    }

    Ok(count as u64)
}

/// List all windows (for compositor use).
///
/// `WINDOW_LIST_CONSUME_DAMAGE` in `flags` clears each listed window's damage
/// as it is read, so the caller that acts on it is the one that takes it.
///
/// Answers with the number of windows, which may exceed `max`, so a caller can
/// size its buffer with a null first call. The buffer receives
/// `WindowListEntry` structs:
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
pub fn sys_window_list(buffer_ptr: *mut u8, max: u64, flags: u64) -> Result<u64, Errno> {
    // `max` is the caller's claim about how many entries its buffer holds.
    // Refuse one that no user buffer could satisfy, rather than trusting it
    // because the window list happened to be shorter. Checked before the
    // registry lock so the error path does not run under it.
    if !buffer_ptr.is_null() && max != 0 {
        let declared_bytes = (max as usize).checked_mul(size_of::<WindowListEntry>());
        if !declared_bytes.is_some_and(|bytes| access_ok(buffer_ptr as u64, bytes)) {
            return Err(Errno::EFAULT);
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
            return Ok(total_count as u64);
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
                    buffer_width: window.buffer_size.0,
                    buffer_height: window.buffer_size.1,
                    flags: window.flags,
                    frame: window.frame.packed(),
                    damage_seq: window.damage_seq,
                    damage_x: window.damage_box.map_or(0, |d| d.x),
                    damage_y: window.damage_box.map_or(0, |d| d.y),
                    damage_w: window.damage_box.map_or(0, |d| d.w),
                    damage_h: window.damage_box.map_or(0, |d| d.h),
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
            return Err(Errno::EFAULT);
        }
    }

    if flags & WINDOW_LIST_CONSUME_DAMAGE != 0 {
        let listed: alloc::vec::Vec<u64> = entries.iter().map(|e| e.id).collect();
        let mut registry = ranked_write!(RANK_WINDOW_REGISTRY, "sys_window_list", WINDOW_REGISTRY);
        for id in listed {
            if let Some(w) = registry.get_window_mut(id) {
                w.damage_box = None;
            }
        }
    }

    Ok(total_count as u64)
}

/// Passed by the one caller that acts on damage regions -- the compositor --
/// to take them. Every other reader leaves them alone.
///
/// This is the same coupling the old code had, where reporting the list
/// cleared each damage flag, but stated in the call instead of hidden in a
/// side effect. Hidden, it meant the panel polling the same list could swallow
/// a repaint the compositor had not seen yet.
pub const WINDOW_LIST_CONSUME_DAMAGE: u64 = 1;

// The shape the client reads back, so neither side owns it: a field added here
// alone is read at the wrong offset there, with nothing to say so at compile
// time. It lives in `window-abi`.
pub use window_abi::{TITLE_MAX, WindowListEntry};

/// Send an event to a window.
///
/// This allows the window manager to send events (like CloseRequested)
/// to windows it doesn't own.
pub fn sys_window_send_event(
    window_id: WindowId,
    event_ptr: *const WindowEvent,
) -> Result<u64, Errno> {
    let info = current_thread_info();
    if event_ptr.is_null() {
        return Err(Errno::EFAULT);
    }

    // A window's own process may post to itself; anyone else needs the shell
    // privilege, because this is how focus is moved and how a close request is
    // delivered, and neither should be available to any process that asks.
    {
        let pid = info.lock().pid;
        let caller_is_shell = shell::is_shell(pid);
        let registry = read_tracked(ReadSite::SysWindowSendEvent);
        let Some(window) = registry.get_window(window_id) else {
            return Err(Errno::ENOENT);
        };
        if window.pid != pid && !caller_is_shell {
            return Err(Errno::EPERM);
        }
    }

    // Read the event from user space
    let event: WindowEvent = match unsafe { try_read_user(event_ptr) } {
        Some(e) => e,
        None => {
            return Err(Errno::EFAULT);
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

    Ok(0)
}

/// Appoint another process as part of the shell, so it may manage windows it
/// does not own.
///
///
/// Only a process that already holds the privilege may grant it, and the
/// kernel seeds exactly one holder: `bin/edos-init`, the only process it
/// starts. What a session consists of is init's policy, so this is init's call
/// to make rather than a race between whoever starts first.
pub fn sys_window_grant_shell(target_pid: u64) -> Result<u64, Errno> {
    let info = current_thread_info();
    let pid = info.lock().pid;

    if !shell::is_shell(pid) {
        return Err(Errno::EPERM);
    }
    if target_pid == 0 || !shell::grant(target_pid) {
        return Err(Errno::EINVAL);
    }
    Ok(0)
}

/// Claim or release a key chord, so the focused window does not see it.
///
/// `mods` is a mask of `MOD_SHIFT`, `MOD_CTRL` and `MOD_ALT`; a non-zero
/// `claim` takes the chord and a zero one releases it.
///
/// The mask is matched exactly, so Alt+Tab and Ctrl+Alt+Tab are different
/// chords and claiming one leaves the other with the focused window.
///
/// Restricted to the session shell for the same reason window management is:
/// a chord claimed by any process at all would let one program read another's
/// keys by taking them away from it.
pub fn sys_window_grab_key(code: u64, mods: u64, claim: u64) -> Result<u64, Errno> {
    let info = current_thread_info();
    let pid = info.lock().pid;

    if !shell::is_shell(pid) {
        return Err(Errno::EPERM);
    }
    if code > u32::MAX as u64 || mods & !(grab::MOD_MASK as u64) != 0 {
        return Err(Errno::EINVAL);
    }

    let (code, mods) = (code as u32, mods as u32);
    if claim != 0 {
        if !grab::grab(pid, code, mods) {
            return Err(Errno::ENOMEM);
        }
    } else {
        grab::ungrab(pid, code, mods);
    }
    Ok(0)
}

/// Mark a window as damaged (client has repainted its buffer).
///
/// The region is in window coordinates; a zero width or height means the whole
/// window.
pub fn sys_window_damage(
    window_id: WindowId,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<u64, Errno> {
    let info = current_thread_info();
    let pid = info.lock().pid;

    let mut registry = ranked_write!(RANK_WINDOW_REGISTRY, "sys_window_damage", WINDOW_REGISTRY);
    let Some(window) = registry.get_window_mut(window_id) else {
        return Err(Errno::ENOENT);
    };
    // Only the window's own process may declare it repainted. Otherwise any
    // process can make the compositor redraw the screen every frame.
    if window.pid != pid {
        return Err(Errno::EPERM);
    }
    window.damage_seq = window.damage_seq.wrapping_add(1);

    let (win_w, win_h) = (window.width, window.height);
    // A zero-sized region means the whole window, which is what a client that
    // does not track its own damage reports. Everything else is clamped to the
    // window: a region outside it would grow the union without ever describing
    // a pixel the compositor can draw.
    let reported = if w == 0 || h == 0 {
        DamageBox {
            x: 0,
            y: 0,
            w: win_w,
            h: win_h,
        }
    } else {
        let x = x.min(win_w);
        let y = y.min(win_h);
        DamageBox {
            x,
            y,
            w: w.min(win_w - x),
            h: h.min(win_h - y),
        }
    };

    window.damage_box = Some(match window.damage_box {
        Some(existing) => existing.union(reported),
        None => reported,
    });
    Ok(0)
}

/// Bits returned by [`sys_window_wait`].
pub mod wait_reason {
    /// Events are queued for the window.
    pub const EVENTS: u64 = 1;
    /// The compositor presented a frame since the caller last saw one.
    pub const FRAME: u64 = 2;
}

/// Block until the window has something to do.
///
/// `seen_frame` is the frame count the caller last acted on, from a previous
/// return; `timeout_ms` of 0 waits indefinitely.
///
/// Answers with `wait_reason` bits in the low 32 and the current frame count in
/// the high 32, or 0 if the wait timed out with nothing to report.
///
/// A client with no way to block guesses an interval and sleeps: it either
/// wakes with nothing to do or leaves an event sitting for the rest of that
/// interval. `SYS_WINDOW_POLL` cannot block, which is why this exists
/// alongside it rather than replacing it -- the caller still polls to collect
/// the events once this says there are some.
///
/// The caller passes back the frame count rather than the kernel remembering
/// one per window: a window can be waited on from more than one thread, and a
/// count held in the kernel would let whichever called first consume the
/// signal, which is the same defect damage reporting had.
pub fn sys_window_wait(
    window_id: WindowId,
    seen_frame: u64,
    timeout_ms: u64,
) -> Result<u64, Errno> {
    // The queue is created on demand, the same way polling does, so a client
    // may wait before anything has ever been sent to it.
    let queue = get_or_create_event_queue(window_id);

    // Compared in 32 bits because that is how many the return value carries
    // back; a full-width comparison against a truncated argument would report
    // every wait as a new frame once the counter passed 2^32.
    let frames = || frame_seq() & 0xFFFF_FFFF;
    let seen_frame = seen_frame & 0xFFFF_FFFF;

    let ready = || !queue.is_empty() || frames() != seen_frame;
    let timeout = (timeout_ms != 0).then(|| Duration::from_millis(timeout_ms));
    queue.waiters.wait_until_timeout(ready, timeout);

    let mut reason = 0u64;
    if !queue.is_empty() {
        reason |= wait_reason::EVENTS;
    }
    let now = frames();
    if now != seen_frame {
        reason |= wait_reason::FRAME;
    }
    Ok(reason | (now << 32))
}

/// Report that a frame has been put on the display.
///
/// Takes no arguments and always succeeds. Called by whatever owns the screen,
/// once per presented frame; it wakes every client blocked in
/// [`sys_window_wait`] so they can draw into the frame after this one instead
/// of guessing when it happened.
///
/// Deliberately not a side effect of consuming damage: that happens before the
/// frame is drawn, and a client woken then would be racing the compositor it
/// is trying to keep step with.
pub fn sys_window_present() -> Result<u64, Errno> {
    present_frame();
    Ok(0)
}

/// Replace the contents of a clipboard buffer.
///
/// `which` is 0 for the clipboard and 1 for the primary selection.
pub fn sys_clipboard_set(which: u64, buffer_ptr: *const u8, len: usize) -> Result<u64, Errno> {
    if !clipboard::is_valid(which) {
        return Err(Errno::EINVAL);
    }
    if len > clipboard::MAX_LEN {
        return Err(Errno::EINVAL);
    }

    // An empty set clears the buffer, and takes no user pointer with it.
    let mut bytes = alloc::vec![0u8; len];
    if len != 0 {
        if buffer_ptr.is_null() || !access_ok(buffer_ptr as u64, len) {
            return Err(Errno::EFAULT);
        }
        // Copied before the lock is taken: a copy from a user pointer can
        // demand-fault, and parking on a page fill under a spin lock leaves
        // every other CPU spinning on it for the duration of the I/O.
        if !unsafe { try_copy_from_user(bytes.as_mut_ptr(), buffer_ptr, len) } {
            return Err(Errno::EFAULT);
        }
    }

    clipboard::set(which, bytes);
    Ok(0)
}

/// Read a clipboard buffer.
///
/// `which` is 0 for the clipboard and 1 for the primary selection. A null
/// destination asks only for the length.
///
/// Answers with the full length of the buffer, which may exceed what was
/// copied, so a caller can size its destination with a null first call.
pub fn sys_clipboard_get(which: u64, buffer_ptr: *mut u8, len: usize) -> Result<u64, Errno> {
    if !clipboard::is_valid(which) {
        return Err(Errno::EINVAL);
    }
    if !buffer_ptr.is_null() && len != 0 && !access_ok(buffer_ptr as u64, len) {
        return Err(Errno::EFAULT);
    }

    let bytes = clipboard::get(which);
    let copy_len = bytes.len().min(len);
    if !buffer_ptr.is_null() && copy_len != 0 {
        // Outside the clipboard lock, which `clipboard::get` has already
        // released, for the reason `sys_window_list` copies outside the
        // registry lock.
        if !unsafe { try_copy_to_user(buffer_ptr, bytes.as_ptr(), copy_len) } {
            return Err(Errno::EFAULT);
        }
    }
    Ok(bytes.len() as u64)
}
