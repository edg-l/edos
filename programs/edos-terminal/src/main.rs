//! EDOS Terminal - A terminal emulator application.

use std::time::Duration;

use edos_lib::process::ChildProcess;
use edos_render::widgets::{Terminal, Widget};
use edos_render::window::{Window, WindowEvent, WindowEventType};

/// Terminal window dimensions
const TERMINAL_WIDTH: u32 = 640;
const TERMINAL_HEIGHT: u32 = 480;

/// Shell path to spawn
const SHELL_PATH: &str = "/bin/sh";

fn main() {
    eprintln!("[terminal] starting");
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
    let mut terminal = Terminal::with_size(1, 0, 0, TERMINAL_WIDTH, TERMINAL_HEIGHT);
    terminal.set_focused(true);

    // Pre-render before showing window to avoid black frame
    window.fill(edos_render::widgets::terminal::terminal_colors::BACKGROUND);
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
            // Publish the grid before anything runs in it, so the first
            // full-screen program to start already knows the real size.
            let _ = edos_lib::io::set_winsize(
                c.master_fd,
                terminal.cols() as u16,
                terminal.rows() as u16,
            );
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

    // Main loop
    loop {
        // Poll window events
        let mut event_count = 0usize;
        if let Ok(count) = window.poll_events(&mut events) {
            event_count = count;
            for event in &events[..count] {
                match event.event_type() {
                    Some(WindowEventType::CloseRequested) => {
                        return;
                    }
                    Some(WindowEventType::Resize) => {
                        let new_w = event.x as u32;
                        let new_h = event.y as u32;
                        if window.resize(new_w, new_h).is_ok() {
                            terminal.resize_to_pixels(new_w, new_h);
                            // The grid moved, so tell the pty: a full-screen
                            // program reads its size from there and would
                            // otherwise keep drawing to the old one.
                            if let Some(c) = child.as_ref() {
                                let _ = edos_lib::io::set_winsize(
                                    c.master_fd,
                                    terminal.cols() as u16,
                                    terminal.rows() as u16,
                                );
                            }
                        } else {
                            eprintln!("[Terminal] Failed to resize window");
                        }
                    }
                    Some(WindowEventType::KeyPress) => {
                        terminal.on_key(event.code, true);
                    }
                    Some(WindowEventType::KeyRelease) => {
                        terminal.on_key(event.code, false);
                    }
                    Some(WindowEventType::MouseButton) => {
                        // Bit order is the boot protocol's: 0 left, 1 right,
                        // 2 middle. Left drives selection, middle pastes the
                        // primary selection on release, so a click that lands
                        // on the wrong window does not paste into it.
                        match event.code {
                            0 => {
                                terminal.on_mouse_button(
                                    event.x as i32,
                                    event.y as i32,
                                    event.data != 0,
                                );
                            }
                            2 if event.data == 0 => terminal.paste_primary(),
                            _ => {}
                        }
                    }
                    Some(WindowEventType::MouseMove) => {
                        terminal.on_mouse_move(event.x as i32, event.y as i32);
                    }
                    Some(WindowEventType::MouseScroll) => {
                        // HID reports a wheel turned away from the user as
                        // positive, and the widget takes positive as "back into
                        // history", which is the same direction: pushing the
                        // wheel away shows earlier output. The sign passes
                        // through unchanged.
                        terminal.scroll(event.data as i32);
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

        // Get input from terminal and send to shell
        let input_chars = terminal.take_input();
        if !input_chars.is_empty() {
            if let Some(ref child) = child {
                // Send input to shell (shell handles its own echo via stdout)
                let input_str: String = input_chars.iter().collect();
                child.write_str(&input_str);
            } else {
                // Echo mode (no shell) - display typed characters
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
        let mut output_len = 0usize;
        if let Some(ref child) = child {
            let n = child.read(&mut read_buf);
            if n > 0 {
                output_len = n as usize;
                let output = String::from_utf8_lossy(&read_buf[..n as usize]);
                terminal.write_str(&output);
            }
        }

        // Update cursor blink. Only the blink phase flipping is a reason to
        // repaint on its own.
        let blinked = terminal.tick();

        // Repainting an unchanged terminal is not free: `swap_buffers` reports
        // damage, and the compositor answers by transferring the whole window.
        // Doing that 62 times a second to show an identical picture is what
        // saturated the display link.
        if event_count > 0 || !input_chars.is_empty() || output_len > 0 || blinked {
            window.fill(edos_render::widgets::terminal::terminal_colors::BACKGROUND);

            let w = window.width;
            let h = window.height;
            if let Some(buf) = window.buffer_mut() {
                terminal.draw(buf, w, h);
            }

            window.swap_buffers();
        }
        std::thread::sleep(Duration::from_millis(16));
    }
}
