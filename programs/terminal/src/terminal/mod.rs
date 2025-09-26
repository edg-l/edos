use alloc::{format, string::String, vec::Vec};
use core::str;

use elibc::{
    KeyEvent, WaitPidStatus, get_raw_input, io::keyboard_fd, process::sys_waitpid, read_from_fd,
    sleep_ms,
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

    while keyboard_handle.is_none() {
        keyboard_handle = keyboard_fd();
        // Avoid a tight busy loop while the keyboard device comes up.
        let _ = sleep_ms(10);
    }

    loop {
        pump_tty_output(&mut terminal);
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

        if render(&mut terminal).is_err() {
            return 1;
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
