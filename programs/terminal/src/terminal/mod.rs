use alloc::{format, string::String, vec::Vec};
use core::str;

use elibc::{
    KeyEvent, PollFd, PollState, WaitPidStatus, get_raw_input, io::keyboard_fd, monotonic_time_ns,
    poll_fds, process::sys_waitpid, read_from_fd, sleep_ms,
};

mod command;
mod render;
mod state;

use command::{execute_command, parse_command};
use render::render;
use state::TerminalState;

const KERNEL_VERSION: &str = env!("KERNEL_VERSION");

pub fn run() -> i32 {
    let mut terminal = match TerminalState::new() {
        Ok(terminal) => terminal,
        Err(_) => return 1,
    };

    terminal.write_line(&format!("edos kernel v{}", KERNEL_VERSION));
    terminal.write_line("");
    terminal.write_line("Type help for more info.");

    if render(&mut terminal).is_err() {
        return 1;
    }

    let mut key_events: Vec<KeyEvent> = Vec::new();
    let mut ready_commands: Vec<String> = Vec::new();
    let mut keyboard_handle = keyboard_fd();

    while keyboard_handle.is_none() {
        keyboard_handle = keyboard_fd();
        // Avoid a tight busy loop while the keyboard device comes up.
        let _ = sleep_ms(10);
    }

    loop {
        let loop_start = monotonic_now();
        pump_running_program(&mut terminal);

        if keyboard_handle.is_none() {
            keyboard_handle = keyboard_fd();
        }

        let mut entries = [PollFd::default(); 2];
        let mut entry_count = 0;

        entries[entry_count] = PollFd::new(
            terminal.tty_fd(),
            PollState {
                readable: true,
                writable: false,
                error: true,
            },
        );
        entry_count += 1;

        if let Some(fd) = keyboard_handle {
            entries[entry_count] = PollFd::new(
                fd,
                PollState {
                    readable: true,
                    writable: false,
                    error: true,
                },
            );
            entry_count += 1;
        }

        let timeout_ms: u64 = if terminal.is_dirty() { 0 } else { 50 };
        let poll_slice = &mut entries[..entry_count];
        let poll_start = monotonic_now();
        let poll_result = poll_fds(poll_slice, timeout_ms);
        let poll_end = monotonic_now();
        let poll_ns = elapsed_ns(poll_start, poll_end);

        if let Ok(_) = poll_result {
            let tty_state = poll_slice[0].result;
            if tty_state.readable || tty_state.error {
                pump_tty_output(&mut terminal);
            }

            if entry_count > 1 {
                let keyboard_state = poll_slice[1].result;
                if keyboard_state.error {
                    keyboard_handle = None;
                } else if keyboard_state.readable {
                    get_raw_input(0, &mut key_events, 6);
                }
            }
        } else {
            // If polling failed, clear cached keyboard handle so we can retry opening it.
            keyboard_handle = None;
        }

        for event in key_events.drain(..) {
            if let Some(line) = terminal.handle_key_event(event) {
                ready_commands.push(line);
            }
        }

        let mut command_time_ns = 0u64;
        for line in ready_commands.drain(..) {
            let parsed = parse_command(&line);
            if parsed.is_empty() {
                continue;
            }
            let (command_name, arguments) = parsed.split_first().unwrap();
            let cmd_start = monotonic_now();
            execute_command(&mut terminal, command_name, arguments);
            let cmd_end = monotonic_now();
            command_time_ns = command_time_ns.saturating_add(elapsed_ns(cmd_start, cmd_end));
        }

        let render_start = monotonic_now();
        let present_ns = match render(&mut terminal) {
            Ok(ns) => ns,
            Err(_) => return 1,
        };
        let render_end = monotonic_now();
        let render_ns = elapsed_ns(render_start, render_end);

        if let (Some(loop_start_ns), Some(loop_end_ns)) = (loop_start, monotonic_now()) {
            let loop_ns = loop_end_ns.saturating_sub(loop_start_ns);
            terminal.record_profile(
                loop_ns,
                poll_ns,
                render_ns,
                present_ns,
                command_time_ns,
                loop_end_ns,
            );
        }
    }
}

fn pump_running_program(state: &mut TerminalState) {
    let Some(program) = state.running_program() else {
        return;
    };

    match sys_waitpid(program.pid, false) {
        Ok(WaitPidStatus::StillRunning) => {}
        Ok(WaitPidStatus::Exited(code)) => {
            state.set_running_program(None);
            state.write_line(&format!("Process exited with code {code}"));
        }
        Err(err) => {
            state.set_running_program(None);
            state.write_line(&format!("waitpid failed: {err:?}"));
        }
    }
}

fn pump_tty_output(state: &mut TerminalState) {
    let fd = state.tty_fd();
    let mut buffer = [0u8; 1024];

    loop {
        match read_from_fd(fd, &mut buffer) {
            Ok(0) => break,
            Ok(bytes_read) => {
                let slice = &buffer[..bytes_read];
                let text = match str::from_utf8(slice) {
                    Ok(txt) => txt,
                    Err(_) => {
                        let owned = String::from_utf8_lossy(slice);
                        state.write_str(owned.as_ref());
                        continue;
                    }
                };
                state.write_str(text);
            }
            Err(err) => {
                state.write_line(&format!("tty read error: {err:?}"));
                break;
            }
        }
    }
}

fn monotonic_now() -> Option<u64> {
    monotonic_time_ns().ok()
}

fn elapsed_ns(start: Option<u64>, end: Option<u64>) -> u64 {
    match (start, end) {
        (Some(s), Some(e)) => e.saturating_sub(s),
        _ => 0,
    }
}
