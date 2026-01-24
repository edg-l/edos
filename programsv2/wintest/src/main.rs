//! Widget toolkit demo application.
//!
//! Demonstrates all available widgets:
//! - Label: Static text display
//! - Button: Clickable buttons with hover/press states
//! - TextInput: Single-line text entry with cursor
//! - Checkbox: Toggle switches with labels
//! - Slider: Horizontal value sliders

use std::time::Duration;

use edos_render::widgets::{
    Button, Checkbox, Label, Slider, TextInput, Widget, WidgetContainer, WidgetEvent,
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

    // Create widget container
    let mut widgets = WidgetContainer::new();

    // === Title Section ===
    widgets.add(Label::with_color(0, 20, 15, "Widget Toolkit Demo", 0xFF000080));

    // === Button Section ===
    widgets.add(Label::new(0, 20, 50, "Buttons:"));
    let btn_hello = widgets.add(Button::new(0, 100, 45, "Say Hello"));
    let btn_count = widgets.add(Button::new(0, 220, 45, "Count"));
    let btn_reset = widgets.add(Button::with_size(0, 320, 45, 80, 28, "Reset"));

    // === Text Input Section ===
    widgets.add(Label::new(0, 20, 95, "Text Input:"));
    let input_name = widgets.add(TextInput::with_placeholder(0, 100, 90, 180, "Enter name..."));
    let btn_greet = widgets.add(Button::new(0, 290, 85, "Greet"));

    // === Checkbox Section ===
    widgets.add(Label::new(0, 20, 140, "Options:"));
    let chk_sound = widgets.add(Checkbox::new(0, 100, 140, "Enable sound"));
    let chk_dark = widgets.add(Checkbox::new(0, 100, 165, "Dark mode"));
    let chk_auto = widgets.add(Checkbox::new(0, 250, 140, "Auto-save"));
    let chk_notify = widgets.add(Checkbox::new(0, 250, 165, "Notifications"));

    // === Slider Section ===
    widgets.add(Label::new(0, 20, 210, "Volume:"));
    let slider_vol = widgets.add(Slider::with_value(0, 100, 205, 180, 0, 100, 50));

    widgets.add(Label::new(0, 20, 250, "Brightness:"));
    let slider_bright = widgets.add(Slider::with_value(0, 100, 245, 180, 0, 255, 128));

    widgets.add(Label::new(0, 20, 290, "Speed:"));
    let slider_speed = widgets.add(Slider::new(0, 100, 285, 180, 1, 10));

    // === Status Section ===
    widgets.add(Label::new(0, 20, 330, "Status:"));

    // Show the window
    if let Err(e) = window.show() {
        eprintln!("Failed to show window: {:?}", e);
        return;
    }

    println!("Widget demo started.");
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
                                // Get the text from the input
                                // We'll use a workaround since we can't easily get text
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
                                status_message = format!("Sound: {}", if enabled { "ON" } else { "OFF" });
                                println!("Sound: {}", if enabled { "enabled" } else { "disabled" });
                            } else if id == chk_dark {
                                dark_mode = value == 1;
                                status_message = format!("Dark mode: {}", if dark_mode { "ON" } else { "OFF" });
                                println!("Dark mode: {}", if dark_mode { "enabled" } else { "disabled" });
                            } else if id == chk_auto {
                                let enabled = value == 1;
                                status_message = format!("Auto-save: {}", if enabled { "ON" } else { "OFF" });
                            } else if id == chk_notify {
                                let enabled = value == 1;
                                status_message = format!("Notifications: {}", if enabled { "ON" } else { "OFF" });
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
            Label::new(0, 290, 210, &vol_text).draw(buf, w, h);

            let bright_text = format!("{}", brightness);
            Label::new(0, 290, 250, &bright_text).draw(buf, w, h);

            let speed_text = format!("{}x", speed);
            Label::new(0, 290, 290, &speed_text).draw(buf, w, h);

            // Draw status message
            Label::with_color(0, 100, 330, &status_message, text_color).draw(buf, w, h);

            // Draw click count
            let count_text = format!("Clicks: {}", click_count);
            Label::new(0, 320, 50, &count_text).draw(buf, w, h);

            // Draw separator lines
            draw_hline(buf, w, 20, 80, w - 40, 0xFFC0C0C0);
            draw_hline(buf, w, 20, 195, w - 40, 0xFFC0C0C0);
            draw_hline(buf, w, 20, 320, w - 40, 0xFFC0C0C0);

            // Draw footer with keyboard hints
            let hint_color = if dark_mode { 0xFF808080 } else { 0xFF606060 };
            Label::with_color(0, 20, 365, "Tab: focus | Enter: submit | Arrows: adjust slider", hint_color)
                .draw(buf, w, h);
        }

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
