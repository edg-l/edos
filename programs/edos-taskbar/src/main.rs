//! EDOS Taskbar - Shows running windows and allows switching between them.

use std::time::Duration;

use edos_lib::process::spawn;
use edos_render::graphics::{Framebuffer, ScreenInfo};
use edos_render::theme::{draw_gradient_v, Theme};
use edos_render::widgets::{char_width, draw_rect, draw_rect_outline, draw_text};
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

/// Gap between adjacent window buttons.
const BUTTON_GAP: i32 = 4;

/// Height of every button in the bar.
const BUTTON_HEIGHT: u32 = 24;

/// Height of the accent underline marking the focused window's button.
const ACCENT_HEIGHT: u32 = 2;

/// Launcher button label.
const LAUNCHER_LABEL: &str = "+ Term";

/// Launcher button width in pixels.
const LAUNCHER_WIDTH: u32 = 64;

/// X position where the launcher button starts (after EDOS branding).
const LAUNCHER_X: i32 = 60;

/// X position where window buttons start (after launcher + gap).
const WINDOW_BUTTONS_X: i32 = LAUNCHER_X + LAUNCHER_WIDTH as i32 + 8;

/// X position of the wordmark, and of the hairline that closes its region.
const BRANDING_X: i32 = 8;
const BRANDING_RULE_X: i32 = 48;

/// Padding between the clock and the right edge, and between the clock and the
/// last window button.
const CLOCK_PADDING: i32 = 12;
const CLOCK_GAP: i32 = 16;

/// Height of a line of text drawn by `draw_text`.
const TEXT_HEIGHT: i32 = 16;

/// Get screen dimensions.
fn get_screen_info() -> Option<ScreenInfo> {
    let fb = Framebuffer::new();
    fb.screen_info().ok()
}

/// Draw `text` centered inside the rectangle at (`x`, `y`) sized `w` by `h`.
#[allow(clippy::too_many_arguments)]
fn draw_centered_text(
    buf: &mut [u32],
    buf_w: u32,
    buf_h: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    text: &str,
    color: u32,
    char_w: u32,
) {
    let text_w = text.chars().count() as i32 * char_w as i32;
    let text_x = x + (w as i32 - text_w) / 2;
    let text_y = y + (h as i32 - TEXT_HEIGHT) / 2;
    draw_text(buf, buf_w, buf_h, text_x, text_y, text, color);
}

fn main() {
    eprintln!("[taskbar] starting");
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
        let mut visible_windows: Vec<&WindowListEntry> = windows
            .iter()
            .filter(|w| w.id != my_window_id && w.visible != 0)
            .collect();
        // Sort by window ID for stable taskbar order (not z_order which changes on focus)
        visible_windows.sort_by_key(|w| w.id);

        // Focus comes from the kernel registry, which the window manager owns.
        // Deriving it from z_order disagrees with the title-bar accent, since a
        // raise also moves z_order.
        let focused_window_id = visible_windows
            .iter()
            .find(|w| w.is_focused())
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
                            let btn_h = BUTTON_HEIGHT as i32;
                            let btn_y = (h as i32 - btn_h) / 2;

                            // Check launcher button click
                            if click_x >= LAUNCHER_X
                                && click_x < LAUNCHER_X + LAUNCHER_WIDTH as i32
                                && click_y >= btn_y
                                && click_y < btn_y + btn_h
                            {
                                let _ = spawn("/bin/edos-terminal", &[], 0, 1, 2);
                            }

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

            // Wordmark, then a hairline closing off the identity region
            let cw = char_width();
            let text_y = (h as i32 - TEXT_HEIGHT) / 2;
            draw_text(
                buf,
                w,
                h,
                BRANDING_X,
                text_y,
                "EDOS",
                Theme::DEFAULT.taskbar_branding_text.raw(),
            );
            draw_rect(
                buf,
                w,
                h,
                BRANDING_RULE_X,
                8,
                1,
                h - 16,
                Theme::DEFAULT.taskbar_separator.raw(),
            );

            // Launcher: the one action in the bar, so it is outlined rather than
            // filled like the window buttons, which report state.
            let btn_h = BUTTON_HEIGHT;
            let btn_y = (h as i32 - btn_h as i32) / 2;

            draw_rect(
                buf,
                w,
                h,
                LAUNCHER_X,
                btn_y,
                LAUNCHER_WIDTH,
                btn_h,
                Theme::DEFAULT.taskbar_button_normal.raw(),
            );
            draw_rect_outline(
                buf,
                w,
                h,
                LAUNCHER_X,
                btn_y,
                LAUNCHER_WIDTH,
                btn_h,
                Theme::DEFAULT.taskbar_button_border.raw(),
            );
            draw_centered_text(
                buf,
                w,
                h,
                LAUNCHER_X,
                btn_y,
                LAUNCHER_WIDTH,
                btn_h,
                LAUNCHER_LABEL,
                Theme::DEFAULT.taskbar_text_active.raw(),
                cw,
            );

            // Clock (right-aligned); window buttons stop short of it
            let (hours, minutes) = if let Some(t) = edos_lib::time::clock_gettime() {
                (t.hour as u64, t.minute as u64)
            } else {
                (0, 0)
            };
            let clock_text = format!("{:02}:{:02}", hours, minutes);
            let clock_w = clock_text.chars().count() as i32 * cw as i32;
            let clock_x = w as i32 - clock_w - CLOCK_PADDING;
            draw_text(
                buf,
                w,
                h,
                clock_x,
                text_y,
                &clock_text,
                Theme::DEFAULT.taskbar_clock_text.raw(),
            );

            // Window buttons
            let buttons_limit = clock_x - CLOCK_GAP;
            let mut btn_x = WINDOW_BUTTONS_X;

            displayed_windows.clear();

            for win_entry in &visible_windows {
                if btn_x + BUTTON_WIDTH as i32 > buttons_limit {
                    break;
                }
                let title = win_entry.title_str();
                let label = if title.is_empty() {
                    format!("Win {}", win_entry.id)
                } else if title.chars().count() > 12 {
                    let truncated: String = title.chars().take(9).collect();
                    format!("{}...", truncated)
                } else {
                    title.to_string()
                };

                // The focused window reads as a raised chip underlined in the
                // same accent the window's own title bar wears.
                let is_focused = focused_window_id == Some(win_entry.id);
                let (btn_color, label_color) = if is_focused {
                    (
                        Theme::DEFAULT.taskbar_button_active,
                        Theme::DEFAULT.taskbar_text_active,
                    )
                } else {
                    (
                        Theme::DEFAULT.taskbar_button_normal,
                        Theme::DEFAULT.taskbar_text,
                    )
                };

                draw_rect(
                    buf,
                    w,
                    h,
                    btn_x,
                    btn_y,
                    BUTTON_WIDTH,
                    btn_h,
                    btn_color.raw(),
                );

                if is_focused {
                    draw_rect(
                        buf,
                        w,
                        h,
                        btn_x,
                        btn_y + (btn_h - ACCENT_HEIGHT) as i32,
                        BUTTON_WIDTH,
                        ACCENT_HEIGHT,
                        Theme::DEFAULT.taskbar_button_accent.raw(),
                    );
                }

                draw_centered_text(
                    buf,
                    w,
                    h,
                    btn_x,
                    btn_y,
                    BUTTON_WIDTH,
                    btn_h,
                    &label,
                    label_color.raw(),
                    cw,
                );

                displayed_windows.push((win_entry.id, btn_x));
                btn_x += BUTTON_WIDTH as i32 + BUTTON_GAP;
            }
        }

        window.swap_buffers();
        std::thread::sleep(Duration::from_millis(50));
    }
}
