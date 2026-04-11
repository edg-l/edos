//! df - display filesystem disk space usage
//!
//! Usage: df [path]
//!   With no args, shows all mounted filesystems.

use std::arch::asm;
use std::env;

const SYS_STATFS: u64 = 254;
const SYS_LIST_MOUNTS: u64 = 208;

unsafe fn syscall2(num: u64, arg1: u64, arg2: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            in("rsi") arg2,
            lateout("rax") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

unsafe fn syscall3(num: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            lateout("rax") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawStatFs {
    fs_type: [u8; 16],
    block_size: u64,
    total_blocks: u64,
    free_blocks: u64,
    total_inodes: u64,
    free_inodes: u64,
    volume_name: [u8; 64],
    version: u32,
    block_groups: u16,
    _pad: [u8; 2],
}

struct MountInfo {
    path: String,
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}K", bytes as f64 / 1024.0)
    } else {
        format!("{}B", bytes)
    }
}

fn statfs(path: &str) -> Option<RawStatFs> {
    let path_c = format!("{}\0", path);
    let mut buf = [0u8; core::mem::size_of::<RawStatFs>()];

    let ret = unsafe {
        syscall3(
            SYS_STATFS,
            path_c.as_ptr() as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        )
    };

    if ret as i64 == -1 {
        return None;
    }

    Some(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const RawStatFs) })
}

fn str_from_padded(buf: &[u8]) -> &str {
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    std::str::from_utf8(&buf[..len]).unwrap_or("???")
}

fn get_mounts() -> Vec<MountInfo> {
    let mut buf = vec![0u8; 4096];
    let ret = unsafe { syscall2(SYS_LIST_MOUNTS, buf.as_mut_ptr() as u64, buf.len() as u64) };
    if ret as i64 <= 0 {
        return Vec::new();
    }

    // Return value is total bytes written, not entry count.
    let total_bytes = ret as usize;
    let mut mounts = Vec::new();
    let mut offset = 0;

    while offset + 24 <= total_bytes {
        let path_len = u32::from_le_bytes([
            buf[offset],
            buf[offset + 1],
            buf[offset + 2],
            buf[offset + 3],
        ]) as usize;
        // skip fs_code(u32) + device_id(u64) + partition_index(u64)
        offset += 24;
        if offset + path_len > total_bytes {
            break;
        }
        let path = std::str::from_utf8(&buf[offset..offset + path_len])
            .unwrap_or("")
            .to_string();
        let path = if path.is_empty() {
            "/".to_string()
        } else {
            path
        };
        mounts.push(MountInfo { path });
        offset += path_len;
    }
    mounts
}

fn print_table(mounts: &[MountInfo]) {
    // Header
    println!(
        "{:<12} {:<8} {:>8} {:>8} {:>8} {:>5} {}",
        "Filesystem", "Type", "Size", "Used", "Avail", "Use%", "Mounted on"
    );

    for mount in mounts {
        let Some(stat) = statfs(&mount.path) else {
            continue;
        };
        let fs_type = str_from_padded(&stat.fs_type);
        let total = stat.total_blocks * stat.block_size;
        let free = stat.free_blocks * stat.block_size;
        let used = total.saturating_sub(free);
        let pct = if total > 0 {
            format!("{:.0}%", (used as f64 / total as f64) * 100.0)
        } else {
            "-".to_string()
        };

        let vol = str_from_padded(&stat.volume_name);
        let name = if vol.is_empty() { fs_type } else { vol };

        let size_str = if total > 0 {
            format_size(total)
        } else {
            "-".to_string()
        };
        let used_str = if total > 0 {
            format_size(used)
        } else {
            "-".to_string()
        };
        let avail_str = if total > 0 {
            format_size(free)
        } else {
            "-".to_string()
        };

        println!(
            "{:<12} {:<8} {:>8} {:>8} {:>8} {:>5} {}",
            name, fs_type, size_str, used_str, avail_str, pct, mount.path
        );
    }
}

fn print_verbose(path: &str, stat: &RawStatFs) {
    let fs_type = str_from_padded(&stat.fs_type);
    let vol_name = str_from_padded(&stat.volume_name);

    println!("Filesystem: {}", path);
    println!("Type:       {}", fs_type);
    if !vol_name.is_empty() {
        println!("Label:      {}", vol_name);
    }
    if stat.block_size > 0 {
        println!("Block size: {} bytes", stat.block_size);
    }

    if fs_type == "efs" {
        println!("Version:    {}", stat.version);
        if stat.block_groups > 0 {
            println!("Groups:     {}", stat.block_groups);
        }
    }

    let total = stat.total_blocks * stat.block_size;
    let free = stat.free_blocks * stat.block_size;
    let used = total.saturating_sub(free);

    if total > 0 {
        let pct = (used as f64 / total as f64) * 100.0;
        println!();
        println!(
            "Blocks:     {}/{} ({} used, {:.1}%)",
            stat.total_blocks - stat.free_blocks,
            stat.total_blocks,
            format_size(used),
            pct
        );
        println!(
            "Space:      {} total, {} free",
            format_size(total),
            format_size(free)
        );
    }
    if stat.total_inodes > 0 {
        let used_inodes = stat.total_inodes.saturating_sub(stat.free_inodes);
        if stat.free_inodes > 0 {
            let inode_pct = (used_inodes as f64 / stat.total_inodes as f64) * 100.0;
            println!(
                "Inodes:     {}/{} ({:.1}% used)",
                used_inodes, stat.total_inodes, inode_pct
            );
        } else {
            println!("Inodes:     {}", stat.total_inodes);
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() > 1 {
        // Verbose single-path mode
        let path = &args[1];
        match statfs(path) {
            Some(stat) => print_verbose(path, &stat),
            None => {
                eprintln!("df: cannot get filesystem info for {}", path);
                std::process::exit(1);
            }
        }
    } else {
        // Table mode: all mounted filesystems
        let mounts = get_mounts();
        if mounts.is_empty() {
            eprintln!("df: no filesystems mounted");
            std::process::exit(1);
        }
        print_table(&mounts);
    }
}
