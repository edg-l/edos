use std::path::PathBuf;
use std::process;

use crate::exit_code::FsckExitCode;

pub struct Args {
    pub image: PathBuf,
    pub repair: bool,
    pub force: bool,
    pub yes: bool,
    pub dry_run: bool,
    pub verbose: bool,
    pub partition_offset: u64,
}

fn usage() -> ! {
    let program = std::env::args()
        .next()
        .and_then(|p| {
            PathBuf::from(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "efs-fsck".to_string());
    eprintln!(
        "Usage: {program} [OPTIONS] <IMAGE>

Arguments:
  <IMAGE>  EFS filesystem image or block device path

Options:
  --repair                       Enable repair mode (writes fixes to disk)
  --force                        Force check even if fsck_in_progress sentinel is set
  -n, --dry-run                  Report issues without writing any fixes
  -y, --yes                      Auto-accept all repair prompts
  -v, --verbose                  Print informational messages
  --partition-offset <BYTES>     Byte offset of EFS partition within the image (default: 0)
  --help                         Show this help
"
    );
    process::exit(FsckExitCode::UsageError.code());
}

pub fn parse_args() -> Args {
    let mut args = std::env::args().skip(1).peekable();
    let mut image: Option<PathBuf> = None;
    let mut repair = false;
    let mut force = false;
    let mut yes = false;
    let mut dry_run = false;
    let mut verbose = false;
    let mut partition_offset: u64 = 0;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => usage(),
            "--repair" => repair = true,
            "--force" => force = true,
            "-y" | "--yes" => yes = true,
            "-n" | "--dry-run" => dry_run = true,
            "-v" | "--verbose" => verbose = true,
            "--partition-offset" => {
                let val = args.next().unwrap_or_else(|| {
                    eprintln!("--partition-offset requires a value");
                    process::exit(FsckExitCode::UsageError.code());
                });
                partition_offset = val.parse().unwrap_or_else(|_| {
                    eprintln!("invalid partition offset: {val}");
                    process::exit(FsckExitCode::UsageError.code());
                });
            }
            s if s.starts_with('-') => {
                eprintln!("unknown option: {s}");
                usage();
            }
            _ => {
                if image.is_some() {
                    eprintln!("unexpected argument: {arg}");
                    process::exit(FsckExitCode::UsageError.code());
                }
                image = Some(PathBuf::from(arg));
            }
        }
    }

    let image = image.unwrap_or_else(|| {
        eprintln!("image path is required");
        usage();
    });

    Args {
        image,
        repair,
        force,
        yes,
        dry_run,
        verbose,
        partition_offset,
    }
}
