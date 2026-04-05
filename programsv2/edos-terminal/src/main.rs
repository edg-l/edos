//! EDOS Terminal - A terminal emulator application.

use std::time::Duration;

use edos_render::process::ChildProcess;
use edos_render::widgets::{Terminal, Widget};
use edos_render::window::{Window, WindowEvent, WindowEventType};

/// Terminal window dimensions
const TERMINAL_WIDTH: u32 = 640;
const TERMINAL_HEIGHT: u32 = 480;

/// Terminal character dimensions (cols x rows)
const TERMINAL_COLS: usize = 80;
const TERMINAL_ROWS: usize = 24;

/// Shell path to spawn
const SHELL_PATH: &str = "/bin/sh";

fn main() {
    // Create terminal window
    let mut window = match Window::new(100, 100, TERMINAL_WIDTH, TERMINAL_HEIGHT) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("Failed to create window: {:?}", e);
            return;
        }
    };

    if let Err(e) = window.set_title("Terminal") {
        eprintln!("Failed to set title: {:?}", e);
    }

    // Create terminal widget directly (not through WidgetContainer)
    let mut terminal = Terminal::new(1, 0, 0, TERMINAL_COLS, TERMINAL_ROWS);
    terminal.set_focused(true);

    // Pre-render before showing window to avoid black frame
    window.fill(0xFF1E1E1E);
    {
        let w = window.width;
        let h = window.height;
        if let Some(buf) = window.buffer_mut() {
            terminal.draw(buf, w, h);
        }
    }

    // Show the window
    if let Err(e) = window.show() {
        eprintln!("Failed to show window: {:?}", e);
        return;
    }

    // Try to spawn the shell
    let child = match ChildProcess::spawn_shell(SHELL_PATH) {
        Some(c) => {
            println!("[Terminal] Spawned shell (PID: {})", c.pid);
            Some(c)
        }
        None => {
            eprintln!("[Terminal] Failed to spawn shell at {}", SHELL_PATH);
            // Continue running even without a shell for testing
            terminal.write_str("EDOS Terminal\n");
            terminal.write_str("Shell not available. Type to echo.\n");
            terminal.write_str("> ");
            None
        }
    };

    // Read buffer for shell output
    let mut read_buf = [0u8; 4096];

    // Event buffer
    let mut events = [WindowEvent::default(); 16];

    let mut term_frame: u64 = 0;

    // Main loop
    loop {
        term_frame += 1;

        // Poll window events
        if let Ok(count) = window.poll_events(&mut events) {
            for event in &events[..count] {
                match event.event_type() {
                    Some(WindowEventType::CloseRequested) => {
                        println!("[Terminal] Close requested, exiting.");
                        return;
                    }
                    Some(WindowEventType::Resize) => {
                        let new_w = event.x as u32;
                        let new_h = event.y as u32;
                        if window.resize(new_w, new_h).is_err() {
                            eprintln!("[Terminal] Failed to resize window");
                        }
                    }
                    Some(WindowEventType::Character) => {
                        if let Some(ch) = event.character() {
                            terminal.on_char(ch);
                        }
                    }
                    Some(WindowEventType::KeyPress) => {
                        terminal.on_key(event.code, true);
                    }
                    Some(WindowEventType::KeyRelease) => {
                        terminal.on_key(event.code, false);
                    }
                    Some(WindowEventType::FocusGained) => {
                        eprintln!("[Term] FocusGained");
                        terminal.set_focused(true);
                    }
                    Some(WindowEventType::FocusLost) => {
                        eprintln!("[Term] FocusLost");
                        terminal.set_focused(false);
                    }
                    _ => {}
                }
            }
        }

        // Get input from terminal and send to shell or echo
        let input_chars = terminal.take_input();
        if !input_chars.is_empty() {
            if let Some(ref child) = child {
                // Local echo: the shell doesn't echo input back through the pipe.
                for ch in &input_chars {
                    if *ch == '\r' {
                        terminal.write_char('\n');
                    } else {
                        terminal.write_char(*ch);
                    }
                }
                // Send input to shell
                let input_str: String = input_chars.iter().collect();
                child.write_str(&input_str);
            } else {
                // Echo mode - display typed characters
                for ch in &input_chars {
                    if *ch == '\n' || *ch == '\r' {
                        terminal.write_str("\n> ");
                    } else {
                        terminal.write_char(*ch);
                    }
                }
            }
        }

        // Read output from shell and display
        if let Some(ref child) = child {
            let n = child.read(&mut read_buf);
            if n > 0 {
                let output = String::from_utf8_lossy(&read_buf[..n as usize]);
                terminal.write_str(&output);
            }
        }

        // Update cursor blink
        terminal.tick();

        // Draw
        window.fill(0xFF1E1E1E);

        let w = window.width;
        let h = window.height;
        if let Some(buf) = window.buffer_mut() {
            terminal.draw(buf, w, h);
        }

        std::thread::sleep(Duration::from_millis(16));
    }
}
