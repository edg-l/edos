//! Window registry and types for the window server.

use crate::debug::lock_order::RANK_WINDOW_REGISTRY;
use crate::thread::preempt::{PreemptReadGuard, PreemptRwLock};
use alloc::{collections::btree_map::BTreeMap, string::String, vec::Vec};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Unique identifier for a window.
pub type WindowId = u64;

/// Counter for generating unique window IDs.
static NEXT_WINDOW_ID: AtomicU64 = AtomicU64::new(1);

/// Counter for z-order assignment.
static Z_ORDER_COUNTER: AtomicU32 = AtomicU32::new(1);

/// Thickness of a window manager's frame on each side, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Frame {
    /// An undecorated window: the window owns every pixel it was given.
    pub const NONE: Frame = Frame {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };

    /// Unpack the four u16 edges a client packs into one property value.
    /// Returns `None` if any edge is implausible for a frame.
    pub fn from_packed(value: u64) -> Option<Frame> {
        let edge = |shift: u32| ((value >> shift) & 0xFFFF) as i32;
        let frame = Frame {
            left: edge(0),
            top: edge(16),
            right: edge(32),
            bottom: edge(48),
        };
        let sane = |v: i32| (0..=256).contains(&v);
        (sane(frame.left) && sane(frame.top) && sane(frame.right) && sane(frame.bottom))
            .then_some(frame)
    }

    pub fn packed(&self) -> u64 {
        (self.left as u64 & 0xFFFF)
            | (self.top as u64 & 0xFFFF) << 16
            | (self.right as u64 & 0xFFFF) << 32
            | (self.bottom as u64 & 0xFFFF) << 48
    }
}

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
    /// Window flags: see the `flags` module.
    pub flags: u64,
    /// Set while the window is put away. Distinct from `visible`, which is the
    /// client saying "do not map me at all": a minimized window is still one
    /// the user owns and expects to find in the panel, so it stays in the
    /// window list and only drops out of compositing and out of focus.
    pub minimized: bool,
    /// Thickness of the frame the window manager draws around this window, as
    /// (left, top, right, bottom).
    ///
    /// The kernel routes pointer events into client space, so it needs the
    /// offset -- but not the reason for it. Whether that space is a title bar,
    /// a shadow or nothing at all is the window manager's business, and it
    /// reports the result per window through `property::FRAME`. A window
    /// nobody has decorated has no frame, which is the correct answer for the
    /// frames before the manager gets to it.
    pub frame: Frame,
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
            // Created unmapped. A client sets up a window in several calls --
            // buffers, title, flags -- and painting is the last of them, so a
            // window that is visible from `create` is composited with whatever
            // its buffer happened to contain: a black rectangle until the
            // client's first frame lands.
            visible: false,
            title: String::new(),
            buffer_shm_id: None,
            flags: 0,
            minimized: false,
            frame: Frame::NONE,
            damaged: true, // newly created windows are damaged
        }
    }

    /// Whether the window currently occupies screen space: mapped by its
    /// client and not put away by the user.
    pub fn on_screen(&self) -> bool {
        self.visible && !self.minimized
    }

    /// Check if a point is inside this window (client area only, excluding decorations).
    pub fn contains(&self, px: i32, py: i32) -> bool {
        self.contains_client(px, py)
    }

    /// Check if a point is within the window's CLIENT area (excluding decorations).
    pub fn contains_client(&self, px: i32, py: i32) -> bool {
        if !self.on_screen() {
            return false;
        }
        let client_x = self.x + self.frame.left;
        let client_y = self.y + self.frame.top;
        let client_w = self.width as i32;
        let client_h = self.height as i32;

        px >= client_x && px < client_x + client_w && py >= client_y && py < client_y + client_h
    }

    /// Check if point is within decorated bounds (for finding windows).
    pub fn contains_decorated(&self, px: i32, py: i32) -> bool {
        if !self.on_screen() {
            return false;
        }
        let total_w = self.width as i32 + self.frame.left + self.frame.right;
        let total_h = self.height as i32 + self.frame.top + self.frame.bottom;

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
    /// Put the window away, or bring it back. Non-zero minimizes.
    pub const MINIMIZED: u64 = 9;
    /// Thickness of the window manager's frame around this window, packed as
    /// four u16 edges: left, top, right, bottom, low bits first. Set by
    /// whoever decorates the window, so pointer routing follows the frame that
    /// is actually drawn.
    pub const FRAME: u64 = 10;
}

/// Window flags.
pub mod flags {
    /// No title bar or border: the window owns every pixel it was given, and
    /// the compositor neither decorates it nor lets the pointer drag it.
    pub const FLAG_UNDECORATED: u64 = 1;
    /// Never holds keyboard focus. Chrome that paints no focus state, such as
    /// the panel, since input landing there is invisible to the user.
    pub const FLAG_NO_FOCUS: u64 = 2;

    /// A panel: undecorated and never focusable. The kernel never tests this
    /// combined value, it tests the two bits separately, which is the whole
    /// point -- a menu is undecorated but must take focus, or it cannot be
    /// dismissed by focus loss. Kept as the name userspace sets.
    #[allow(dead_code)]
    pub const FLAG_DOCK: u64 = FLAG_UNDECORATED | FLAG_NO_FOCUS;
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

    /// Create a new window for the given process, focusing it.
    ///
    /// Returns the new id and whoever held focus before, so the caller can
    /// deliver `FocusLost` to it outside the registry lock. Without that the
    /// old holder keeps painting a focused caret while its keystrokes go to
    /// the new window.
    pub fn create_window(
        &mut self,
        pid: u64,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> (WindowId, Option<WindowId>) {
        let window = WindowInfo::new(pid, x, y, width, height);
        let id = window.id;

        // Auto-focus the new window. Its flags arrive later, so a window that
        // turns out to be chrome is moved off focus by `release_dock_focus`.
        let previous = self.focused_window.filter(|prev| *prev != id);
        self.focused_window = Some(id);

        self.windows.insert(id, window);
        (id, previous)
    }

    /// Put a window away, or bring it back. Returns the window that should
    /// take focus as a result, if focus has to move.
    ///
    /// Minimizing the focused window hands focus to whatever is left, since a
    /// window the user cannot see must not keep the keyboard.
    pub fn set_minimized(&mut self, id: WindowId, minimized: bool) -> Option<WindowId> {
        let Some(window) = self.windows.get_mut(&id) else {
            return None;
        };
        if window.minimized == minimized {
            return None;
        }
        window.minimized = minimized;

        if minimized {
            if self.focused_window != Some(id) {
                return None;
            }
            self.focused_window = self.topmost_focusable();
            self.focused_window
        } else {
            self.set_focused(id).then_some(id)
        }
    }

    /// Destroy a window.
    ///
    /// Returns the window that inherits keyboard focus, if the destroyed one
    /// held it, so the caller can deliver the focus event outside the registry
    /// lock. Delivering it is not optional: the heir was told it lost focus
    /// when the dying window was created, and click-to-focus sends nothing
    /// when the registry already names the window clicked, so an undelivered
    /// gain leaves it dropping keystrokes for the rest of the session.
    pub fn destroy_window(&mut self, id: WindowId) -> Option<WindowId> {
        if self.windows.remove(&id).is_none() {
            return None;
        }
        if self.focused_window != Some(id) {
            return None;
        }
        self.focused_window = self.topmost_focusable();
        self.focused_window
    }

    /// Get a reference to a window.
    pub fn get_window(&self, id: WindowId) -> Option<&WindowInfo> {
        self.windows.get(&id)
    }

    /// Get a mutable reference to a window.
    pub fn get_window_mut(&mut self, id: WindowId) -> Option<&mut WindowInfo> {
        self.windows.get_mut(&id)
    }

    /// The topmost visible window allowed to hold keyboard focus.
    ///
    /// A dock is chrome: it paints no focus state, so input landing in one is
    /// invisible to the user. Focus never rests there.
    fn topmost_focusable(&self) -> Option<WindowId> {
        self.windows
            .values()
            .filter(|w| w.on_screen() && w.flags & flags::FLAG_NO_FOCUS == 0)
            .max_by_key(|w| w.z_order)
            .map(|w| w.id)
    }

    /// Set the focused window and bring it to the top. Fails for a dock.
    pub fn set_focused(&mut self, id: WindowId) -> bool {
        let Some(window) = self.windows.get_mut(&id) else {
            return false;
        };
        if window.flags & flags::FLAG_NO_FOCUS != 0 {
            return false;
        }
        // Bring to top with new z-order
        window.z_order = Z_ORDER_COUNTER.fetch_add(1, Ordering::Relaxed);
        self.focused_window = Some(id);
        true
    }

    /// Move focus off `id` if it has just declared itself a dock.
    ///
    /// A window is created before its flags are set, so a dock is auto-focused
    /// like any other window and would otherwise keep focus for the rest of the
    /// session. Returns the window that takes over, so the caller can deliver
    /// the focus events outside the registry lock.
    pub fn release_dock_focus(&mut self, id: WindowId) -> Option<WindowId> {
        if self.focused_window != Some(id) {
            return None;
        }
        let is_dock = self
            .windows
            .get(&id)
            .is_some_and(|w| w.flags & flags::FLAG_NO_FOCUS != 0);
        if !is_dock {
            return None;
        }
        self.focused_window = self.topmost_focusable();
        self.focused_window
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

    /// Every window a client has mapped, sorted by z-order, back to front.
    ///
    /// Minimized windows are included: the panel needs them to offer a way
    /// back, and the compositor skips them on the `minimized` field rather
    /// than by their absence.
    pub fn listed_windows_sorted(&self) -> Vec<&WindowInfo> {
        let mut windows: Vec<&WindowInfo> = self.windows.values().filter(|w| w.visible).collect();
        windows.sort_by_key(|w| w.z_order);
        windows
    }

    /// Every window the registry holds, sorted by z-order, back to front.
    ///
    /// Unfiltered, unlike `listed_windows_sorted`: introspection wants the
    /// window a client has created and not yet mapped as much as the ones on
    /// screen, since "created but never shown" is a state worth seeing.
    pub fn all_windows_sorted(&self) -> Vec<&WindowInfo> {
        let mut windows: Vec<&WindowInfo> = self.windows.values().collect();
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
    /// Destroy every window owned by `pid`.
    ///
    /// Returns the window left holding keyboard focus, if focus moved. A
    /// destroy that moves focus onto another window of the same process is
    /// superseded by that window's own destroy, so the last answer is the
    /// surviving one.
    pub fn destroy_windows_for_pid(&mut self, pid: u64) -> Option<WindowId> {
        let window_ids: Vec<WindowId> = self.windows_for_pid(pid);
        let mut refocused = None;
        for id in window_ids {
            if let Some(heir) = self.destroy_window(id) {
                refocused = Some(heir);
            }
        }
        refocused
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
    ProcWindows = 8,
}

/// A copy of the registry, taken under the guard and formatted outside it.
///
/// The guard covers a clone rather than the whole render: `/proc/windows` is
/// read by anything that wants to find a window, and holding the registry
/// across a `String` build would put the compositor's frame behind a formatter.
pub fn snapshot() -> (Vec<WindowInfo>, Option<WindowId>) {
    let registry = read_tracked(ReadSite::ProcWindows);
    let windows = registry
        .all_windows_sorted()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let focused = registry.focused_window();
    (windows, focused)
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

/// Rank-stack label for every `read_tracked` acquisition. `ReadSite` names the
/// call site for the reader table; the rank system only needs one label.
const READ_SITE: &str = "window::read_tracked";

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

impl Drop for TrackedReadGuard<'_> {
    fn drop(&mut self) {
        #[cfg(feature = "window-lock-debug")]
        if let Some(slot) = self.slot {
            WINDOW_REGISTRY_READERS[slot].store(0, Ordering::Release);
        }
        crate::debug::lock_order::exit(RANK_WINDOW_REGISTRY, READ_SITE);
    }
}

/// Acquire `WINDOW_REGISTRY` for reading, recording the holder.
///
/// Without the `window-lock-debug` feature this is exactly `.read()`.
pub fn read_tracked(site: ReadSite) -> TrackedReadGuard<'static> {
    crate::debug::lock_order::enter(RANK_WINDOW_REGISTRY, READ_SITE);
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
