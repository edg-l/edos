//! EDOS Taskbar - Shows running windows and allows switching between them.

use std::time::Duration;

use edos_render::graphics::{Framebuffer, ScreenInfo};
use edos_render::widgets::layout::{HBoxLayout, Insets};
use edos_render::widgets::{Button, Label, Rect, WidgetContainer, WidgetEvent};
use edos_render::window::{
    flags::FLAG_DOCK, property, window_list, window_send_event, window_set, Window, WindowEvent,
    WindowEventType, WindowListEntry,
};

/// Taskbar height in pixels.
const TASKBAR_HEIGHT: u32 = 32;

/// Maximum number of windows to track.
const MAX_WINDOWS: usize = 32;

/// Button width for each window entry.
const BUTTON_WIDTH: u32 = 120;

/// Get screen dimensions.
fn get_screen_info() -> Option<ScreenInfo> {
    let fb = Framebuffer::new();
    fb.screen_info().ok()
}

fn main() {
    // Get screen dimensions
    let screen_info = match get_screen_info() {
        Some(info) => info,
        None => {
            eprintln!("Failed to get screen info");
            return;
        }
    };

    let screen_width = screen_info.width as u32;
    let screen_height = screen_info.height as u32;

    // Create taskbar window at bottom of screen (no decorations)
    let taskbar_y = (screen_height - TASKBAR_HEIGHT) as i32;
    let mut window = match Window::new(0, taskbar_y, screen_width, TASKBAR_HEIGHT) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("Failed to create taskbar window: {:?}", e);
            return;
        }
    };

    // Set dock flag (no decorations, not draggable)
    if let Err(e) = window_set(window.id, property::FLAGS, FLAG_DOCK) {
        eprintln!("Failed to set dock flag: {:?}", e);
    }

    if let Err(e) = window.set_title("Taskbar") {
        eprintln!("Failed to set title: {:?}", e);
    }

    if let Err(e) = window.show() {
        eprintln!("Failed to show taskbar: {:?}", e);
        return;
    }

    println!("[Taskbar] Started at y={} with FLAG_DOCK", taskbar_y);

    // Event buffer
    let mut events = [WindowEvent::default(); 16];

    // Window list buffer
    let mut entries = [WindowListEntry::default(); MAX_WINDOWS];

    // Track which windows we're showing buttons for
    let mut displayed_windows: Vec<(u64, u32)> = Vec::new(); // (window_id, button_id)

    // Main loop
    loop {
        // Get current window list
        let window_count = match window_list(&mut entries) {
            Ok(count) => count.min(MAX_WINDOWS),
            Err(_) => 0,
        };

        let windows = &entries[..window_count];

        // Filter out our own window and hidden windows
        let my_window_id = window.id;
        let visible_windows: Vec<&WindowListEntry> = windows
            .iter()
            .filter(|w| w.id != my_window_id && w.visible != 0)
            .collect();

        // Create widget container
        let mut widgets = WidgetContainer::new();

        // Create layout
        let mut layout = HBoxLayout::new();
        layout.set_padding(Insets::new(4, 8, 4, 8));
        layout.set_spacing(4);
        layout.set_bounds(Rect::new(0, 0, screen_width, TASKBAR_HEIGHT));

        // Add "Start" label
        let start_label = widgets.add(Label::with_color(0, 0, 0, "EDOS", 0xFF4080F0));
        layout.add(start_label);

        // Update displayed_windows list
        displayed_windows.clear();

        // Add button for each visible window
        for win_entry in &visible_windows {
            // Create label text (truncate if needed)
            let label = format!("Win {}", win_entry.id);
            let label = if label.len() > 12 {
                format!("{}...", &label[..9])
            } else {
                label
            };

            let button_id =
                widgets.add(Button::with_size(0, 0, 0, BUTTON_WIDTH, 24, &label));
            layout.add(button_id);

            displayed_windows.push((win_entry.id, button_id));
        }

        // Apply layout
        layout.layout(&mut widgets);

        // Handle taskbar events
        if let Ok(count) = window.poll_events(&mut events) {
            for event in &events[..count] {
                match event.event_type() {
                    Some(WindowEventType::CloseRequested) => {
                        return;
                    }
                    Some(WindowEventType::Resize) => {
                        let new_w = event.x as u32;
                        let new_h = event.y as u32;
                        if window.resize(new_w, new_h).is_err() {
                            eprintln!("[Taskbar] Failed to resize");
                        }
                    }
                    _ => {}
                }

                // Handle widget events
                for (button_id, widget_event) in widgets.handle_event(event) {
                    if let WidgetEvent::Clicked = widget_event {
                        // Find which window this button corresponds to
                        if let Some((target_window_id, _)) = displayed_windows
                            .iter()
                            .find(|(_, btn_id)| *btn_id == button_id)
                        {
                            // Send a FocusGained event to try to raise the window
                            let focus_event = WindowEvent {
                                event_type: WindowEventType::FocusGained as u32,
                                x: 0,
                                y: 0,
                                code: 0,
                                data: 0,
                            };
                            let _ = window_send_event(*target_window_id, &focus_event);
                            println!("[Taskbar] Clicked on window {}", target_window_id);
                        }
                    }
                }
            }
        }

        // Draw taskbar background
        let bg_color = 0xFF2D2D2D; // Dark gray
        window.fill(bg_color);

        // Draw widgets
        let w = window.width;
        let h = window.height;
        if let Some(buf) = window.buffer_mut() {
            widgets.draw_all(buf, w, h);

            // Draw top border (separator line)
            let border_color = 0xFF505050;
            for x in 0..w {
                buf[x as usize] = border_color;
            }
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}
