//! df - display filesystem disk space usage
//!
//! Usage: df [path]
//!   path defaults to "/"

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
    fs_code: u32,
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
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

fn print_statfs(path: &str, stat: &RawStatFs) {
    let fs_type = str_from_padded(&stat.fs_type);
    let vol_name = str_from_padded(&stat.volume_name);

    let total_size = stat.total_blocks * stat.block_size;
    let free_size = stat.free_blocks * stat.block_size;
    let used_size = total_size.saturating_sub(free_size);
    let used_pct = if total_size > 0 {
        (used_size as f64 / total_size as f64) * 100.0
    } else {
        0.0
    };

    println!("Filesystem: {}", path);
    println!("Type:       {}", fs_type);
    if !vol_name.is_empty() {
        println!("Label:      {}", vol_name);
    }
    println!("Block size: {} bytes", stat.block_size);

    // EFS-specific info
    if fs_type == "efs" {
        println!("Version:    {}", stat.version);
        if stat.block_groups > 0 {
            println!("Groups:     {}", stat.block_groups);
        }
    }

    println!();
    println!(
        "Blocks:     {}/{} ({} used, {:.1}%)",
        stat.total_blocks - stat.free_blocks,
        stat.total_blocks,
        format_size(used_size),
        used_pct
    );
    println!(
        "Space:      {} total, {} free",
        format_size(total_size),
        format_size(free_size)
    );
    if stat.total_inodes > 0 {
        let used_inodes = stat.total_inodes - stat.free_inodes;
        let inode_pct = (used_inodes as f64 / stat.total_inodes as f64) * 100.0;
        println!(
            "Inodes:     {}/{} ({:.1}% used)",
            used_inodes, stat.total_inodes, inode_pct
        );
    }
}

/// Get mount points from SYS_LIST_MOUNTS.
fn get_mounts() -> Vec<MountInfo> {
    let mut buf = vec![0u8; 4096];
    let ret = unsafe { syscall2(SYS_LIST_MOUNTS, buf.as_mut_ptr() as u64, buf.len() as u64) };
    if ret as i64 <= 0 {
        return Vec::new();
    }

    let count = ret as usize;
    let mut mounts = Vec::new();
    let mut offset = 0;

    for _ in 0..count {
        if offset + 24 > buf.len() {
            break;
        }
        let path_len = u32::from_le_bytes([
            buf[offset],
            buf[offset + 1],
            buf[offset + 2],
            buf[offset + 3],
        ]) as usize;
        let fs_code = u32::from_le_bytes([
            buf[offset + 4],
            buf[offset + 5],
            buf[offset + 6],
            buf[offset + 7],
        ]);
        // skip rest of header: device_id(u64) + partition_index(u64)
        offset += 24;
        if offset + path_len > buf.len() {
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
        mounts.push(MountInfo { path, fs_code });
        offset += path_len;
    }
    mounts
}

/// Returns true if the filesystem code represents a real (disk-backed) filesystem.
fn is_disk_filesystem(fs_code: u32) -> bool {
    matches!(
        fs_code,
        1 | 2 | 3 | 9 // fat12, fat16, fat32, efs
    )
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() > 1 {
        let path = &args[1];
        match statfs(path) {
            Some(stat) => print_statfs(path, &stat),
            None => {
                eprintln!("df: cannot get filesystem info for {}", path);
                std::process::exit(1);
            }
        }
    } else {
        // Show only disk-backed mounted filesystems
        let mounts = get_mounts();
        let mut first = true;
        for mount in &mounts {
            if !is_disk_filesystem(mount.fs_code) {
                continue;
            }
            if let Some(stat) = statfs(&mount.path) {
                if !first {
                    println!();
                }
                print_statfs(&mount.path, &stat);
                first = false;
            }
        }
        if first {
            match statfs("/") {
                Some(stat) => print_statfs("/", &stat),
                None => eprintln!("df: no filesystem info available"),
            }
        }
    }
}
