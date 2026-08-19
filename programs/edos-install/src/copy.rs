//! Copying the live root onto the freshly formatted target.

use std::fs;
use std::io;
use std::path::Path;

/// Copy one file's contents.
pub fn copy_file(from: &Path, to: &Path) -> io::Result<()> {
    crate::klog::trace(&format!("copy {}", from.display()));
    let data = fs::read(from).map_err(|e| at(from, "read", e))?;
    fs::write(to, data).map_err(|e| at(to, "write", e))
}

/// Attach the path and operation to an error, so a failure halfway through a
/// recursive copy names the file it stopped on.
fn at(path: &Path, op: &str, e: io::Error) -> io::Error {
    io::Error::other(format!("{op} {}: {e}", path.display()))
}

/// Copy `src` into `dst` recursively, skipping the named top-level entries.
///
/// The skip list is what keeps mount points out of the copy: `/dev`, `/proc`
/// and `/tmp` belong to the running system, and `/mnt` holds the target we are
/// writing into.
pub fn copy_root(src: &str, dst: &str, skip_top_level: &[&str]) -> io::Result<usize> {
    let mut copied = 0;
    for entry in fs::read_dir(src).map_err(|e| at(Path::new(src), "read_dir", e))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if skip_top_level.contains(&name.as_ref()) {
            continue;
        }
        copied += copy_tree(&entry.path(), &Path::new(dst).join(name.as_ref()))?;
    }
    Ok(copied)
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<usize> {
    let meta = fs::metadata(from).map_err(|e| at(from, "stat", e))?;
    if !meta.is_dir() {
        // Only a regular file has contents to copy. A FIFO, a socket or a
        // device node is live state belonging to the running system, and
        // reading one is not merely pointless: opening `/var/run/svc.ctl`,
        // the services control pipe, parks until a writer appears, which on
        // the target filesystem is never.
        if !meta.is_file() {
            crate::klog::trace(&format!("skip {} (not a regular file)", from.display()));
            return Ok(0);
        }
        copy_file(from, to)?;
        return Ok(1);
    }

    crate::klog::trace(&format!("mkdir {}", to.display()));
    if let Err(e) = fs::create_dir(to)
        && e.kind() != io::ErrorKind::AlreadyExists {
            return Err(at(to, "mkdir", e));
        }

    let mut copied = 0;
    for entry in fs::read_dir(from).map_err(|e| at(from, "read_dir", e))? {
        let entry = entry?;
        copied += copy_tree(&entry.path(), &to.join(entry.file_name()))?;
    }
    Ok(copied)
}
