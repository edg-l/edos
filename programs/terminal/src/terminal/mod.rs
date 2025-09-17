use alloc::{format, string::String, vec::Vec};
use core::str;

use elibc::io::get_kernel_logs;
use elibc::{KeyEvent, get_raw_input, process::sys_waitpid, read_from_fd, sys_close};

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

    loop {
        pump_kernel_logs(&mut terminal);
        pump_running_program(&mut terminal);

        get_raw_input(20, &mut key_events, 16);

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

        if let Err(_) = render(&mut terminal) {
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

    let mut buffer = [0u8; 1024];
    loop {
        match read_from_fd(program.read_fd, &mut buffer) {
            Ok(0) => break,
            Ok(bytes_read) => {
                if let Ok(text) = str::from_utf8(&buffer[..bytes_read]) {
                    state.write_str(text);
                }
            }
            Err(err) => {
                state.write_line(&format!("Read error: {err:?}"));
                break;
            }
        }
    }

    if !sys_waitpid(program.pid, false) {
        let _ = sys_close(program.read_fd);
        state.set_running_program(None);
    }
}
