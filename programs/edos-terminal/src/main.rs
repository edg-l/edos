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

/// How long a wait may last before the loop runs anyway.
///
/// The shell's output arrives on a pty, which the window wait cannot watch, so
/// this is the ceiling on how long output waits to be shown. It is not the rate
/// the loop runs at: input and frames wake it immediately.
const PTY_POLL_MS: u64 = 16;

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

    // Pre-render before showing window to avoid black frame. `show` publishes
    // this buffer itself, so nothing is swapped here.
    {
        let (w, h, slot) = (window.width, window.height, window.back_index());
        if let Some(buf) = window.buffer_mut() {
            terminal.draw_changed(slot, buf, w, h);
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

    // The presented-frame count this loop last saw, passed back to each wait.
    let mut seen_frame = 0u64;

    // Main loop
    loop {
        // Poll window events
        // Not every event changes what is on screen. Pointer motion in
        // particular arrives for the whole of a window drag, and treating it
        // as a reason to repaint made a terminal re-rasterise every glyph it
        // holds on every frame of that drag -- which is why dragging a
        // terminal full of text was far worse than dragging an empty one.
        let mut content_changed = false;
        if let Ok(count) = window.poll_events(&mut events) {
            for event in &events[..count] {
                match event.event_type() {
                    Some(WindowEventType::CloseRequested) => {
                        return;
                    }
                    Some(WindowEventType::Resize) => {
                        content_changed = true;
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
                        content_changed = true;
                    }
                    Some(WindowEventType::KeyRelease) => {
                        terminal.on_key(event.code, false);
                    }
                    Some(WindowEventType::MouseButton) => {
                        content_changed = true;
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
                        // Only a selection drag changes the picture; the
                        // pointer merely crossing the window does not.
                        if terminal.is_selecting() {
                            terminal.on_mouse_move(event.x as i32, event.y as i32);
                            content_changed = true;
                        }
                    }
                    Some(WindowEventType::MouseScroll) => {
                        content_changed = true;
                        // HID reports a wheel turned away from the user as
                        // positive, and the widget takes positive as "back into
                        // history", which is the same direction: pushing the
                        // wheel away shows earlier output. The sign passes
                        // through unchanged.
                        terminal.scroll(event.data as i32);
                    }
                    Some(WindowEventType::FocusGained) => {
                        terminal.set_focused(true);
                        content_changed = true;
                    }
                    Some(WindowEventType::FocusLost) => {
                        terminal.set_focused(false);
                        content_changed = true;
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

        // Repainting an unchanged terminal is not free: swapping reports
        // damage, and the compositor answers by transferring the window. Doing
        // that 62 times a second to show an identical picture is what saturated
        // the display link.
        //
        // The events above only say a repaint is *worth attempting*;
        // `draw_changed` is what decides whether anything actually differs, and
        // hands back the rectangle that does. A key that inserts one character
        // therefore costs one row of pixels rather than the whole window.
        if content_changed || !input_chars.is_empty() || output_len > 0 || blinked {
            let (w, h, slot) = (window.width, window.height, window.back_index());
            let changed = window
                .buffer_mut()
                .and_then(|buf| terminal.draw_changed(slot, buf, w, h));
            if let Some(rect) = changed {
                window.swap_buffers_damaged(rect.x, rect.y, rect.width, rect.height);
            }
        }

        // Block until there is something to do rather than sleeping a guessed
        // interval: a keystroke that arrives just after a sleep begins used to
        // wait out the rest of it. The timeout is still needed because the
        // shell's output arrives on a pty this cannot wait on, and the cursor
        // blink is a timer of its own; it is the ceiling on how long those go
        // unnoticed, not the rate the loop runs at.
        match edos_render::window::wait(window.id, seen_frame, PTY_POLL_MS) {
            Ok(woke) => seen_frame = woke.frame_seq,
            // Nothing to wait on: fall back to the timer rather than spinning.
            Err(_) => std::thread::sleep(Duration::from_millis(PTY_POLL_MS)),
        }
    }
}
