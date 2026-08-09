//! Create an EFS filesystem on a device or image.
//!
//! The formatter itself lives in `tools/efs-mkfs`, shared verbatim with the
//! host tool, so a filesystem written here and one written on the host cannot
//! diverge.

fn main() {
    efs_mkfs::cli_main();
}
