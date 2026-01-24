//! Simple test window for EDOS window manager testing.

use std::time::Duration;
use edos_render::window::{Window, WindowEvent, WindowEventType};

fn main() {
    // Create a 200x150 window at position (100, 100)
    let mut window = match Window::new(100, 100, 200, 150) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("Failed to create window: {:?}", e);
            return;
        }
    };

    // Fill with a nice blue color
    window.fill(0xFF4080C0);

    // Draw a simple pattern to make resize visible
    draw_border(&mut window);

    // Show the window
    if let Err(e) = window.show() {
        eprintln!("Failed to show window: {:?}", e);
        return;
    }

    println!("Window created. Press Ctrl+C to exit.");

    // Event buffer
    let mut events = [WindowEvent::default(); 16];

    // Main loop
    loop {
        // Poll for events
        if let Ok(count) = window.poll_events(&mut events) {
            for event in &events[..count] {
                match event.event_type() {
                    Some(WindowEventType::CloseRequested) => {
                        println!("[wintest] Received CloseRequested, exiting.");
                        return;
                    }
                    Some(WindowEventType::Resize) => {
                        println!("Window resized to {}x{}", event.x, event.y);
                        // Note: Would need to reallocate buffer for actual resize handling
                    }
                    Some(WindowEventType::MouseButton) if event.is_button_press() => {
                        println!("Mouse button {} pressed at ({}, {})", event.code, event.x, event.y);
                    }
                    _ => {}
                }
            }
        }

        std::thread::sleep(Duration::from_millis(16));
    }
}

/// Draw a visible border pattern on the window.
fn draw_border(window: &mut Window) {
    let w = window.width;
    let h = window.height;
    let border_color = 0xFFFFFFFF; // White

    // Top and bottom borders
    for x in 0..w {
        window.set_pixel(x, 0, border_color);
        window.set_pixel(x, 1, border_color);
        window.set_pixel(x, h - 1, border_color);
        window.set_pixel(x, h - 2, border_color);
    }

    // Left and right borders
    for y in 0..h {
        window.set_pixel(0, y, border_color);
        window.set_pixel(1, y, border_color);
        window.set_pixel(w - 1, y, border_color);
        window.set_pixel(w - 2, y, border_color);
    }

    // Draw corner markers (so resize is more visible)
    let corner_color = 0xFFFF0000; // Red
    for i in 0..10 {
        // Top-left
        window.set_pixel(i, i, corner_color);
        // Top-right
        window.set_pixel(w - 1 - i, i, corner_color);
        // Bottom-left
        window.set_pixel(i, h - 1 - i, corner_color);
        // Bottom-right
        window.set_pixel(w - 1 - i, h - 1 - i, corner_color);
    }
}
