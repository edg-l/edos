//! Window server kernel module.
//!
//! Provides window management, input event routing, and compositor support.

use crate::debug::lock_order::RANK_WINDOW_REGISTRY;
use crate::ranked_write;

pub mod clipboard;
pub mod grab;
pub mod input;
pub mod registry;
pub mod shell;

pub use input::WindowEvent;
pub use registry::WindowId;

/// Initialize the window server.
pub fn init() {
    crate::log!("Initializing window server");
    input::init_input_routing();
}

/// Destroy all windows owned by a process and clean up their event queues.
/// Called during process exit.
pub fn cleanup_process_windows(pid: u64) {
    // Before the windows: a pid can be reused, and a later process must not
    // inherit the authority this one was given, nor the chords it claimed.
    shell::revoke(pid);
    grab::release_pid(pid);

    // Get window IDs then destroy windows first, so the input thread can't
    // send events to windows whose queues we're about to remove.
    let window_ids: alloc::vec::Vec<WindowId> = {
        let registry = registry::read_tracked(registry::ReadSite::CleanupProcessWindows);
        registry.windows_for_pid(pid)
    };

    let refocused = ranked_write!(
        RANK_WINDOW_REGISTRY,
        "window::cleanup_pid",
        registry::WINDOW_REGISTRY
    )
    .destroy_windows_for_pid(pid);

    for &id in &window_ids {
        input::remove_event_queue(id);
    }

    // Outside the registry lock, and after the dead queues are gone: the
    // window that inherits focus has to be told, or a process exiting leaves
    // the keyboard pointed at a window that believes it is unfocused and
    // drops every key.
    if let Some(new_focus) = refocused {
        input::send_event(new_focus, WindowEvent::focus_gained());
    }
}
