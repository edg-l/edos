//! EDOS panel: the launcher, the running windows, and the machine's status.

use std::time::Duration;

use edos_lib::io::klog_dump;
use edos_lib::process::spawn;
use edos_render::graphics::{Framebuffer, ScreenInfo};
use edos_render::icons;
use edos_render::surface::Surface;
use edos_render::text::Style;
use edos_render::theme::Theme;
use edos_render::widgets::{text_height, text_width};
use edos_render::window::{
    flags::FLAG_DOCK, property, window_list, window_minimize, window_send_event, window_set,
    Window, WindowEvent, WindowEventType, WindowListEntry,
};

mod menu;
mod panel;
mod status;

use panel::{Action, Hit};

/// Maximum number of windows to track.
const MAX_WINDOWS: usize = 32;

/// How long a wait may last before the panel looks around anyway.
///
/// The clock and the window list are polls, not events, so this is how stale
/// either may be. Everything the pointer does wakes the loop at once, so this
/// is not the rate it runs at.
const IDLE_POLL_MS: u64 = 250;

/// Prefix on every line of a panel dump, so a host-side reader can pick the
/// block out of an interleaved serial log.
const PANEL_DUMP_TAG: &str = "panel|";

/// Header starting each block, which is also how a reader tells where one
/// block ends and the next begins: it is the only line whose first field is
/// not a number.
const PANEL_DUMP_HEADER: &str = "X Y W H KIND LABEL";

/// Where every control of the panel sits on screen, and the name a person
/// would use for it.
///
/// The panel's buttons are not windows, so nothing in `/proc/windows` accounts
/// for them and something driving the machine from outside would otherwise
/// have to mirror this file's arithmetic by hand. A task is named by its
/// window's full title rather than the elided label actually drawn, since the
/// caller is naming the window, not reading the screen.
fn action_lines(
    hits: &[Hit],
    labelled: &[(&WindowListEntry, String)],
    panel_y: i32,
) -> Vec<String> {
    let y = panel_y + panel::button_y();
    hits.iter()
        .map(|hit| {
            let (kind, label) = match hit.action {
                Action::Launcher => ("launcher", ""),
                Action::Task(id) => (
                    "task",
                    labelled
                        .iter()
                        .find(|(entry, _)| entry.id == id)
                        .map(|(entry, _)| entry.title_str())
                        .unwrap_or(""),
                ),
                Action::Volume => ("volume", ""),
                Action::Network => ("network", ""),
                Action::Clock => ("clock", ""),
            };
            format!(
                "{} {} {} {} {} {}",
                hit.x,
                y,
                hit.width,
                panel::BUTTON_HEIGHT,
                kind,
                label
            )
        })
        .collect()
}

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

/// One control as it will be drawn.
///
/// Deciding what to repaint means comparing this against what a buffer already
/// holds, so everything that changes the picture has to be in here: a field
/// left out is a change that never reaches the screen.
#[derive(Clone, PartialEq)]
struct Control {
    x: i32,
    width: u32,
    icon: icons::Mask,
    label: String,
    fill: Option<u32>,
    ink: u32,
    /// The focused task's underline.
    accent: bool,
}

impl Control {
    /// The panel is one row of buttons, so a control's rectangle is its
    /// horizontal span at the shared button height.
    fn spans(&self) -> (i32, i32) {
        (self.x, self.x + self.width as i32)
    }
}

/// Draw a button's fill, its icon and its label, centred inside it.
fn draw_button(surface: &mut Surface<'_>, control: &Control) {
    let y = panel::button_y();
    if let Some(fill) = control.fill {
        surface.rect(control.x, y, control.width, panel::BUTTON_HEIGHT, fill);
    }

    // Icon and label are one group, centred together, so a button with a short
    // label does not leave its icon stranded against the left padding.
    let content_w = if control.label.is_empty() {
        icons::SIZE as u32
    } else {
        icons::SIZE as u32 + panel::ICON_GAP + text_width(&control.label)
    };
    let start = control.x + (control.width as i32 - content_w as i32) / 2;

    let icon_y = y + (panel::BUTTON_HEIGHT as i32 - icons::SIZE as i32) / 2;
    surface.icon(start, icon_y, &control.icon, control.ink);

    if !control.label.is_empty() {
        let text_y = y + (panel::BUTTON_HEIGHT as i32 - text_height() as i32) / 2;
        let text_x = start + icons::SIZE as i32 + panel::ICON_GAP as i32;
        surface.text(text_x, text_y, &control.label, Style::new(control.ink));
    }

    if control.accent {
        surface.rect(
            control.x,
            y + panel::BUTTON_HEIGHT as i32 - panel::ACCENT_HEIGHT as i32,
            control.width,
            panel::ACCENT_HEIGHT,
            Theme::DEFAULT.taskbar_button_accent.raw(),
        );
    }
}

/// What one of the panel's shm buffers already shows.
type PanelState = (Vec<Control>, u32, u32);

/// The horizontal span covering every control that differs between `previous`
/// and `current`, or None when the difference is not describable as a span and
/// the whole panel has to be reported.
///
/// Both the old and the new rectangle of a differing control are included: a
/// label that shrank leaves pixels behind that are only inside the old one.
fn changed_span(previous: Option<&PanelState>, current: &PanelState) -> Option<(i32, i32)> {
    let previous = previous?;
    // A different size or a different set of controls moves everything after
    // the change, so there is no span worth computing.
    if previous.1 != current.1 || previous.2 != current.2 {
        return None;
    }
    if previous.0.len() != current.0.len() {
        return None;
    }

    let mut span: Option<(i32, i32)> = None;
    for (was, now) in previous.0.iter().zip(&current.0) {
        if was == now {
            continue;
        }
        for (left, right) in [was.spans(), now.spans()] {
            span = Some(match span {
                Some((l, r)) => (l.min(left), r.max(right)),
                None => (left, right),
            });
        }
    }
    span
}

/// Lay the panel's ground: the gradient and the hairline along its top edge.
fn draw_ground(surface: &mut Surface<'_>) {
    let (w, h) = (surface.width, surface.height);
    surface.gradient_v(
        0,
        0,
        w,
        h,
        Theme::DEFAULT.taskbar_bg_top,
        Theme::DEFAULT.taskbar_bg_bottom,
    );
    surface.hline(0, 0, w, Theme::DEFAULT.taskbar_separator.raw());
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
    let mut popups = status::StatusPopups::new();
    let mut published: Vec<String> = Vec::new();
    let mut painted: [Option<PanelState>; 2] = [None, None];
    // The presented-frame count this loop last saw, passed back to each wait.
    let mut seen_frame = 0u64;

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
        let popup_id = popups.window_id();
        let mut tasks: Vec<&WindowListEntry> = windows
            .iter()
            .filter(|w| {
                w.id != my_id && Some(w.id) != menu_id && Some(w.id) != popup_id && w.visible != 0
            })
            .filter(|w| w.flags & FLAG_DOCK == 0)
            .collect();
        // By id, not z_order: sorting by z_order makes every button jump the
        // moment focus moves.
        tasks.sort_by_key(|w| w.id);

        let focused = tasks.iter().find(|w| w.is_focused()).map(|w| w.id);

        let clock = match edos_lib::time::local_time() {
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

        // Republished only when it moves, which is when a window opens or
        // closes or the clock's width changes. Every frame would drown the log.
        let lines = action_lines(&hits, &labelled, panel_y);
        if lines != published {
            klog_dump(
                PANEL_DUMP_TAG,
                std::iter::once(PANEL_DUMP_HEADER.to_string()).chain(lines.iter().cloned()),
            );
            published = lines;
        }

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
                            Action::Volume => {
                                popups.toggle(status::Kind::Volume, hit.x, hit.width, panel_y);
                            }
                            Action::Network => {
                                popups.toggle(status::Kind::Network, hit.x, hit.width, panel_y);
                            }
                            Action::Clock => {}
                        }
                    }
                    _ => {}
                }
            }
        }

        menu.tick(windows);
        popups.tick(windows);

        let hover_fill = Theme::DEFAULT.taskbar_button_normal.raw();
        let controls: Vec<Control> = hits
            .iter()
            .filter_map(|hit| {
                let is_hovered = hovered == Some(hit.action);
                let (icon, label, fill, ink, accent) = match hit.action {
                    Action::Launcher => {
                        let ink = if menu.is_open() {
                            Theme::DEFAULT.taskbar_button_accent
                        } else {
                            Theme::DEFAULT.taskbar_text_active
                        };
                        (
                            icons::APPS,
                            String::new(),
                            (is_hovered || menu.is_open()).then_some(hover_fill),
                            ink,
                            false,
                        )
                    }
                    Action::Task(id) => {
                        let (entry, label) = labelled.iter().find(|(entry, _)| entry.id == id)?;
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
                        (icons::TERMINAL, label.clone(), fill, ink, is_focused)
                    }
                    Action::Volume | Action::Network => {
                        let (icon, kind) = if hit.action == Action::Volume {
                            (icons::VOLUME, status::Kind::Volume)
                        } else {
                            (icons::NETWORK, status::Kind::Network)
                        };
                        let is_open = popups.open_kind() == Some(kind);
                        let ink = if is_open {
                            Theme::DEFAULT.taskbar_button_accent
                        } else {
                            Theme::DEFAULT.taskbar_text
                        };
                        (
                            icon,
                            String::new(),
                            (is_hovered || is_open).then_some(hover_fill),
                            ink,
                            false,
                        )
                    }
                    Action::Clock => (
                        icons::CLOCK,
                        clock.clone(),
                        is_hovered.then_some(hover_fill),
                        Theme::DEFAULT.taskbar_clock_text,
                        false,
                    ),
                };
                Some(Control {
                    x: hit.x,
                    width: hit.width,
                    icon,
                    label,
                    fill,
                    ink: ink.raw(),
                    accent,
                })
            })
            .collect();

        // Redrawing an unchanged panel is not free: swapping reports damage and
        // the compositor answers by transferring the whole 1280-pixel strip. At
        // twenty frames a second that was the largest single source of idle
        // display traffic on the machine.
        //
        // The two buffers are compared separately for the same reason the
        // terminal does it: the one being drawn holds the frame from two frames
        // ago, while the one on screen is what a viewer is looking at.
        let (w, h) = (window.width, window.height);
        let slot = window.back_index();
        let state = (controls, w, h);
        let redraw = painted[slot].as_ref() != Some(&state);
        let present = painted[slot ^ 1].as_ref() != Some(&state);

        if redraw {
            if let Some(buf) = window.buffer_mut() {
                let mut surface = Surface::new(buf, w, h);
                draw_ground(&mut surface);
                for control in &state.0 {
                    draw_button(&mut surface, control);
                }
            }
        }

        // The controls that differ from what the *screen* shows, as one
        // horizontal span: the panel is a single row, so the union of the
        // changed buttons is a tight rectangle rather than a bounding box
        // reaching across unrelated corners.
        let span = changed_span(painted[slot ^ 1].as_ref(), &state);
        if redraw || present {
            painted[slot] = Some(state);
        }
        if present {
            match span {
                Some((left, right)) => window.swap_buffers_damaged(
                    left,
                    panel::button_y(),
                    (right - left).max(0) as u32,
                    panel::BUTTON_HEIGHT,
                ),
                None => window.swap_buffers(),
            }
        }

        // Block until something happens rather than waking twenty times a
        // second to find the same clock. The timeout is what the clock and the
        // window list are polled at, since neither can be waited on; pointer
        // and click both wake immediately, which is what makes hover feel
        // attached to the pointer rather than sampled.
        match edos_render::window::wait(window.id, seen_frame, IDLE_POLL_MS) {
            Ok(woke) => seen_frame = woke.frame_seq,
            Err(_) => std::thread::sleep(Duration::from_millis(IDLE_POLL_MS)),
        }
    }
}

/// Launch a terminal. Kept here so the menu and any future shortcut agree on
/// what "new terminal" means.
pub fn launch_terminal() {
    let _ = spawn("/bin/edos-terminal", &[], 0, 1, 2);
}
