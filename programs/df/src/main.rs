//! df - display filesystem disk space usage
//!
//! Usage: df [path]
//!   With no args, shows all mounted filesystems.

use edos_lib::mounts::{self, Mount, StatFs};
use std::env;

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

fn print_table(mounts: &[Mount]) {
    println!(
        "{:<12} {:<8} {:>8} {:>8} {:>8} {:>5} {}",
        "Filesystem", "Type", "Size", "Used", "Avail", "Use%", "Mounted on"
    );

    for mount in mounts {
        let Some(stat) = mounts::statfs(&mount.path) else {
            continue;
        };
        let total = stat.total_bytes();
        let pct = match stat.used_percent() {
            Some(pct) => format!("{pct}%"),
            None => "-".to_string(),
        };

        let name = if stat.volume_name.is_empty() {
            stat.fs_type.as_str()
        } else {
            stat.volume_name.as_str()
        };

        let size_str = if total > 0 {
            format_size(total)
        } else {
            "-".to_string()
        };
        let used_str = if total > 0 {
            format_size(stat.used_bytes())
        } else {
            "-".to_string()
        };
        let avail_str = if total > 0 {
            format_size(stat.free_bytes())
        } else {
            "-".to_string()
        };

        println!(
            "{:<12} {:<8} {:>8} {:>8} {:>8} {:>5} {}",
            name, stat.fs_type, size_str, used_str, avail_str, pct, mount.path
        );
    }
}

fn print_verbose(path: &str, stat: &StatFs) {
    println!("Filesystem: {}", path);
    println!("Type:       {}", stat.fs_type);
    if !stat.volume_name.is_empty() {
        println!("Label:      {}", stat.volume_name);
    }
    if stat.block_size > 0 {
        println!("Block size: {} bytes", stat.block_size);
    }

    if stat.fs_type == "efs" {
        println!("Version:    {}", stat.version);
        if stat.block_groups > 0 {
            println!("Groups:     {}", stat.block_groups);
        }
    }

    let total = stat.total_bytes();
    if total > 0 {
        let used = stat.used_bytes();
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
            format_size(stat.free_bytes())
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
        match mounts::statfs(path) {
            Some(stat) => print_verbose(path, &stat),
            None => {
                eprintln!("df: cannot get filesystem info for {}", path);
                std::process::exit(1);
            }
        }
    } else {
        // Table mode: all mounted filesystems
        let mounts = mounts::list();
        if mounts.is_empty() {
            eprintln!("df: no filesystems mounted");
            std::process::exit(1);
        }
        print_table(&mounts);
    }
}
