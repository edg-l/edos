//! Window registry and types for the window server.

use alloc::{collections::btree_map::BTreeMap, string::String, vec::Vec};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use spin::RwLock;

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
}

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
        // Client area starts at (x + BORDER_WIDTH, y + TITLE_HEIGHT)
        let client_x = self.x + decoration::BORDER_WIDTH;
        let client_y = self.y + decoration::TITLE_HEIGHT;
        let client_w = self.width as i32;
        let client_h = self.height as i32;

        px >= client_x && px < client_x + client_w && py >= client_y && py < client_y + client_h
    }

    /// Check if point is within decorated bounds (for finding windows).
    pub fn contains_decorated(&self, px: i32, py: i32) -> bool {
        if !self.visible {
            return false;
        }
        let total_w = self.width as i32 + decoration::BORDER_WIDTH * 2;
        let total_h = self.height as i32 + decoration::TITLE_HEIGHT + decoration::BORDER_WIDTH;

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

/// Window decoration constants (must match WM).
pub mod decoration {
    pub const TITLE_HEIGHT: i32 = 24;
    pub const BORDER_WIDTH: i32 = 2;
}

/// Global window registry.
pub struct WindowRegistry {
    /// All windows indexed by ID.
    windows: BTreeMap<WindowId, WindowInfo>,
    /// Currently focused window.
    focused_window: Option<WindowId>,
}

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
pub static WINDOW_REGISTRY: RwLock<WindowRegistry> = RwLock::new(WindowRegistry::new());
