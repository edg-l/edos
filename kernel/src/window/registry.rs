//! Window registry and types for the window server.

use crate::thread::preempt::{PreemptReadGuard, PreemptRwLock};
use alloc::{collections::btree_map::BTreeMap, string::String, vec::Vec};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Unique identifier for a window.
pub type WindowId = u64;

/// Counter for generating unique window IDs.
static NEXT_WINDOW_ID: AtomicU64 = AtomicU64::new(1);

/// Counter for z-order assignment.
static Z_ORDER_COUNTER: AtomicU32 = AtomicU32::new(1);

/// Information about a window.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    /// Unique window ID.
    pub id: WindowId,
    /// Process ID that owns this window.
    pub pid: u64,
    /// X position on screen.
    pub x: i32,
    /// Y position on screen.
    pub y: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Z-order (higher = on top).
    pub z_order: u32,
    /// Whether the window is visible.
    pub visible: bool,
    /// Window title.
    pub title: String,
    /// Shared memory ID for the window buffer, if any.
    pub buffer_shm_id: Option<u64>,
    /// Window flags (e.g., FLAG_DOCK for no decorations).
    pub flags: u64,
    /// Damage flag: set by client via SYS_WINDOW_DAMAGE, cleared by WM via window_list.
    pub damaged: bool,
}

#[allow(dead_code)]
impl WindowInfo {
    /// Create a new window with the given parameters.
    pub fn new(pid: u64, x: i32, y: i32, width: u32, height: u32) -> Self {
        let id = NEXT_WINDOW_ID.fetch_add(1, Ordering::Relaxed);
        let z_order = Z_ORDER_COUNTER.fetch_add(1, Ordering::Relaxed);

        Self {
            id,
            pid,
            x,
            y,
            width,
            height,
            z_order,
            visible: true,
            title: String::new(),
            buffer_shm_id: None,
            flags: 0,
            damaged: true, // newly created windows are damaged
        }
    }

    /// Check if a point is inside this window (client area only, excluding decorations).
    pub fn contains(&self, px: i32, py: i32) -> bool {
        self.contains_client(px, py)
    }

    /// Check if a point is within the window's CLIENT area (excluding decorations).
    pub fn contains_client(&self, px: i32, py: i32) -> bool {
        if !self.visible {
            return false;
        }
        let is_dock = (self.flags & flags::FLAG_DOCK) != 0;
        let client_x = if is_dock {
            self.x
        } else {
            self.x + decoration::BORDER_WIDTH
        };
        let client_y = if is_dock {
            self.y
        } else {
            self.y + decoration::TITLE_HEIGHT
        };
        let client_w = self.width as i32;
        let client_h = self.height as i32;

        px >= client_x && px < client_x + client_w && py >= client_y && py < client_y + client_h
    }

    /// Check if point is within decorated bounds (for finding windows).
    pub fn contains_decorated(&self, px: i32, py: i32) -> bool {
        if !self.visible {
            return false;
        }
        let is_dock = (self.flags & flags::FLAG_DOCK) != 0;
        let (total_w, total_h) = if is_dock {
            (self.width as i32, self.height as i32)
        } else {
            (
                self.width as i32 + decoration::BORDER_WIDTH * 2,
                self.height as i32 + decoration::TITLE_HEIGHT + decoration::BORDER_WIDTH,
            )
        };

        px >= self.x && px < self.x + total_w && py >= self.y && py < self.y + total_h
    }
}

/// Window property constants for sys_window_set/get.
pub mod property {
    pub const VISIBLE: u64 = 1;
    pub const X: u64 = 2;
    pub const Y: u64 = 3;
    pub const WIDTH: u64 = 4;
    pub const HEIGHT: u64 = 5;
    pub const TITLE_PTR: u64 = 6;
    pub const BUFFER_SHM: u64 = 7;
    pub const FLAGS: u64 = 8;
}

/// Window flags.
pub mod flags {
    /// Dock window: no decorations, not draggable.
    pub const FLAG_DOCK: u64 = 1;
}

/// Window decoration constants (must match WM).
pub mod decoration {
    pub const TITLE_HEIGHT: i32 = 24;
    pub const BORDER_WIDTH: i32 = 1;
}

/// Global window registry.
pub struct WindowRegistry {
    /// All windows indexed by ID.
    windows: BTreeMap<WindowId, WindowInfo>,
    /// Currently focused window.
    focused_window: Option<WindowId>,
}

#[allow(dead_code)]
impl WindowRegistry {
    /// Create a new empty registry.
    pub const fn new() -> Self {
        Self {
            windows: BTreeMap::new(),
            focused_window: None,
        }
    }

    /// Create a new window for the given process.
    pub fn create_window(&mut self, pid: u64, x: i32, y: i32, width: u32, height: u32) -> WindowId {
        let window = WindowInfo::new(pid, x, y, width, height);
        let id = window.id;

        // Auto-focus new window
        self.focused_window = Some(id);

        self.windows.insert(id, window);
        id
    }

    /// Destroy a window.
    pub fn destroy_window(&mut self, id: WindowId) -> bool {
        if self.windows.remove(&id).is_some() {
            // Clear focus if this was the focused window
            if self.focused_window == Some(id) {
                // Find the next window with highest z-order
                self.focused_window = self
                    .windows
                    .values()
                    .filter(|w| w.visible)
                    .max_by_key(|w| w.z_order)
                    .map(|w| w.id);
            }
            true
        } else {
            false
        }
    }

    /// Get a reference to a window.
    pub fn get_window(&self, id: WindowId) -> Option<&WindowInfo> {
        self.windows.get(&id)
    }

    /// Get a mutable reference to a window.
    pub fn get_window_mut(&mut self, id: WindowId) -> Option<&mut WindowInfo> {
        self.windows.get_mut(&id)
    }

    /// Set the focused window and bring it to the top.
    pub fn set_focused(&mut self, id: WindowId) -> bool {
        if let Some(window) = self.windows.get_mut(&id) {
            // Bring to top with new z-order
            window.z_order = Z_ORDER_COUNTER.fetch_add(1, Ordering::Relaxed);
            self.focused_window = Some(id);
            true
        } else {
            false
        }
    }

    /// Clear focus (no window focused).
    pub fn clear_focus(&mut self) {
        self.focused_window = None;
    }

    /// Get the currently focused window ID.
    pub fn focused_window(&self) -> Option<WindowId> {
        self.focused_window
    }

    /// Find the topmost window at the given coordinates (checks decorated bounds).
    pub fn window_at(&self, x: i32, y: i32) -> Option<WindowId> {
        self.windows
            .values()
            .filter(|w| w.contains_decorated(x, y))
            .max_by_key(|w| w.z_order)
            .map(|w| w.id)
    }

    /// Find window whose CLIENT area contains the point (for routing input).
    pub fn window_at_client(&self, x: i32, y: i32) -> Option<WindowId> {
        self.windows
            .values()
            .filter(|w| w.contains_client(x, y))
            .max_by_key(|w| w.z_order)
            .map(|w| w.id)
    }

    /// Get all visible windows sorted by z-order (back to front).
    pub fn visible_windows_sorted(&self) -> Vec<&WindowInfo> {
        let mut windows: Vec<&WindowInfo> = self.windows.values().filter(|w| w.visible).collect();
        windows.sort_by_key(|w| w.z_order);
        windows
    }

    /// Get all windows owned by a process.
    pub fn windows_for_pid(&self, pid: u64) -> Vec<WindowId> {
        self.windows
            .values()
            .filter(|w| w.pid == pid)
            .map(|w| w.id)
            .collect()
    }

    /// Get a list of all window IDs.
    pub fn all_window_ids(&self) -> Vec<WindowId> {
        self.windows.keys().copied().collect()
    }

    /// Destroy all windows owned by a process.
    pub fn destroy_windows_for_pid(&mut self, pid: u64) {
        let window_ids: Vec<WindowId> = self.windows_for_pid(pid);
        for id in window_ids {
            self.destroy_window(id);
        }
    }
}

/// Global window registry instance.
pub static WINDOW_REGISTRY: PreemptRwLock<WindowRegistry> =
    PreemptRwLock::new(WindowRegistry::new());

/// Identifies a `read_tracked` call site in the reader table.
///
/// Values are stable so an external debugger can decode a stuck slot without
/// consulting the source; append rather than renumber.
#[derive(Clone, Copy)]
#[repr(u8)]
pub enum ReadSite {
    CleanupProcessWindows = 1,
    HandleMouseEvent = 2,
    HandleKeyboardEvent = 3,
    SysWindowGet = 4,
    SysWindowPoll = 5,
    SysWindowList = 6,
    SysWindowSendEvent = 7,
}

/// Forensic table of live readers of `WINDOW_REGISTRY`.
///
/// A reader that parks without releasing leaves its slot occupied forever,
/// which is exactly the state that wedges every writer. Reading this table from
/// outside the guest names the thread:
///
/// ```text
/// nm -C kernel | grep WINDOW_REGISTRY_READERS
/// scripts/edos-vm qmp human-monitor-command '{"command-line":"x /8gx <addr>"}'
/// ```
///
/// Each non-zero entry decodes as `(tid << 8) | site`.
///
/// Deliberately silent: no logging, no allocation, no serial output, because
/// the paths involved run at compositor frame rate. Off by default so the same
/// build can be run with and without it; instrumentation perturbs timing, and a
/// race that only appears in one configuration is itself a finding.
#[cfg(feature = "window-lock-debug")]
pub static WINDOW_REGISTRY_READERS: [AtomicU64; READER_SLOTS] =
    [const { AtomicU64::new(0) }; READER_SLOTS];

/// Number of concurrent readers the table can name. Beyond this, acquisitions
/// still succeed but go unrecorded and bump `WINDOW_REGISTRY_READER_OVERFLOW`.
#[cfg(feature = "window-lock-debug")]
pub const READER_SLOTS: usize = 8;

/// Count of acquisitions that found no free slot.
#[cfg(feature = "window-lock-debug")]
pub static WINDOW_REGISTRY_READER_OVERFLOW: AtomicU64 = AtomicU64::new(0);

/// Total tracked acquisitions since boot.
///
/// Live slots are held for microseconds, so sampling the table from outside
/// almost never catches one. This counter is the positive control: if it is
/// advancing, the table is being maintained and an all-zero table means "no
/// reader is stuck", not "instrumentation is broken".
#[cfg(feature = "window-lock-debug")]
pub static WINDOW_REGISTRY_READER_ACQUIRES: AtomicU64 = AtomicU64::new(0);

/// A read guard that records its holder while live.
pub struct TrackedReadGuard<'a> {
    guard: PreemptReadGuard<'a, WindowRegistry>,
    #[cfg(feature = "window-lock-debug")]
    slot: Option<usize>,
}

impl core::ops::Deref for TrackedReadGuard<'_> {
    type Target = WindowRegistry;

    fn deref(&self) -> &WindowRegistry {
        &self.guard
    }
}

#[cfg(feature = "window-lock-debug")]
impl Drop for TrackedReadGuard<'_> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            WINDOW_REGISTRY_READERS[slot].store(0, Ordering::Release);
        }
    }
}

/// Acquire `WINDOW_REGISTRY` for reading, recording the holder.
///
/// Without the `window-lock-debug` feature this is exactly `.read()`.
pub fn read_tracked(site: ReadSite) -> TrackedReadGuard<'static> {
    let guard = WINDOW_REGISTRY.read();

    #[cfg(feature = "window-lock-debug")]
    {
        let tid = crate::thread::scheduler::current_thread_id().map_or(0, |t| t.0);
        let tag = (tid << 8) | site as u64;

        WINDOW_REGISTRY_READER_ACQUIRES.fetch_add(1, Ordering::Relaxed);

        let mut slot = None;
        for (i, entry) in WINDOW_REGISTRY_READERS.iter().enumerate() {
            if entry
                .compare_exchange(0, tag, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                slot = Some(i);
                break;
            }
        }
        if slot.is_none() {
            WINDOW_REGISTRY_READER_OVERFLOW.fetch_add(1, Ordering::Relaxed);
        }
        return TrackedReadGuard { guard, slot };
    }

    #[cfg(not(feature = "window-lock-debug"))]
    {
        let _ = site;
        TrackedReadGuard { guard }
    }
}
