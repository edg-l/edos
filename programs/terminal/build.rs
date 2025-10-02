use std::{env, fs, path::PathBuf};

fn main() {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));
    let kernel_manifest = manifest_dir
        .join("..")
        .join("..")
        .join("kernel")
        .join("Cargo.toml");

    let contents = fs::read_to_string(&kernel_manifest)
        .unwrap_or_else(|err| panic!("failed to read {}: {}", kernel_manifest.display(), err));

    let mut in_package = false;
    let mut kernel_version: Option<String> = None;

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }

        if in_package && trimmed.starts_with("version") {
            if let Some(start) = trimmed.find('"') {
                if let Some(end) = trimmed[start + 1..].find('"') {
                    kernel_version = Some(trimmed[start + 1..start + 1 + end].to_string());
                    break;
                }
            }
        }
    }

    let kernel_version = kernel_version.expect("kernel version not found in kernel/Cargo.toml");
    println!("cargo:rustc-env=KERNEL_VERSION={}", kernel_version);
}
