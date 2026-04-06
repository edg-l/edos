//! EDOS Taskbar - Shows running windows and allows switching between them.

use std::time::Duration;

use edos_render::graphics::{Framebuffer, ScreenInfo};
use edos_render::theme::{Theme, draw_gradient_v};
use edos_render::widgets::{char_width, draw_rect, draw_text};
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

    // Track which windows we're showing buttons for: (window_id, btn_x)
    let mut displayed_windows: Vec<(u64, i32)> = Vec::new();

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

        // Determine focused window (highest z_order among visible windows)
        let focused_window_id = visible_windows
            .iter()
            .max_by_key(|w| w.z_order)
            .map(|w| w.id);

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
                    Some(WindowEventType::MouseButton) => {
                        if event.data == 1 {
                            // Button press
                            let click_x = event.x;
                            let click_y = event.y;
                            let h = window.height;
                            let btn_h = 24i32;
                            let btn_y = (h as i32 - btn_h) / 2;

                            for (win_id, bx) in &displayed_windows {
                                if click_x >= *bx
                                    && click_x < *bx + BUTTON_WIDTH as i32
                                    && click_y >= btn_y
                                    && click_y < btn_y + btn_h
                                {
                                    let focus_event = WindowEvent {
                                        event_type: WindowEventType::FocusGained as u32,
                                        x: 0,
                                        y: 0,
                                        code: 0,
                                        data: 0,
                                    };
                                    let _ = window_send_event(*win_id, &focus_event);
                                    println!("[Taskbar] Clicked on window {}", win_id);
                                    break;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // Draw taskbar
        let w = window.width;
        let h = window.height;
        if let Some(buf) = window.buffer_mut() {
            // Gradient background
            draw_gradient_v(
                buf,
                w,
                h,
                0,
                0,
                w,
                h,
                Theme::DEFAULT.taskbar_bg_top,
                Theme::DEFAULT.taskbar_bg_bottom,
            );

            // Top separator line
            for x in 0..w {
                buf[x as usize] = Theme::DEFAULT.taskbar_separator.raw();
            }

            // EDOS branding: colored accent bar then text
            let branding_x = 8i32;
            let accent_y = (h as i32 - 20) / 2;
            draw_rect(
                buf,
                w,
                h,
                branding_x,
                accent_y,
                4,
                20,
                Theme::DEFAULT.taskbar_branding_accent.raw(),
            );
            draw_text(
                buf,
                w,
                h,
                branding_x + 8,
                (h as i32 - 16) / 2,
                "EDOS",
                Theme::DEFAULT.taskbar_branding_text.raw(),
            );

            // Window buttons
            let cw = char_width();
            let btn_h = 24u32;
            let btn_y = (h as i32 - btn_h as i32) / 2;
            let mut btn_x = 60i32;

            displayed_windows.clear();

            for win_entry in &visible_windows {
                let title = win_entry.title_str();
                let label = if title.is_empty() {
                    format!("Win {}", win_entry.id)
                } else if title.len() > 12 {
                    format!("{}...", &title[..9])
                } else {
                    title.to_string()
                };

                let is_focused = focused_window_id == Some(win_entry.id);
                let btn_color = if is_focused {
                    Theme::DEFAULT.taskbar_button_active
                } else {
                    Theme::DEFAULT.taskbar_button_normal
                };

                // Button background
                draw_rect(buf, w, h, btn_x, btn_y, BUTTON_WIDTH, btn_h, btn_color.raw());

                // 1px top highlight on button
                draw_rect(
                    buf,
                    w,
                    h,
                    btn_x,
                    btn_y,
                    BUTTON_WIDTH,
                    1,
                    Theme::DEFAULT.taskbar_separator.raw(),
                );

                // Button text centered
                let text_w = label.len() as i32 * cw as i32;
                let text_x = btn_x + (BUTTON_WIDTH as i32 - text_w) / 2;
                let text_y = btn_y + (btn_h as i32 - 16) / 2;
                draw_text(
                    buf,
                    w,
                    h,
                    text_x,
                    text_y,
                    &label,
                    Theme::DEFAULT.taskbar_text.raw(),
                );

                displayed_windows.push((win_entry.id, btn_x));
                btn_x += BUTTON_WIDTH as i32 + 4;
            }

            // Clock display (right-aligned)
            let (hours, minutes) = if let Some(t) = edos_render::time::clock_gettime() {
                (t.hour as u64, t.minute as u64)
            } else {
                (0, 0)
            };
            let clock_text = format!("{:02}:{:02}", hours, minutes);
            let clock_w = clock_text.len() as u32 * cw;
            let clock_x = w as i32 - clock_w as i32 - 12;
            let clock_y = (h as i32 - 16) / 2;
            draw_text(
                buf,
                w,
                h,
                clock_x,
                clock_y,
                &clock_text,
                Theme::DEFAULT.taskbar_clock_text.raw(),
            );
        }

        window.swap_buffers();
        std::thread::sleep(Duration::from_millis(50));
    }
}
