//! Check an unmounted EFS filesystem on a block device or image.
//!
//! The checker is `tools/efs-fsck` linked as a library, shared verbatim with
//! the host tool, so the two cannot reach different verdicts on the same image.
//!
//! A mounted device is refused rather than checked: the kernel owns its blocks
//! while it is mounted, and it replays the journal itself at mount time, which
//! is what covers the boot case. Checking the live root would need an
//! initramfs, which this system does not have.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::process;

use edos_lib::io::ioctl;
use efs_fsck::exit_code::FsckExitCode;

/// `BLOCK_IOCTL_IS_MOUNTED` from `kernel/src/fs/devfs/block.rs`.
const BLOCK_IOCTL_IS_MOUNTED: u64 = 0x424B_0004;

fn main() {
    let args = efs_fsck::parse_args();

    if let Ok(dev) = File::open(&args.image)
        && ioctl(dev.as_raw_fd() as u64, BLOCK_IOCTL_IS_MOUNTED, 0) == Ok(1)
    {
        eprintln!(
            "fsck: {} is mounted; unmount it before checking it",
            args.image.display()
        );
        process::exit(FsckExitCode::OperationalError.code());
    }

    efs_fsck::run(args);
}
