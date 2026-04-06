//! Widget toolkit demo application.
//!
//! Demonstrates all available widgets with automatic layout:
//! - Label: Static text display
//! - Button: Clickable buttons with hover/press states
//! - TextInput: Single-line text entry with cursor
//! - Checkbox: Toggle switches with labels
//! - Slider: Horizontal value sliders

use std::time::Duration;

use edos_render::widgets::layout::{Alignment, HBoxLayout, Insets, VBoxLayout};
use edos_render::widgets::{
    Button, Checkbox, Label, Rect, Slider, TextInput, Widget, WidgetContainer, WidgetEvent,
};
use edos_render::window::{Window, WindowEvent, WindowEventType};

fn main() {
    // Create a window for the widget demo
    let mut window = match Window::new(100, 100, 450, 400) {
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
        0,
        "Widget Toolkit Demo",
        0xFF000080,
    ));

    // Button section
    let btn_label = widgets.add(Label::new(0, 0, 0, "Buttons:"));
    let btn_hello = widgets.add(Button::new(0, 0, 0, "Say Hello"));
    let btn_count = widgets.add(Button::new(0, 0, 0, "Count"));
    let btn_reset = widgets.add(Button::with_size(0, 0, 0, 80, 28, "Reset"));

    // Text input section
    let input_label = widgets.add(Label::new(0, 0, 0, "Text Input:"));
    let input_name = widgets.add(TextInput::with_placeholder(0, 0, 0, 180, "Enter name..."));
    let btn_greet = widgets.add(Button::new(0, 0, 0, "Greet"));

    // Checkbox section
    let chk_label = widgets.add(Label::new(0, 0, 0, "Options:"));
    let chk_sound = widgets.add(Checkbox::new(0, 0, 0, "Enable sound"));
    let chk_dark = widgets.add(Checkbox::new(0, 0, 0, "Dark mode"));
    let chk_auto = widgets.add(Checkbox::new(0, 0, 0, "Auto-save"));
    let chk_notify = widgets.add(Checkbox::new(0, 0, 0, "Notifications"));

    // Slider section
    let vol_label = widgets.add(Label::new(0, 0, 0, "Volume:"));
    let slider_vol = widgets.add(Slider::with_value(0, 0, 0, 180, 0, 100, 50));

    let bright_label = widgets.add(Label::new(0, 0, 0, "Brightness:"));
    let slider_bright = widgets.add(Slider::with_value(0, 0, 0, 180, 0, 255, 128));

    let speed_label = widgets.add(Label::new(0, 0, 0, "Speed:"));
    let slider_speed = widgets.add(Slider::new(0, 0, 0, 180, 1, 10));

    // Status section
    let status_label = widgets.add(Label::new(0, 0, 0, "Status:"));

    // Create main vertical layout
    let mut main_layout = VBoxLayout::new();
    main_layout.set_padding(Insets::new(15, 20, 15, 20));
    main_layout.set_spacing(10);
    main_layout.set_bounds(Rect::new(0, 0, 450, 400));

    // Title
    main_layout.add(title_label);

    // Buttons row (use HBoxLayout)
    let mut button_row = HBoxLayout::new();
    button_row.set_spacing(10);
    button_row.set_bounds(Rect::new(20, 45, 410, 35));
    button_row.add(btn_label);
    button_row.add(btn_hello);
    button_row.add(btn_count);
    button_row.add(btn_reset);
    button_row.add_stretch(1.0);

    // Input row
    let mut input_row = HBoxLayout::new();
    input_row.set_spacing(10);
    input_row.set_bounds(Rect::new(20, 90, 410, 30));
    input_row.add(input_label);
    input_row.add(input_name);
    input_row.add(btn_greet);

    // Checkboxes - two rows
    let mut chk_row1 = HBoxLayout::new();
    chk_row1.set_spacing(20);
    chk_row1.set_bounds(Rect::new(20, 130, 410, 25));
    chk_row1.add(chk_label);
    chk_row1.add(chk_sound);
    chk_row1.add(chk_auto);
    chk_row1.add_stretch(1.0);

    let mut chk_row2 = HBoxLayout::new();
    chk_row2.set_spacing(20);
    chk_row2.set_bounds(Rect::new(100, 155, 330, 25));
    chk_row2.add(chk_dark);
    chk_row2.add(chk_notify);
    chk_row2.add_stretch(1.0);

    // Slider rows
    let mut vol_row = HBoxLayout::new();
    vol_row.set_spacing(10);
    vol_row.set_bounds(Rect::new(20, 195, 300, 30));
    vol_row
        .add(vol_label)
        .set_alignment(Alignment::center_left());
    vol_row.add(slider_vol);

    let mut bright_row = HBoxLayout::new();
    bright_row.set_spacing(10);
    bright_row.set_bounds(Rect::new(20, 235, 300, 30));
    bright_row
        .add(bright_label)
        .set_alignment(Alignment::center_left());
    bright_row.add(slider_bright);

    let mut speed_row = HBoxLayout::new();
    speed_row.set_spacing(10);
    speed_row.set_bounds(Rect::new(20, 275, 300, 30));
    speed_row
        .add(speed_label)
        .set_alignment(Alignment::center_left());
    speed_row.add(slider_speed);

    // Status row
    let mut status_row = HBoxLayout::new();
    status_row.set_spacing(10);
    status_row.set_bounds(Rect::new(20, 320, 410, 25));
    status_row.add(status_label);

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
    let mut brightness = 128;
    let mut speed = 1;
    let mut status_message = String::from("Ready");
    let mut dark_mode = false;

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
                                brightness = 128;
                                speed = 1;
                                status_message = String::from("Reset!");
                                println!("Reset all values");
                            } else if id == btn_greet {
                                status_message = String::from("Greet button clicked!");
                            }
                        }
                        WidgetEvent::TextChanged(_text) => {
                            // Text is being typed
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
                                dark_mode = value == 1;
                                status_message =
                                    format!("Dark mode: {}", if dark_mode { "ON" } else { "OFF" });
                                println!(
                                    "Dark mode: {}",
                                    if dark_mode { "enabled" } else { "disabled" }
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
                                status_message = format!("Brightness: {}", brightness);
                            } else if id == slider_speed {
                                speed = value;
                                status_message = format!("Speed: {}x", speed);
                            }
                        }
                    }
                }
            }
        }

        // Choose background color based on dark mode
        let bg_color = if dark_mode { 0xFF303030 } else { 0xFFE8E8E8 };
        let text_color = if dark_mode { 0xFFE0E0E0 } else { 0xFF202020 };

        // Clear and draw
        window.fill(bg_color);

        let w = window.width;
        let h = window.height;
        if let Some(buf) = window.buffer_mut() {
            // Draw all managed widgets
            widgets.draw_all(buf, w, h);

            // Draw dynamic labels for slider values
            let vol_text = format!("{}%", volume);
            Label::new(0, 320, 200, &vol_text).draw(buf, w, h);

            let bright_text = format!("{}", brightness);
            Label::new(0, 320, 240, &bright_text).draw(buf, w, h);

            let speed_text = format!("{}x", speed);
            Label::new(0, 320, 280, &speed_text).draw(buf, w, h);

            // Draw status message
            Label::with_color(0, 100, 320, &status_message, text_color).draw(buf, w, h);

            // Draw click count
            let count_text = format!("Clicks: {}", click_count);
            Label::new(0, 350, 50, &count_text).draw(buf, w, h);

            // Draw separator lines
            draw_hline(buf, w, 20, 80, w - 40, 0xFFC0C0C0);
            draw_hline(buf, w, 20, 185, w - 40, 0xFFC0C0C0);
            draw_hline(buf, w, 20, 310, w - 40, 0xFFC0C0C0);

            // Draw footer with keyboard hints
            let hint_color = if dark_mode { 0xFF808080 } else { 0xFF606060 };
            Label::with_color(
                0,
                20,
                365,
                "Tab: focus | Enter: submit | Arrows: adjust slider",
                hint_color,
            )
            .draw(buf, w, h);
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
