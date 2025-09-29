use core::fmt::Write as _;

use alloc::{
    ffi::CString,
    format,
    string::{String, ToString},
    vec::Vec,
};
use elibc::{
    Errno, FilesystemKind, create_dir,
    io::{
        FileType, FstatEntry, chdir, fstat, getcwd, list_dir, open, open_flags, read_to_end,
        write_all_fd,
    },
    list_mounts, list_partitions, mount_partition, remove_dir, remove_dir_all, remove_file, spawn,
    sys_close,
};

use super::state::{Program, TerminalState};

pub(super) fn parse_command(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = '\"';
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\"' | '\'' if !in_quotes => {
                in_quotes = true;
                quote_char = ch;
            }
            q if in_quotes && q == quote_char => {
                in_quotes = false;
            }
            '\\' if in_quotes => {
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            ' ' | '\t' if !in_quotes => {
                if !current.is_empty() {
                    args.push(current);
                    current = String::new();
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        args.push(current);
    }

    args
}

pub(super) fn execute_command(state: &mut TerminalState, command: &str, args: &[String]) {
    match command {
        "help" => print_help(state),
        "pwd" => print_pwd(state),
        "cd" => change_directory(state, args),
        "ls" => list_directory(state, args),
        "cat" => cat_file(state, args),
        "write" => write_file(state, args),
        "stat" => cmd_stat(state, args),
        "partitions" => cmd_list_partitions(state, args),
        "mount" => cmd_mount(state, args),
        "mkdir" => cmd_mkdir(state, args),
        "rmdir" => cmd_rmdir(state, args),
        "rm" => cmd_rm(state, args),
        "ps" => cmd_ps(state, args),
        "dmesg" => cmd_dmesg(state, args),
        "clear" => state.clear_output(),
        "profile" => cmd_profile(state, args),
        _ => spawn_program(state, command, args),
    }
}

fn print_help(state: &mut TerminalState) {
    let lines = [
        "Commands:",
        "- help",
        "- pwd",
        "- cd [path]",
        "- ls [path]",
        "- cat <path>",
        "- write <path> <content>",
        "- stat <path>",
        "- ps [<path>]",
        "- dmesg [<path>]",
        "- partitions",
        "- mount [<device_id> <partition_idx> <mount_point> <fstype>]",
        "- mkdir <path>",
        "- rmdir <path>",
        "- rm [-r] <path>",
        "- clear",
        "- profile [on|off]",
    ];

    for line in lines {
        state.write_line(line);
    }
}

fn cmd_profile(state: &mut TerminalState, args: &[String]) {
    if args.is_empty() {
        if state.toggle_profiling(None) {
            state.write_line("profiling overlay enabled");
        } else {
            state.write_line("profiling overlay disabled");
        }
        return;
    }

    match args[0].as_str() {
        "on" => {
            state.toggle_profiling(Some(true));
            state.write_line("profiling overlay enabled");
        }
        "off" => {
            state.toggle_profiling(Some(false));
            state.write_line("profiling overlay disabled");
        }
        _ => state.write_line("Usage: profile [on|off]"),
    }
}

fn print_pwd(state: &mut TerminalState) {
    match getcwd() {
        Ok(cwd) => state.write_line(&cwd),
        Err(_) => state.write_line("pwd: error getting current directory"),
    }
}

fn change_directory(state: &mut TerminalState, args: &[String]) {
    let target = args.first().map(String::as_str).unwrap_or("/");

    match chdir(target) {
        Ok(()) => state.update_prompt(),
        Err(_) => state.write_line(&format!("cd: {}: No such file or directory", target)),
    }
}

fn list_directory(state: &mut TerminalState, args: &[String]) {
    let path = args.first().map(String::as_str).unwrap_or(".");

    match list_dir(path) {
        Ok(entries) if entries.is_empty() => {}
        Ok(entries) => {
            let mut line = String::new();
            for (idx, entry) in entries.iter().enumerate() {
                let suffix = match entry.file_type {
                    FileType::File => "",
                    FileType::Directory => "/",
                    FileType::Symlink => "@",
                    FileType::Special => "*",
                };

                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(&entry.name);
                line.push_str(suffix);

                if (idx + 1) % 12 == 0 {
                    state.write_line(&line);
                    line.clear();
                }
            }
            if !line.is_empty() {
                state.write_line(&line);
            }
        }
        Err(_) => state.write_line(&format!(
            "ls: cannot access '{}': No such file or directory",
            path
        )),
    }
}

fn cat_file(state: &mut TerminalState, args: &[String]) {
    let Some(path) = args.first() else {
        state.write_line("Usage: cat <path>");
        return;
    };

    match open(path, 0) {
        Ok(fd) => {
            match read_to_end(fd, Some(16 * 1024)) {
                Ok(data) => match core::str::from_utf8(&data) {
                    Ok(text) => {
                        state.write_str(text);
                        state.write_line("");
                    }
                    Err(_) => state.write_line("[non-utf8 data]"),
                },
                Err(_) => state.write_line("cat: read error"),
            }
            let _ = sys_close(fd);
        }
        Err(_) => state.write_line("cat: open failed"),
    }
}

fn write_file(state: &mut TerminalState, args: &[String]) {
    if args.len() < 2 {
        state.write_line("Usage: write <path> <content>");
        return;
    }

    let path = &args[0];
    let content = args[1..].join(" ");

    match open(path, open_flags::O_APPEND | open_flags::O_CREAT) {
        Ok(fd) => {
            match write_all_fd(fd, content.as_bytes()) {
                Ok(()) => {}
                Err(_) => state.write_line("write: error"),
            }
            let _ = sys_close(fd);
        }
        Err(_) => state.write_line("write: open failed"),
    }
}

fn cmd_ps(state: &mut TerminalState, args: &[String]) {
    let path = args
        .first()
        .map(String::as_str)
        .unwrap_or("/proc/processes");

    match open(path, 0) {
        Ok(fd) => {
            let read_result = read_to_end(fd, Some(64 * 1024));
            let _ = sys_close(fd);

            match read_result {
                Ok(data) => match core::str::from_utf8(&data) {
                    Ok(text) => {
                        state.write_str(text);
                        if !text.ends_with('\n') {
                            state.write_line("");
                        }
                    }
                    Err(_) => state.write_line("ps: non-text response"),
                },
                Err(_) => state.write_line("ps: read error"),
            }
        }
        Err(_) => state.write_line(&format!("ps: failed to open {}", path)),
    }
}

fn cmd_dmesg(state: &mut TerminalState, args: &[String]) {
    let path = args.first().map(String::as_str).unwrap_or("/dev/klog");

    match open(path, 0) {
        Ok(fd) => {
            let read_result = read_to_end(fd, Some(128 * 1024));
            let _ = sys_close(fd);

            match read_result {
                Ok(data) => match core::str::from_utf8(&data) {
                    Ok(text) => {
                        state.write_str(text);
                        if !text.ends_with('\n') {
                            state.write_line("");
                        }
                    }
                    Err(_) => state.write_line("dmesg: non-text response"),
                },
                Err(_) => state.write_line("dmesg: read error"),
            }
        }
        Err(_) => state.write_line(&format!("dmesg: failed to open {}", path)),
    }
}

fn spawn_program(state: &mut TerminalState, command: &str, args: &[String]) {
    let mut argv: Vec<&str> = Vec::with_capacity(args.len() + 1);
    for arg in args {
        argv.push(arg);
    }

    let candidates = [
        format!("/bin/{}", command),
        format!("./{}", command),
        format!("/usr/bin/{}", command),
        format!("/{}", command),
    ];

    for path in candidates.iter() {
        let pid = spawn(path, &argv, 0, 1, 2);
        if pid != u64::MAX {
            state.set_running_program(Some(Program { pid }));
            return;
        }
    }

    state.write_line(&format!("Command not found: {}", command));
}

fn cmd_list_partitions(state: &mut TerminalState, _args: &[String]) {
    match list_partitions() {
        Ok(parts) if parts.is_empty() => state.write_line("No partitions found."),
        Ok(parts) => {
            state.write_line("Device Partition      Start LBA        End LBA  Size(sectors) GUID");
            for part in parts {
                let guid = format_guid(&part.unique_partition_guid);
                state.write_line(&format!(
                    "{:>5} {:>9} {:>14} {:>14} {:>14} {}",
                    part.device_id,
                    part.index,
                    part.starting_lba,
                    part.ending_lba,
                    part.size_sectors,
                    guid
                ));
            }
        }
        Err(err) => state.write_line(&format!("partitions: syscall failed ({:?})", err)),
    }
}

fn format_guid(bytes: &[u8; 16]) -> String {
    let mut out = String::with_capacity(36);
    for (idx, byte) in bytes.iter().enumerate() {
        if matches!(idx, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        let _ = write!(&mut out, "{:02x}", byte);
    }
    out
}

fn cmd_mount(state: &mut TerminalState, args: &[String]) {
    if args.is_empty() {
        match list_mounts() {
            Ok(mounts) if mounts.is_empty() => state.write_line("No filesystems mounted."),
            Ok(mounts) => {
                for entry in mounts {
                    let device = if entry.device_id == 9000 {
                        match entry.filesystem {
                            FilesystemKind::Devfs => "devfs".to_string(),
                            FilesystemKind::Memfs => "memfs".to_string(),
                            FilesystemKind::Procfs => "procfs".to_string(),
                            _ => {
                                format!("special:{}p{}", entry.device_id, entry.partition_index)
                            }
                        }
                    } else {
                        format!("dev{}p{}", entry.device_id, entry.partition_index)
                    };
                    state.write_line(&format!(
                        "{device} on {} type {}",
                        entry.path,
                        entry.filesystem.as_str()
                    ));
                }
            }
            Err(err) => state.write_line(&format!("mount: failed to list mounts: {:?}", err)),
        }
        return;
    }

    if args.len() != 4 {
        state.write_line("Usage: mount [<device_id> <partition_idx> <mount_point> <fstype>]");
        return;
    }

    let device_id = match args[0].parse::<u64>() {
        Ok(id) => id,
        Err(_) => {
            state.write_line("mount: invalid device id");
            return;
        }
    };

    let partition_idx = match args[1].parse::<u64>() {
        Ok(idx) => idx,
        Err(_) => {
            state.write_line("mount: invalid partition index");
            return;
        }
    };

    let c_path = match CString::new(args[2].as_str()) {
        Ok(path) => path,
        Err(_) => {
            state.write_line("mount: mount point contains null byte");
            return;
        }
    };

    let fs_type = match CString::new(args[3].as_str()) {
        Ok(path) => path,
        Err(_) => {
            state.write_line("mount: fs type contains null byte");
            return;
        }
    };

    match mount_partition(
        device_id,
        partition_idx,
        c_path.as_c_str(),
        fs_type.as_c_str(),
    ) {
        Ok(()) => state.write_line("Mounted successfully."),
        Err(err) => state.write_line(&format!("mount failed: {:?}", err)),
    }
}

fn cmd_mkdir(state: &mut TerminalState, args: &[String]) {
    let Some(path) = args.first() else {
        state.write_line("Usage: mkdir <path>");
        return;
    };

    let c_path = match CString::new(path.as_str()) {
        Ok(p) => p,
        Err(_) => {
            state.write_line("mkdir: path contains null byte");
            return;
        }
    };

    match create_dir(c_path.as_c_str()) {
        Ok(()) => state.write_line("Directory created."),
        Err(err) => state.write_line(&format!("mkdir failed: {:?}", err)),
    }
}

fn cmd_rmdir(state: &mut TerminalState, args: &[String]) {
    let Some(path) = args.first() else {
        state.write_line("Usage: rmdir <path>");
        return;
    };

    let c_path = match CString::new(path.as_str()) {
        Ok(p) => p,
        Err(_) => {
            state.write_line("rmdir: path contains null byte");
            return;
        }
    };

    match remove_dir(c_path.as_c_str()) {
        Ok(()) => state.write_line("Directory removed."),
        Err(err) => state.write_line(&format!("rmdir failed: {:?}", err)),
    }
}

fn cmd_rm(state: &mut TerminalState, args: &[String]) {
    if args.is_empty() {
        state.write_line("Usage: rm [-r] <path>");
        return;
    }

    let (recursive, path_arg) = if let Some(first) = args.first() {
        if first == "-r" || first == "-R" {
            (true, args.get(1))
        } else {
            (false, Some(first))
        }
    } else {
        (false, None)
    };

    let Some(path) = path_arg else {
        state.write_line("Usage: rm [-r] <path>");
        return;
    };

    let c_path = match CString::new(path.as_str()) {
        Ok(p) => p,
        Err(_) => {
            state.write_line("rm: path contains null byte");
            return;
        }
    };

    let result = if recursive {
        match remove_dir_all(c_path.as_c_str()) {
            Ok(()) => Ok(()),
            Err(err) if err == Errno::ENOTDIR || err == Errno::EISDIR => {
                remove_file(c_path.as_c_str())
            }
            Err(err) => Err(err),
        }
    } else {
        remove_file(c_path.as_c_str())
    };

    match result {
        Ok(()) => state.write_line("Removed."),
        Err(err) => state.write_line(&format!("rm failed: {:?}", err)),
    }
}

fn cmd_stat(state: &mut TerminalState, args: &[String]) {
    let Some(path) = args.first() else {
        state.write_line("Usage: stat <path>");
        return;
    };

    match open(path, 0) {
        Ok(fd) => {
            match fstat(fd) {
                Ok(entry) => {
                    display_file_stat(state, path, &entry);
                }
                Err(_) => state.write_line("stat: fstat failed"),
            }
            let _ = sys_close(fd);
        }
        Err(_) => state.write_line(&format!("stat: failed to open '{}'", path)),
    }
}

fn display_file_stat(state: &mut TerminalState, path: &str, entry: &FstatEntry) {
    state.write_line(&format!("File: {}", path));

    // Display file type
    let file_type = match entry.file_type() {
        FileType::File => "Regular file",
        FileType::Directory => "Directory",
        FileType::Symlink => "Symbolic link",
        FileType::Special => "Special file",
    };
    state.write_line(&format!("Type: {}", file_type));

    // Display size
    state.write_line(&format!("Size: {} bytes", entry.size));

    // Display timestamps (raw values for now)
    if entry.created != 0 {
        state.write_line(&format!("Created: {}", entry.created));
    }
    if entry.accessed != 0 {
        state.write_line(&format!("Accessed: {}", entry.accessed));
    }
    if entry.modified != 0 {
        state.write_line(&format!("Modified: {}", entry.modified));
    }

    // Display attributes
    let mut attrs = Vec::new();
    if entry.is_readonly() {
        attrs.push("readonly");
    }
    if entry.is_hidden() {
        attrs.push("hidden");
    }
    if entry.attrs & 4 != 0 {
        attrs.push("system");
    }
    if entry.attrs & 8 != 0 {
        attrs.push("archive");
    }

    if !attrs.is_empty() {
        state.write_line(&format!("Attributes: {}", attrs.join(", ")));
    } else {
        state.write_line("Attributes: none");
    }
}
