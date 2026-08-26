//! Widget toolkit demo application.
//!
//! Demonstrates all available widgets with automatic layout:
//! - Label: Static text display
//! - Button: Clickable buttons with hover/press states
//! - TextInput: Single-line text entry with cursor
//! - Checkbox: Toggle switches with labels
//! - Slider: Horizontal value sliders

use std::time::Duration;

use edos_render::metrics::{space, CONTROL_HEIGHT, TEXT_CELL_HEIGHT};
use edos_render::surface::Surface;
use edos_render::widgets::layout::{Alignment, Insets, LinearLayout, SizePolicy};
use edos_render::widgets::{
    Button, Checkbox, Label, Rect, Slider, TextInput, Widget, WidgetContainer, WidgetEvent,
    WidgetId,
};
use edos_render::window::{
    property, window_list, window_send_event, window_set, Window, WindowEvent, WindowEventType,
    WindowListEntry,
};

/// Margin from the window edge to the content column.
const MARGIN: u32 = space(5);
/// Every row is one control tall, so sections share a baseline rhythm.
const ROW_H: u32 = CONTROL_HEIGHT;
/// Gap between rows of the same group.
const ROW_GAP: u32 = space(2);
/// Gap between groups, and between a group and its separator.
const GROUP_GAP: u32 = space(4);
const WIN_W: u32 = 450;
const WIN_H: u32 = 450;
const CONTENT_X: i32 = MARGIN as i32;
const CONTENT_W: u32 = WIN_W - MARGIN * 2;
/// Width of the section-label column. Every row's label is sized to this, so
/// the controls beside them start at one column instead of wherever each
/// label's own text happens to end. Clears the longest label, "Brightness:".
const LABEL_W: u32 = space(24);
/// Left edge of anything that hangs under a section label.
const INDENT_X: i32 = CONTENT_X + (LABEL_W + GROUP_GAP) as i32;

/// Top of the row that follows one starting at `y`.
const fn row_after(y: i32, gap: u32) -> i32 {
    y + ROW_H as i32 + gap as i32
}

/// Top of text centred in a row starting at `y`.
const fn row_text_y(y: i32) -> i32 {
    y + (ROW_H as i32 - TEXT_CELL_HEIGHT as i32) / 2
}

/// A separator sits centred in the gap below the row starting at `y`.
const fn row_rule_y(y: i32) -> i32 {
    y + ROW_H as i32 + (GROUP_GAP / 2) as i32
}

const ROW_BUTTONS: i32 = (MARGIN + TEXT_CELL_HEIGHT + GROUP_GAP) as i32;
const ROW_INPUT: i32 = row_after(ROW_BUTTONS, GROUP_GAP);
const ROW_CHK1: i32 = row_after(ROW_INPUT, GROUP_GAP);
const ROW_CHK2: i32 = row_after(ROW_CHK1, ROW_GAP);
const ROW_VOLUME: i32 = row_after(ROW_CHK2, GROUP_GAP);
const ROW_BRIGHTNESS: i32 = row_after(ROW_VOLUME, ROW_GAP);
const ROW_SPEED: i32 = row_after(ROW_BRIGHTNESS, ROW_GAP);
const ROW_STATUS: i32 = row_after(ROW_SPEED, GROUP_GAP);
const ROW_FOOTER: i32 = row_after(ROW_STATUS, ROW_GAP);

/// Width of a slider row's control column, leaving room for its value readout.
const SLIDER_ROW_W: u32 = 300;

/// Put a section label in the shared label column, so the controls beside it
/// start at `INDENT_X` whatever the label says.
fn add_section_label(row: &mut LinearLayout, label: WidgetId) {
    row.add(label)
        .set_width(SizePolicy::Fixed(LABEL_W))
        .set_alignment(Alignment::center_left());
}

/// Ids of windows this process did not create.
fn windows_owned_by_others() -> Vec<u64> {
    let mut entries = [WindowListEntry::default(); 32];
    let count = window_list(&mut entries).unwrap_or(0).min(entries.len());
    let me = edos_lib::process::getpid();
    entries[..count]
        .iter()
        .filter(|w| w.pid != me)
        .map(|w| w.id)
        .collect()
}

/// Report whether the kernel refuses window management from a process that was
/// never appointed to the shell.
fn check_unprivileged(others: &[u64]) {
    let Some(&victim) = others.first() else {
        println!("wintest: no other window to test against, skipping privilege check");
        return;
    };
    let moved = window_set(victim, property::X, 0).is_ok();
    let minimized = window_set(victim, property::MINIMIZED, 1).is_ok();
    let posted = window_send_event(victim, &WindowEvent::close_requested()).is_ok();

    if moved || minimized || posted {
        println!(
            "wintest: FAIL privilege check on window {victim}: move={moved} minimize={minimized} event={posted}"
        );
    } else {
        println!("wintest: PASS an unappointed process cannot manage another window");
    }
}

fn main() {
    // Create a window for the widget demo
    let mut window = match Window::new(100, 100, WIN_W, WIN_H) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("Failed to create window: {:?}", e);
            return;
        }
    };
    let _ = window.set_title("Widgets");

    // Create widget container
    let mut widgets = WidgetContainer::new();

    // Create all widgets (positions will be set by layout)
    let title_label = widgets.add(Label::with_color(
        0,
        0,
        "Widget Toolkit Demo",
        edos_render::widgets::colors::FOCUS_RING,
    ));

    // Button section
    let btn_label = widgets.add(Label::new(0, 0, "Buttons:"));
    let btn_hello = widgets.add(Button::new(0, 0, "Say Hello"));
    let btn_count = widgets.add(Button::new(0, 0, "Count"));
    let btn_reset = widgets.add(Button::with_size(0, 0, space(20), ROW_H, "Reset"));

    // Text input section
    let input_label = widgets.add(Label::new(0, 0, "Text Input:"));
    let input_name = widgets.add(TextInput::with_placeholder(0, 0, 180, "Ada Lovelace"));
    let btn_greet = widgets.add(Button::new(0, 0, "Greet"));

    // Checkbox section
    let chk_label = widgets.add(Label::new(0, 0, "Options:"));
    let chk_sound = widgets.add(Checkbox::new(0, 0, "Enable sound"));
    let chk_dark = widgets.add(Checkbox::new(0, 0, "Dark mode"));
    let chk_auto = widgets.add(Checkbox::new(0, 0, "Auto-save"));
    let chk_notify = widgets.add(Checkbox::new(0, 0, "Notifications"));

    // Slider section
    let vol_label = widgets.add(Label::new(0, 0, "Volume:"));
    let slider_vol = widgets.add(Slider::with_value(0, 0, 180, 0, 100, 50));

    let bright_label = widgets.add(Label::new(0, 0, "Brightness:"));
    let slider_bright = widgets.add(Slider::with_value(0, 0, 180, 0, 100, 50));

    let speed_label = widgets.add(Label::new(0, 0, "Speed:"));
    let slider_speed = widgets.add(Slider::with_value(0, 0, 180, 0, 100, 25));

    // Status section
    let status_label = widgets.add(Label::new(0, 0, "Status:"));

    // Create main vertical layout
    let mut main_layout = LinearLayout::vertical();
    main_layout.set_padding(Insets::new(MARGIN, MARGIN, MARGIN, MARGIN));
    main_layout.set_spacing(ROW_GAP);
    main_layout.set_bounds(Rect::new(0, 0, WIN_W, WIN_H));

    // Title
    main_layout.add(title_label);

    // Buttons row (use HBoxLayout)
    let mut button_row = LinearLayout::horizontal();
    button_row.set_spacing(GROUP_GAP);
    button_row.set_bounds(Rect::new(CONTENT_X, ROW_BUTTONS, CONTENT_W, ROW_H));
    add_section_label(&mut button_row, btn_label);
    button_row.add(btn_hello);
    button_row.add(btn_count);
    button_row.add(btn_reset);
    button_row.add_stretch(1.0);

    // Input row
    let mut input_row = LinearLayout::horizontal();
    input_row.set_spacing(GROUP_GAP);
    input_row.set_bounds(Rect::new(CONTENT_X, ROW_INPUT, CONTENT_W, ROW_H));
    add_section_label(&mut input_row, input_label);
    input_row.add(input_name);
    input_row.add(btn_greet);

    // Checkboxes - two rows
    let mut chk_row1 = LinearLayout::horizontal();
    chk_row1.set_spacing(GROUP_GAP);
    chk_row1.set_uniform(true);
    chk_row1.set_bounds(Rect::new(CONTENT_X, ROW_CHK1, CONTENT_W, ROW_H));
    add_section_label(&mut chk_row1, chk_label);
    chk_row1.add(chk_sound);
    chk_row1.add(chk_auto);
    chk_row1.add_stretch(1.0);

    let mut chk_row2 = LinearLayout::horizontal();
    chk_row2.set_spacing(GROUP_GAP);
    chk_row2.set_uniform(true);
    chk_row2.set_bounds(Rect::new(
        INDENT_X,
        ROW_CHK2,
        CONTENT_W - (LABEL_W + GROUP_GAP),
        ROW_H,
    ));
    chk_row2.add(chk_dark);
    chk_row2.add(chk_notify);
    chk_row2.add_stretch(1.0);

    // Slider rows
    let mut vol_row = LinearLayout::horizontal();
    vol_row.set_spacing(GROUP_GAP);
    vol_row.set_bounds(Rect::new(CONTENT_X, ROW_VOLUME, SLIDER_ROW_W, ROW_H));
    add_section_label(&mut vol_row, vol_label);
    vol_row.add(slider_vol);

    let mut bright_row = LinearLayout::horizontal();
    bright_row.set_spacing(GROUP_GAP);
    bright_row.set_bounds(Rect::new(CONTENT_X, ROW_BRIGHTNESS, SLIDER_ROW_W, ROW_H));
    add_section_label(&mut bright_row, bright_label);
    bright_row.add(slider_bright);

    let mut speed_row = LinearLayout::horizontal();
    speed_row.set_spacing(GROUP_GAP);
    speed_row.set_bounds(Rect::new(CONTENT_X, ROW_SPEED, SLIDER_ROW_W, ROW_H));
    add_section_label(&mut speed_row, speed_label);
    speed_row.add(slider_speed);

    // Status row
    let mut status_row = LinearLayout::horizontal();
    status_row.set_spacing(GROUP_GAP);
    status_row.set_bounds(Rect::new(CONTENT_X, ROW_STATUS, CONTENT_W, ROW_H));
    add_section_label(&mut status_row, status_label);

    // Apply all layouts
    main_layout.layout(&mut widgets);
    button_row.layout(&mut widgets);
    input_row.layout(&mut widgets);
    chk_row1.layout(&mut widgets);
    chk_row2.layout(&mut widgets);
    vol_row.layout(&mut widgets);
    bright_row.layout(&mut widgets);
    speed_row.layout(&mut widgets);
    status_row.layout(&mut widgets);

    // Nothing to greet until there is a name. A disabled control keeps the
    // action visible and stops inviting a click, which is what hiding it
    // cannot do.
    if let Some(w) = widgets.get_mut(btn_greet) {
        w.set_enabled(false);
    }

    // Negative control for the shell privilege. wintest is an ordinary
    // program: init never appoints it, so the kernel must refuse to let it
    // move, put away, or post events to a window it does not own. Without a
    // check that fails when the privilege is *not* enforced, "the session
    // still works" proves nothing.
    check_unprivileged(&windows_owned_by_others());

    // Show the window
    if let Err(e) = window.show() {
        eprintln!("Failed to show window: {:?}", e);
        return;
    }

    println!("Widget demo started (using layout system).");
    println!("- Tab: cycle focus between widgets");
    println!("- Click: interact with widgets");
    println!("- Type: enter text in text input");
    println!("- Arrow keys: adjust slider when focused");

    // Event buffer
    let mut events = [WindowEvent::default(); 16];

    // Application state
    let mut click_count = 0;
    let mut volume = 50;
    let mut brightness = 50;
    let mut speed = 25;
    let mut status_message = String::from("Ready");

    // Main loop
    loop {
        // Poll for events
        if let Ok(count) = window.poll_events(&mut events) {
            for event in &events[..count] {
                match event.event_type() {
                    Some(WindowEventType::CloseRequested) => {
                        println!("Close requested, exiting.");
                        return;
                    }
                    Some(WindowEventType::Resize) => {
                        let new_w = event.x as u32;
                        let new_h = event.y as u32;
                        if window.resize(new_w, new_h).is_err() {
                            eprintln!("Failed to resize window");
                        }
                    }
                    _ => {}
                }

                // Dispatch event to widgets
                for (id, widget_event) in widgets.handle_event(event) {
                    match widget_event {
                        WidgetEvent::Clicked => {
                            if id == btn_hello {
                                status_message = String::from("Hello, World!");
                                println!("Hello, World!");
                            } else if id == btn_count {
                                click_count += 1;
                                status_message = format!("Count: {}", click_count);
                                println!("Count: {}", click_count);
                            } else if id == btn_reset {
                                click_count = 0;
                                volume = 50;
                                brightness = 50;
                                speed = 25;
                                status_message = String::from("Reset!");
                                println!("Reset all values");
                            } else if id == btn_greet {
                                let name = widgets
                                    .get(input_name)
                                    .map(|_| String::new())
                                    .unwrap_or_default();
                                let _ = name;
                                status_message = String::from("Greeted");
                            }
                        }
                        WidgetEvent::TextChanged(text) => {
                            if id == input_name {
                                let has_name = !text.trim().is_empty();
                                if let Some(w) = widgets.get_mut(btn_greet) {
                                    w.set_enabled(has_name);
                                }
                            }
                        }
                        WidgetEvent::Submit(text) => {
                            if id == input_name {
                                if text.is_empty() {
                                    status_message = String::from("Please enter a name!");
                                } else {
                                    status_message = format!("Hello, {}!", text);
                                    println!("Greeting: Hello, {}!", text);
                                }
                            }
                        }
                        WidgetEvent::ValueChanged(value) => {
                            if id == chk_sound {
                                let enabled = value == 1;
                                status_message =
                                    format!("Sound: {}", if enabled { "ON" } else { "OFF" });
                                println!("Sound: {}", if enabled { "enabled" } else { "disabled" });
                            } else if id == chk_dark {
                                let enabled = value == 1;
                                status_message =
                                    format!("Dark mode: {}", if enabled { "ON" } else { "OFF" });
                                println!(
                                    "Dark mode: {}",
                                    if enabled { "enabled" } else { "disabled" }
                                );
                            } else if id == chk_auto {
                                let enabled = value == 1;
                                status_message =
                                    format!("Auto-save: {}", if enabled { "ON" } else { "OFF" });
                            } else if id == chk_notify {
                                let enabled = value == 1;
                                status_message = format!(
                                    "Notifications: {}",
                                    if enabled { "ON" } else { "OFF" }
                                );
                            } else if id == slider_vol {
                                volume = value;
                                status_message = format!("Volume: {}%", volume);
                            } else if id == slider_bright {
                                brightness = value;
                                status_message = format!("Brightness: {}%", brightness);
                            } else if id == slider_speed {
                                speed = value;
                                status_message = format!("Speed: {}%", speed);
                            }
                        }
                    }
                }
            }
        }

        // Use theme background color (dark mode is now always-on via theme)
        let bg_color = edos_render::widgets::colors::BACKGROUND;
        let text_color = edos_render::widgets::colors::TEXT;

        // Clear and draw
        window.fill(bg_color);

        let w = window.width;
        let h = window.height;
        if let Some(buf) = window.buffer_mut() {
            // Draw all managed widgets
            widgets.draw_all(&mut Surface::new(buf, w, h));

            // Draw dynamic labels for slider values
            let value_x = CONTENT_X + SLIDER_ROW_W as i32 + GROUP_GAP as i32;
            let vol_text = format!("{}%", volume);
            Label::new(value_x, row_text_y(ROW_VOLUME), &vol_text)
                .draw(&mut Surface::new(buf, w, h));

            let bright_text = format!("{}%", brightness);
            Label::new(value_x, row_text_y(ROW_BRIGHTNESS), &bright_text)
                .draw(&mut Surface::new(buf, w, h));

            let speed_text = format!("{}%", speed);
            Label::new(value_x, row_text_y(ROW_SPEED), &speed_text)
                .draw(&mut Surface::new(buf, w, h));

            // Draw status message
            Label::with_color(
                INDENT_X,
                row_text_y(ROW_STATUS),
                &status_message,
                text_color,
            )
            .draw(&mut Surface::new(buf, w, h));

            // Draw separator lines
            let rule = edos_render::widgets::colors::INPUT_BORDER;
            let rule_x = CONTENT_X as u32;
            draw_hline(
                buf,
                w,
                rule_x,
                row_rule_y(ROW_BUTTONS) as u32,
                CONTENT_W,
                rule,
            );
            draw_hline(buf, w, rule_x, row_rule_y(ROW_CHK2) as u32, CONTENT_W, rule);
            draw_hline(
                buf,
                w,
                rule_x,
                row_rule_y(ROW_SPEED) as u32,
                CONTENT_W,
                rule,
            );

            // Draw footer with keyboard hints
            // A legend the reader cannot read is decoration. This one earns
            // its place in a toolkit demo, so it gets contrast to match.
            let hint_color = edos_render::widgets::colors::LABEL_TEXT;
            Label::with_color(
                CONTENT_X,
                row_text_y(ROW_FOOTER),
                "Tab: focus | Enter: submit | Arrows: adjust slider",
                hint_color,
            )
            .draw(&mut Surface::new(buf, w, h));
        }

        window.swap_buffers();
        std::thread::sleep(Duration::from_millis(16));
    }
}

/// Draw a horizontal line in the buffer.
fn draw_hline(buffer: &mut [u32], buffer_width: u32, x: u32, y: u32, width: u32, color: u32) {
    for px in x..(x + width).min(buffer_width) {
        let idx = (y * buffer_width + px) as usize;
        if idx < buffer.len() {
            buffer[idx] = color;
        }
    }
}
