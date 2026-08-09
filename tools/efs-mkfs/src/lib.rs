//! EFS formatter.
//!
//! The same implementation serves the host tool and the in-EDOS `efs-mkfs`:
//! both binaries are a `main` that calls [`cli_main`], so a filesystem written
//! on the host and one written in the guest cannot drift apart.

mod alloc;
mod disk;
mod layout;
mod mkfs;
mod populate;
mod random;

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process;

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("G").or_else(|| s.strip_suffix("g")) {
        n.parse::<u64>().ok().map(|v| v * 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("M").or_else(|| s.strip_suffix("m")) {
        n.parse::<u64>().ok().map(|v| v * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("K").or_else(|| s.strip_suffix("k")) {
        n.parse::<u64>().ok().map(|v| v * 1024)
    } else {
        s.parse::<u64>().ok()
    }
}

fn usage() -> ! {
    eprintln!(
        "Usage: efs-mkfs [OPTIONS] <OUTPUT>

Arguments:
  <OUTPUT>  Output image file path

Options:
  --size <SIZE>                  Image size (e.g. 1G, 512M, 64M). Required unless image already exists.
  --block-size <BYTES>           Block size: 1024, 2048, 4096 (default), 8192
  --populate <DIR>               Recursively copy files from DIR into the root
  --partition-offset <BYTES>     Byte offset of EFS partition within the image (default: 0)
  --label <NAME>                 Volume label (max 63 chars)
  --journal-size-mib <N>         Journal size in MiB (default: 16, min: 4)
  --help                         Show this help
"
    );
    process::exit(1);
}

struct Args {
    output: PathBuf,
    size: Option<u64>,
    block_size: u32,
    populate: Option<PathBuf>,
    partition_offset: u64,
    label: Option<String>,
    journal_size_mib: u32,
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1).peekable();
    let mut output: Option<PathBuf> = None;
    let mut size: Option<u64> = None;
    let mut block_size: u32 = 4096;
    let mut populate: Option<PathBuf> = None;
    let mut partition_offset: u64 = 0;
    let mut label: Option<String> = None;
    let mut journal_size_mib: u32 = 16;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => usage(),
            "--size" => {
                let val = args.next().unwrap_or_else(|| {
                    eprintln!("--size requires a value");
                    process::exit(1);
                });
                size = Some(parse_size(&val).unwrap_or_else(|| {
                    eprintln!("invalid size: {val}");
                    process::exit(1);
                }));
            }
            "--block-size" => {
                let val = args.next().unwrap_or_else(|| {
                    eprintln!("--block-size requires a value");
                    process::exit(1);
                });
                block_size = val.parse().unwrap_or_else(|_| {
                    eprintln!("invalid block size: {val}");
                    process::exit(1);
                });
                if !matches!(block_size, 1024 | 2048 | 4096 | 8192) {
                    eprintln!("block size must be 1024, 2048, 4096, or 8192");
                    process::exit(1);
                }
            }
            "--populate" => {
                let val = args.next().unwrap_or_else(|| {
                    eprintln!("--populate requires a directory");
                    process::exit(1);
                });
                populate = Some(PathBuf::from(val));
            }
            "--partition-offset" => {
                let val = args.next().unwrap_or_else(|| {
                    eprintln!("--partition-offset requires a value");
                    process::exit(1);
                });
                partition_offset = val.parse().unwrap_or_else(|_| {
                    eprintln!("invalid partition offset: {val}");
                    process::exit(1);
                });
            }
            "--label" => {
                let val = args.next().unwrap_or_else(|| {
                    eprintln!("--label requires a value");
                    process::exit(1);
                });
                if val.len() > 63 {
                    eprintln!("label too long (max 63 chars)");
                    process::exit(1);
                }
                label = Some(val);
            }
            "--journal-size-mib" => {
                let val = args.next().unwrap_or_else(|| {
                    eprintln!("--journal-size-mib requires a value");
                    process::exit(1);
                });
                let n: u32 = val.parse().unwrap_or_else(|_| {
                    eprintln!("invalid journal size: {val}");
                    process::exit(1);
                });
                if n < 4 {
                    eprintln!("--journal-size-mib must be at least 4");
                    process::exit(1);
                }
                journal_size_mib = n;
            }
            s if s.starts_with('-') => {
                eprintln!("unknown option: {s}");
                usage();
            }
            _ => {
                if output.is_some() {
                    eprintln!("unexpected argument: {arg}");
                    process::exit(1);
                }
                output = Some(PathBuf::from(arg));
            }
        }
    }

    let output = output.unwrap_or_else(|| {
        eprintln!("output path is required");
        usage();
    });

    Args {
        output,
        size,
        block_size,
        populate,
        partition_offset,
        label,
        journal_size_mib,
    }
}

/// What to format, and how. The CLI fills this in from its flags;
/// `edos-install` fills it in directly.
pub struct Format<'a> {
    /// Device or image to write to.
    pub target: &'a std::path::Path,
    /// Byte offset of the partition within the target.
    pub partition_offset: u64,
    /// Partition size in bytes, or `None` to use everything after the offset.
    pub partition_size: Option<u64>,
    pub block_size: u32,
    pub label: Option<&'a str>,
    pub journal_size_mib: u32,
    /// Directory tree to copy into the new filesystem.
    pub populate: Option<&'a std::path::Path>,
}

/// Create an EFS filesystem as described by `spec`.
///
/// This is the whole formatter; the CLI below is only argument parsing around
/// it, so the host tool, the in-EDOS `efs-mkfs` and `edos-install` all produce
/// byte-identical layouts.
pub fn format(spec: &Format) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(spec.target)?;

    let partition_size = match spec.partition_size {
        Some(size) => size,
        None => {
            let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
            if file_len <= spec.partition_offset {
                return Err(std::io::Error::other(format!(
                    "'{}' is too small or empty; give an explicit size",
                    spec.target.display()
                )));
            }
            file_len - spec.partition_offset
        }
    };

    if partition_size < spec.block_size as u64 * 16 {
        return Err(std::io::Error::other("partition too small"));
    }

    println!(
        "Formatting {} (partition size {}, block size {}, offset {})...",
        spec.target.display(),
        partition_size,
        spec.block_size,
        spec.partition_offset
    );

    let layout = layout::Layout::compute(partition_size, spec.block_size, spec.partition_offset);

    let journal_blocks = spec.journal_size_mib as u64 * 1024 * 1024 / spec.block_size as u64;
    if journal_blocks + 16 > layout.total_blocks {
        return Err(std::io::Error::other(format!(
            "journal too large: {} blocks requested but partition only has {} blocks",
            journal_blocks, layout.total_blocks
        )));
    }

    println!(
        "  {} blocks, {} groups, {} inodes, journal {} blocks",
        layout.total_blocks, layout.block_group_count, layout.total_inodes, journal_blocks,
    );

    let (mut allocator, mut bgds) =
        mkfs::format(&mut file, &layout, spec.label, journal_blocks)?;

    if let Some(pop_dir) = spec.populate {
        if !pop_dir.is_dir() {
            return Err(std::io::Error::other(format!(
                "populate path is not a directory: {}",
                pop_dir.display()
            )));
        }
        println!("Populating from {}...", pop_dir.display());
        populate::populate(&mut file, &mut allocator, &layout, &mut bgds, pop_dir)?;
    }

    mkfs::finalize(&mut file, &layout, &allocator, &mut bgds)?;
    Ok(())
}

pub fn cli_main() {
    let args = parse_args();

    let spec = Format {
        target: &args.output,
        partition_offset: args.partition_offset,
        partition_size: args.size.map(|sz| sz - args.partition_offset),
        block_size: args.block_size,
        label: args.label.as_deref(),
        journal_size_mib: args.journal_size_mib,
        populate: args.populate.as_deref(),
    };

    if let Err(e) = format(&spec) {
        eprintln!("efs-mkfs: {e}");
        process::exit(1);
    }

    println!("Done.");
}
