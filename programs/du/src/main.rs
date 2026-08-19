use std::env;
use std::path::Path;

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut human = false;
    let mut summary = false;
    let mut paths: Vec<&str> = Vec::new();

    for arg in &args[1..] {
        match arg.as_str() {
            "-h" => human = true,
            "-s" => summary = true,
            _ => paths.push(arg),
        }
    }

    if paths.is_empty() {
        paths.push(".");
    }

    for path in paths {
        let size = dir_size(Path::new(path));
        if summary {
            print_size(path, size, human);
        } else {
            walk_print(Path::new(path), human);
        }
    }
}

fn walk_print(dir: &Path, human: bool) {
    let (total, children) = dir_size_with_children(dir);
    for (child_name, child_size) in children {
        let child_path = dir.join(&child_name);
        print_size(
            child_path.to_str().unwrap_or(&child_name),
            child_size,
            human,
        );
    }
    print_size(dir.to_str().unwrap_or("."), total, human);
}

fn dir_size_with_children(dir: &Path) -> (u64, Vec<(String, u64)>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return (0, Vec::new()),
    };

    let mut total = 0u64;
    let mut children: Vec<(String, u64)> = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

        if is_dir {
            let child_size = dir_size(&path);
            total += child_size;
            children.push((name, child_size));
        } else {
            let file_size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            total += file_size;
        }
    }

    // Count dir itself (metadata overhead). For now, 0.
    (total, children)
}

fn dir_size(path: &Path) -> u64 {
    let entries = match std::fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => return 0,
    };

    let mut total = 0u64;
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            total += dir_size(&entry.path());
        } else {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    total
}

fn format_human(bytes: u64) -> String {
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

fn format_raw(bytes: u64) -> String {
    let kb = bytes.div_ceil(1024);
    if kb == 0 {
        "0K".to_string()
    } else {
        format!("{}K", kb)
    }
}

fn print_size(path: &str, bytes: u64, human: bool) {
    let size = if human {
        format_human(bytes)
    } else {
        format_raw(bytes)
    };
    println!("{}\t{}", size, path);
}
