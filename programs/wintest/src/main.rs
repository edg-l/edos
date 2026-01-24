//! Simple window test program.
//!
//! Creates a window and polls for events, printing them to stdout.

#![no_std]
#![no_main]

use elibc::{
    println, sleep_ms,
    window::{WindowEvent, WindowEventType, window_create, window_destroy, window_poll},
};

#[unsafe(no_mangle)]
extern "C" fn main(_argc: isize, _argv: *const *const u8) -> i32 {
    println!("Window test starting...");

    // Create a 400x300 window at position (100, 100)
    let window_id = match window_create(100, 100, 400, 300) {
        Ok(id) => {
            println!("Created window with ID: {}", id);
            id
        }
        Err(e) => {
            println!("Failed to create window: {}", e);
            return 1;
        }
    };

    // Poll for events in a loop
    let mut events = [WindowEvent {
        event_type: 0,
        x: 0,
        y: 0,
        code: 0,
        data: 0,
    }; 16];

    println!("Polling for events (press Ctrl+C to exit)...");

    for iteration in 0..100 {
        match window_poll(window_id, &mut events) {
            Ok(count) => {
                for event in &events[..count] {
                    match event.kind() {
                        Some(WindowEventType::MouseMove) => {
                            println!("MouseMove: ({}, {})", event.x, event.y);
                        }
                        Some(WindowEventType::MouseButton) => {
                            println!(
                                "MouseButton: btn={} pressed={} at ({}, {})",
                                event.code,
                                event.is_pressed(),
                                event.x,
                                event.y
                            );
                        }
                        Some(WindowEventType::MouseScroll) => {
                            println!("MouseScroll: delta={} at ({}, {})", event.data as i32, event.x, event.y);
                        }
                        Some(WindowEventType::KeyPress) => {
                            println!("KeyPress: code={}", event.code);
                        }
                        Some(WindowEventType::Character) => {
                            if let Some(ch) = event.character() {
                                println!("Character: '{}'", ch);
                            }
                        }
                        Some(WindowEventType::FocusGained) => {
                            println!("FocusGained");
                        }
                        Some(WindowEventType::FocusLost) => {
                            println!("FocusLost");
                        }
                        Some(kind) => {
                            println!("Event: {:?}", kind);
                        }
                        None => {
                            println!("Unknown event type: {}", event.event_type);
                        }
                    }
                }
            }
            Err(e) => {
                println!("Poll error: {}", e);
                break;
            }
        }

        // Sleep a bit between polls
        let _ = sleep_ms(100);

        if iteration % 10 == 0 {
            println!("Iteration {}/100", iteration);
        }
    }

    // Cleanup
    println!("Destroying window...");
    if let Err(e) = window_destroy(window_id) {
        println!("Failed to destroy window: {}", e);
        return 1;
    }

    println!("Window test complete!");
    0
}
