use alloc::{format, string::String, vec::Vec};
use core::{hint::spin_loop, str};

use elibc::{
    KeyEvent, WaitPidStatus, get_raw_input,
    io::{PollState, SelectFd, get_kernel_logs, keyboard_fd, select},
    process::sys_waitpid,
    read_from_fd,
};

mod command;
mod render;
mod state;

use command::{execute_command, parse_command};
use render::render;
use state::TerminalState;

pub fn run() -> i32 {
    let mut terminal = match TerminalState::new() {
        Ok(terminal) => terminal,
        Err(_) => return 1,
    };

    terminal.write_line("edos v0.1.0");
    terminal.write_line("");
    terminal.write_line("Type help for more info.");

    if render(&mut terminal).is_err() {
        return 1;
    }

    let mut key_events: Vec<KeyEvent> = Vec::new();
    let mut ready_commands: Vec<String> = Vec::new();
    let mut keyboard_handle = keyboard_fd();

    let mut select_entries: Vec<SelectFd> = Vec::new();

    while keyboard_handle.is_none() {
        keyboard_handle = keyboard_fd();
        // todo add sleep here
        spin_loop();
    }

    select_entries.push(SelectFd::new(
        keyboard_handle.unwrap(),
        PollState {
            readable: true,
            writable: false,
            error: true,
        },
    ));

    select_entries.push(SelectFd::new(
        terminal.tty_fd(),
        PollState {
            readable: true,
            writable: false,
            error: true,
        },
    ));

    loop {
        let select_result = select(&mut select_entries, Some(100));
        if select_result.is_err() {
            continue;
        }

        let mut keyboard_ready = false;
        let mut keyboard_fault = false;
        let mut tty_ready = false;

        for entry in &select_entries {
            if entry.fd == terminal.tty_fd() {
                if entry.result.readable || entry.result.error {
                    tty_ready = true;
                }
            } else if Some(entry.fd) == keyboard_handle {
                if entry.result.readable {
                    keyboard_ready = true;
                }
                if entry.result.error {
                    keyboard_fault = true;
                }
            }
        }

        if keyboard_fault {
            keyboard_handle = None;
        }

        if keyboard_ready {
            get_raw_input(0, &mut key_events, 16);
        }

        if tty_ready {
            pump_tty_output(&mut terminal);
        }

        pump_kernel_logs(&mut terminal);
        pump_running_program(&mut terminal);

        for event in key_events.drain(..) {
            if let Some(line) = terminal.handle_key_event(event) {
                ready_commands.push(line);
            }
        }

        for line in ready_commands.drain(..) {
            let parsed = parse_command(&line);
            if parsed.is_empty() {
                continue;
            }
            let (command_name, arguments) = parsed.split_first().unwrap();
            execute_command(&mut terminal, command_name, arguments);
        }

        if render(&mut terminal).is_err() {
            return 1;
        }
    }
}

fn pump_kernel_logs(state: &mut TerminalState) {
    if !state.logs_enabled() {
        return;
    }

    let logs = get_kernel_logs();
    if logs.is_empty() {
        return;
    }

    for log in logs {
        if log.ends_with('\n') {
            state.write_str(&log);
        } else {
            state.write_line(&log);
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
