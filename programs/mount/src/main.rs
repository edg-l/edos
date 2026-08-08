//! mount - list mounts or mount a filesystem
//!
//! Usage:
//!   mount                                    # list current mounts
//!   mount <device_id> <partition_idx> <path> <fstype>  # mount a partition

use edos_lib::sys::{SYS_LIST_MOUNTS, SYS_LIST_PARTITIONS, SYS_MOUNT, syscall2, syscall4};
use std::env;
use std::process;



fn list_mounts() {
    let mut buf = vec![0u8; 4096];
    let ret = unsafe { syscall2(SYS_LIST_MOUNTS, buf.as_mut_ptr() as u64, buf.len() as u64) };
    let ret = ret as i64;
    if ret < 0 {
        eprintln!("mount: failed to list mounts");
        process::exit(1);
    }

    let len = ret as usize;
    if len == 0 {
        println!("No filesystems mounted.");
        return;
    }

    let mut offset = 0;
    while offset + 24 <= len {
        let path_len = u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap()) as usize;
        let fs_code = u32::from_le_bytes(buf[offset + 4..offset + 8].try_into().unwrap());
        let device_id = u64::from_le_bytes(buf[offset + 8..offset + 16].try_into().unwrap());
        let part_idx = u64::from_le_bytes(buf[offset + 16..offset + 24].try_into().unwrap());
        offset += 24;

        if offset + path_len > len {
            break;
        }
        let path = std::str::from_utf8(&buf[offset..offset + path_len]).unwrap_or("???");
        offset += path_len;

        let fs_name = match fs_code {
            3 => "fat32",
            6 => "memfs",
            7 => "devfs",
            8 => "procfs",
            9 => "efs",
            _ => "unknown",
        };

        let device = match fs_code {
            6 => "memfs".to_string(),
            7 => "devfs".to_string(),
            8 => "procfs".to_string(),
            _ => format!("dev{}p{}", device_id, part_idx),
        };

        println!("{} on {} type {}", device, path, fs_name);
    }
}

fn list_partitions() {
    let mut buf = vec![0u8; 4096];
    let ret = unsafe {
        syscall2(
            SYS_LIST_PARTITIONS,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        )
    };
    let ret = ret as i64;
    if ret < 0 {
        eprintln!("mount: failed to list partitions");
        process::exit(1);
    }

    let len = ret as usize;
    // Each SysPartition is 56 bytes
    let count = len / 56;
    if count == 0 {
        println!("No partitions found.");
        return;
    }

    println!("DEVICE  PART  START_LBA    END_LBA      SIZE_SECTORS");
    for i in 0..count {
        let base = i * 56;
        let index = u64::from_le_bytes(buf[base..base + 8].try_into().unwrap());
        let start = u64::from_le_bytes(buf[base + 8..base + 16].try_into().unwrap());
        let end = u64::from_le_bytes(buf[base + 16..base + 24].try_into().unwrap());
        let size = u64::from_le_bytes(buf[base + 24..base + 32].try_into().unwrap());
        let dev_id = u64::from_le_bytes(buf[base + 32..base + 40].try_into().unwrap());
        println!(
            "dev{}    {:>4}  {:>11}  {:>11}  {:>12}",
            dev_id, index, start, end, size
        );
    }
}

fn do_mount(device_id: u64, part_idx: u64, path: &str, fs_type: &str) {
    let path_c = format!("{}\0", path);
    let fs_c = format!("{}\0", fs_type);

    let ret = unsafe {
        syscall4(
            SYS_MOUNT,
            device_id,
            part_idx,
            path_c.as_ptr() as u64,
            fs_c.as_ptr() as u64,
        )
    };
    let ret = ret as i64;
    if ret < 0 {
        eprintln!(
            "mount: failed to mount dev{}p{} at {}",
            device_id, part_idx, path
        );
        process::exit(1);
    }
    println!(
        "Mounted dev{}p{} at {} ({})",
        device_id, part_idx, path, fs_type
    );
}

fn main() {
    let args: Vec<String> = env::args().collect();

    match args.len() {
        1 => list_mounts(),
        2 if args[1] == "-l" => list_partitions(),
        5 => {
            let device_id: u64 = args[1].parse().unwrap_or_else(|_| {
                eprintln!("mount: invalid device id: {}", args[1]);
                process::exit(1);
            });
            let part_idx: u64 = args[2].parse().unwrap_or_else(|_| {
                eprintln!("mount: invalid partition index: {}", args[2]);
                process::exit(1);
            });
            do_mount(device_id, part_idx, &args[3], &args[4]);
        }
        _ => {
            eprintln!("Usage: mount                                    # list mounts");
            eprintln!("       mount -l                                 # list partitions");
            eprintln!("       mount <dev_id> <part_idx> <path> <type>  # mount");
            process::exit(1);
        }
    }
}
