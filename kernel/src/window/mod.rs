//! Window server kernel module.
//!
//! Provides window management, input event routing, and compositor support.

pub mod input;
pub mod registry;

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
    // Get the list of window IDs to destroy
    let window_ids: alloc::vec::Vec<WindowId> = {
        let registry = registry::WINDOW_REGISTRY.read();
        registry.windows_for_pid(pid)
    };

    // Remove event queues for each window
    for &id in &window_ids {
        input::remove_event_queue(id);
    }

    // Destroy the windows
    registry::WINDOW_REGISTRY
        .write()
        .destroy_windows_for_pid(pid);
}
