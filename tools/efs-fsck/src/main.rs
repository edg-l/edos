//! Host-side EFS checker. The checker itself is the `efs_fsck` library, shared
//! with the in-EDOS `/bin/fsck`.

fn main() {
    efs_fsck::run(efs_fsck::parse_args());
}
