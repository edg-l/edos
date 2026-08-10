//! EDOS panel: the launcher, the running windows, and the machine's status.

use std::time::Duration;

use edos_lib::process::spawn;
use edos_render::graphics::{Framebuffer, ScreenInfo};
use edos_render::icons;
use edos_render::theme::{draw_gradient_v, Theme};
use edos_render::widgets::{draw_rect, draw_text, text_height, text_width};
use edos_render::window::{
    flags::FLAG_DOCK, property, window_list, window_minimize, window_send_event, window_set,
    Window, WindowEvent, WindowEventType, WindowListEntry,
};

mod menu;
mod panel;

use panel::{Action, Hit};

/// Maximum number of windows to track.
const MAX_WINDOWS: usize = 32;

fn get_screen_info() -> Option<ScreenInfo> {
    let fb = Framebuffer::new();
    fb.screen_info().ok()
}

/// Shorten `text` with an ellipsis until it fits `max_width` pixels.
///
/// Measured, not counted: the face is proportional, so a fixed character
/// budget truncates `Illuminate` and leaves `WWWWWWWWWW` overflowing.
fn fit_label(text: &str, max_width: u32) -> String {
    if text_width(text) <= max_width {
        return text.to_string();
    }
    let ellipsis = "...";
    let budget = max_width.saturating_sub(text_width(ellipsis));
    let mut out = String::new();
    for ch in text.chars() {
        out.push(ch);
        if text_width(&out) > budget {
            out.pop();
            break;
        }
    }
    out.push_str(ellipsis);
    out
}

/// Draw a button's fill, its icon and its label, left-aligned inside it.
#[allow(clippy::too_many_arguments)]
fn draw_button(
    buf: &mut [u32],
    w: u32,
    h: u32,
    hit: &Hit,
    icon: &icons::Mask,
    label: &str,
    fill: Option<u32>,
    ink: u32,
) {
    let y = panel::button_y();
    if let Some(fill) = fill {
        draw_rect(buf, w, h, hit.x, y, hit.width, panel::BUTTON_HEIGHT, fill);
    }

    // Icon and label are one group, centred together, so a button with a short
    // label does not leave its icon stranded against the left padding.
    let content_w = if label.is_empty() {
        icons::SIZE as u32
    } else {
        icons::SIZE as u32 + panel::ICON_GAP + text_width(label)
    };
    let start = hit.x + (hit.width as i32 - content_w as i32) / 2;

    let icon_y = y + (panel::BUTTON_HEIGHT as i32 - icons::SIZE as i32) / 2;
    icons::draw(buf, w, h, start, icon_y, icon, ink);

    if !label.is_empty() {
        let text_y = y + (panel::BUTTON_HEIGHT as i32 - text_height() as i32) / 2;
        let text_x = start + icons::SIZE as i32 + panel::ICON_GAP as i32;
        draw_text(buf, w, h, text_x, text_y, label, ink);
    }
}

fn main() {
    eprintln!("[panel] starting");
    let screen_info = match get_screen_info() {
        Some(info) => info,
        None => {
            eprintln!("[panel] no screen info");
            return;
        }
    };

    let screen_width = screen_info.width as u32;
    let screen_height = screen_info.height as u32;

    let panel_y = (screen_height - panel::HEIGHT) as i32;
    let mut window = match Window::new(0, panel_y, screen_width, panel::HEIGHT) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[panel] could not create window: {e:?}");
            return;
        }
    };

    // Undecorated and never focusable: the panel paints no focus state, so
    // keystrokes landing in it would be invisible.
    if let Err(e) = window_set(window.id, property::FLAGS, FLAG_DOCK) {
        eprintln!("[panel] could not set dock flags: {e:?}");
    }
    if let Err(e) = window.set_title("Panel") {
        eprintln!("[panel] could not set title: {e:?}");
    }
    if let Err(e) = window.show() {
        eprintln!("[panel] could not show: {e:?}");
        return;
    }

    let mut events = [WindowEvent::default(); 16];
    let mut entries = [WindowListEntry::default(); MAX_WINDOWS];
    let mut hovered: Option<Action> = None;
    let mut menu = menu::Menu::new();

    loop {
        let window_count = match window_list(&mut entries) {
            Ok(count) => count.min(MAX_WINDOWS),
            Err(_) => 0,
        };
        let windows = &entries[..window_count];

        // Everything the user opened: our own panel and the menu are chrome,
        // and a minimized window stays listed so there is a way back to it.
        let my_id = window.id;
        let menu_id = menu.window_id();
        let mut tasks: Vec<&WindowListEntry> = windows
            .iter()
            .filter(|w| w.id != my_id && Some(w.id) != menu_id && w.visible != 0)
            .filter(|w| w.flags & FLAG_DOCK == 0)
            .collect();
        // By id, not z_order: sorting by z_order makes every button jump the
        // moment focus moves.
        tasks.sort_by_key(|w| w.id);

        let focused = tasks.iter().find(|w| w.is_focused()).map(|w| w.id);

        let clock = match edos_lib::time::clock_gettime() {
            Some(t) => format!("{:02}:{:02}", t.hour, t.minute),
            None => String::from("--:--"),
        };

        let labelled: Vec<(&WindowListEntry, String)> = tasks
            .iter()
            .map(|entry| {
                let title = entry.title_str();
                let label = if title.is_empty() {
                    format!("Window {}", entry.id)
                } else {
                    fit_label(
                        title,
                        panel::TASK_MAX_WIDTH
                            - panel::BUTTON_PAD * 2
                            - icons::SIZE as u32
                            - panel::ICON_GAP,
                    )
                };
                (*entry, label)
            })
            .collect();

        let layout = panel::compute(window.width, &labelled, &clock);
        let hits: Vec<Hit> = layout.hits;

        if let Ok(count) = window.poll_events(&mut events) {
            for event in &events[..count] {
                match event.event_type() {
                    Some(WindowEventType::CloseRequested) => return,
                    Some(WindowEventType::Resize) => {
                        let (new_w, new_h) = (event.x as u32, event.y as u32);
                        if window.resize(new_w, new_h).is_err() {
                            eprintln!("[panel] resize failed");
                        }
                    }
                    Some(WindowEventType::MouseMove) => {
                        hovered = hits
                            .iter()
                            .find(|hit| hit.contains(event.x))
                            .map(|hit| hit.action);
                    }
                    Some(WindowEventType::MouseButton) if event.data == 1 => {
                        let Some(hit) = hits.iter().find(|hit| hit.contains(event.x)) else {
                            continue;
                        };
                        match hit.action {
                            Action::Launcher => menu.toggle(hit.x, panel_y),
                            Action::Task(id) => {
                                // A second click on the window that already has
                                // focus puts it away, which is the only way to
                                // clear the screen without moving windows off it.
                                if focused == Some(id) {
                                    let _ = window_minimize(id, true);
                                } else {
                                    let _ = window_minimize(id, false);
                                    let event = WindowEvent {
                                        event_type: WindowEventType::FocusGained as u32,
                                        x: 0,
                                        y: 0,
                                        code: 0,
                                        data: 0,
                                    };
                                    let _ = window_send_event(id, &event);
                                }
                            }
                            Action::Volume | Action::Network | Action::Clock => {}
                        }
                    }
                    _ => {}
                }
            }
        }

        menu.tick(windows);

        let w = window.width;
        let h = window.height;
        if let Some(buf) = window.buffer_mut() {
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
            for x in 0..w {
                buf[x as usize] = Theme::DEFAULT.taskbar_separator.raw();
            }

            let hover_fill = Theme::DEFAULT.taskbar_button_normal.raw();
            for hit in &hits {
                let is_hovered = hovered == Some(hit.action);
                match hit.action {
                    Action::Launcher => {
                        let ink = if menu.is_open() {
                            Theme::DEFAULT.taskbar_button_accent
                        } else {
                            Theme::DEFAULT.taskbar_text_active
                        };
                        let fill = (is_hovered || menu.is_open()).then_some(hover_fill);
                        draw_button(buf, w, h, hit, &icons::APPS, "", fill, ink.raw());
                    }
                    Action::Task(id) => {
                        let Some((entry, label)) =
                            labelled.iter().find(|(entry, _)| entry.id == id)
                        else {
                            continue;
                        };
                        let is_focused = focused == Some(id);
                        let ink = if is_focused {
                            Theme::DEFAULT.taskbar_text_active
                        } else if entry.is_minimized() {
                            Theme::DEFAULT.text_placeholder
                        } else {
                            Theme::DEFAULT.taskbar_text
                        };
                        let fill = if is_focused {
                            Some(Theme::DEFAULT.taskbar_button_active.raw())
                        } else if is_hovered {
                            Some(hover_fill)
                        } else {
                            None
                        };
                        draw_button(buf, w, h, hit, &icons::TERMINAL, label, fill, ink.raw());

                        if is_focused {
                            draw_rect(
                                buf,
                                w,
                                h,
                                hit.x,
                                panel::button_y() + panel::BUTTON_HEIGHT as i32
                                    - panel::ACCENT_HEIGHT as i32,
                                hit.width,
                                panel::ACCENT_HEIGHT,
                                Theme::DEFAULT.taskbar_button_accent.raw(),
                            );
                        }
                    }
                    Action::Volume => {
                        let fill = is_hovered.then_some(hover_fill);
                        draw_button(
                            buf,
                            w,
                            h,
                            hit,
                            &icons::VOLUME,
                            "",
                            fill,
                            Theme::DEFAULT.taskbar_text.raw(),
                        );
                    }
                    Action::Network => {
                        let fill = is_hovered.then_some(hover_fill);
                        draw_button(
                            buf,
                            w,
                            h,
                            hit,
                            &icons::NETWORK,
                            "",
                            fill,
                            Theme::DEFAULT.taskbar_text.raw(),
                        );
                    }
                    Action::Clock => {
                        let fill = is_hovered.then_some(hover_fill);
                        draw_button(
                            buf,
                            w,
                            h,
                            hit,
                            &icons::CLOCK,
                            &clock,
                            fill,
                            Theme::DEFAULT.taskbar_clock_text.raw(),
                        );
                    }
                }
            }
        }

        window.swap_buffers();
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Launch a terminal. Kept here so the menu and any future shortcut agree on
/// what "new terminal" means.
pub fn launch_terminal() {
    let _ = spawn("/bin/edos-terminal", &[], 0, 1, 2);
}
