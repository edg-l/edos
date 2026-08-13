//! Unpacking a package onto the filesystem.
//!
//! The archive is treated as hostile input even though it was signed: a
//! signature says who published something, not that what they published is
//! sane. Every path is checked, everything is unpacked to a staging directory
//! first, and nothing is moved into place until the whole archive has been
//! read and every destination has been cleared.

use crate::{Error, Result, db, repo};
use flate2::read::GzDecoder;
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
};
use ustar::{BLOCK, Decoded, Kind};

/// The only top-level directories a package may write into.
///
/// `etc` is deliberately absent: settings belong to the machine, not to a
/// package, and a package that could write `/etc` could change where `grab`
/// itself looks for its repository.
const ALLOWED_ROOTS: [&str; 4] = ["bin", "lib", "share", "opt"];

/// Unpack `archive` and move it into place, returning every path installed.
pub fn apply(name: &str, archive: &[u8]) -> Result<Vec<String>> {
    let stage = format!("{}/stage/{}", repo::CACHE, name);
    // A staging directory left over from an interrupted run would otherwise
    // contribute its files to this install.
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir_all(&stage).map_err(|e| Error::Io(format!("{}: {}", stage, e)))?;

    let staged = unpack(archive, &stage)?;
    if staged.is_empty() {
        return Err(Error::Malformed(format!("{} contains no files", name)));
    }

    check_ownership(name, &staged)?;

    let mut installed = Vec::new();
    for path in &staged {
        let destination = format!("/{}", path);
        if let Some(parent) = Path::new(&destination).parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Error::Io(format!("{}: {}", parent.display(), e)))?;
        }
        let from = format!("{}/{}", stage, path);
        // Rename rather than copy: it is atomic, so a running program is never
        // observed half-replaced. Staging lives under /var alongside the
        // destinations, which is what keeps this within one filesystem.
        fs::rename(&from, &destination)
            .map_err(|e| Error::Io(format!("{}: {}", destination, e)))?;
        installed.push(path.clone());
    }

    let _ = fs::remove_dir_all(&stage);
    Ok(installed)
}

/// Walk the archive, writing each regular file under `stage`.
fn unpack(archive: &[u8], stage: &str) -> Result<Vec<String>> {
    let mut reader = GzDecoder::new(archive);
    let mut staged = Vec::new();

    loop {
        let mut block = [0u8; BLOCK];
        if !read_exact_or_end(&mut reader, &mut block)? {
            break;
        }

        let entry = match ustar::decode(&block).map_err(Error::Malformed)? {
            Decoded::End => break,
            Decoded::Entry(entry) => entry,
        };

        let padded = entry.size.div_ceil(BLOCK as u64) * BLOCK as u64;

        match entry.kind {
            Kind::File => {
                let path = validate(&entry.name)?;
                let destination = format!("{}/{}", stage, path);
                if let Some(parent) = Path::new(&destination).parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| Error::Io(format!("{}: {}", parent.display(), e)))?;
                }
                write_file(&mut reader, &destination, entry.size, entry.mode)?;
                skip(&mut reader, padded - entry.size)?;
                staged.push(path);
            }
            // A directory entry needs nothing: the parents of every file are
            // created as that file is written. Its name is still validated so
            // a malformed archive is reported rather than ignored.
            Kind::Dir => {
                validate(entry.name.trim_end_matches('/'))?;
                skip(&mut reader, padded)?;
            }
            // Symlinks are refused rather than followed. A link is the
            // straightforward way to make a later entry in the same archive
            // write outside the tree the path checks are guarding.
            Kind::Symlink => {
                return Err(Error::Malformed(format!(
                    "{} is a symlink, which a package may not contain",
                    entry.name
                )));
            }
        }
    }

    Ok(staged)
}

/// Check one archive path, returning it in the form it will be installed under.
///
/// Refusals here are the difference between a package and an arbitrary write
/// to the filesystem.
fn validate(name: &str) -> Result<String> {
    let refuse = |why: &str| Error::Malformed(format!("{}: {}", name, why));

    if name.is_empty() {
        return Err(refuse("an empty path"));
    }
    if name.starts_with('/') {
        return Err(refuse("an absolute path"));
    }
    if name.contains('\\') {
        return Err(refuse("a backslash in a path"));
    }

    let mut components = Vec::new();
    for component in name.split('/') {
        match component {
            // `..` is the whole reason this function exists.
            ".." => return Err(refuse("a `..` component")),
            "." | "" => continue,
            other => components.push(other),
        }
    }

    if components.is_empty() {
        return Err(refuse("no path left after normalisation"));
    }
    if !ALLOWED_ROOTS.contains(&components[0]) {
        return Err(refuse(&format!(
            "packages may only write into {}",
            ALLOWED_ROOTS.join(", ")
        )));
    }

    Ok(components.join("/"))
}

/// Refuse to overwrite anything this package does not already own.
///
/// A path that exists but belongs to no installed package is part of the base
/// system, and a package replacing `/bin/sh` by being named carefully is
/// exactly what this prevents.
fn check_ownership(name: &str, paths: &[String]) -> Result<()> {
    for path in paths {
        let destination = format!("/{}", path);
        if !fs::exists(&destination).unwrap_or(false) {
            continue;
        }
        match db::owner_of(path)? {
            Some(owner) if owner == name => {}
            Some(owner) => {
                return Err(Error::Conflict(format!(
                    "{} is owned by {}",
                    destination, owner
                )));
            }
            None => {
                return Err(Error::Conflict(format!(
                    "{} already exists and belongs to the base system",
                    destination
                )));
            }
        }
    }
    Ok(())
}

pub fn remove_files(files: &[String]) -> Result<()> {
    for path in files {
        let destination = format!("/{}", path);
        match fs::remove_file(&destination) {
            Ok(()) => {}
            // A file already gone is the state removal wanted, so it is not an
            // error; anything else is.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::Io(format!("{}: {}", destination, e))),
        }
    }
    Ok(())
}

fn write_file(reader: &mut dyn Read, path: &str, size: u64, mode: u32) -> Result<()> {
    let mut file = File::create(path).map_err(|e| Error::Io(format!("{}: {}", path, e)))?;
    let mut left = size;
    let mut buf = vec![0u8; 32 * 1024];

    while left > 0 {
        let want = (left.min(buf.len() as u64)) as usize;
        let n = reader
            .read(&mut buf[..want])
            .map_err(|e| Error::Io(format!("{}: {}", path, e)))?;
        if n == 0 {
            return Err(Error::Malformed(format!(
                "the archive ends {} bytes into {}",
                left, path
            )));
        }
        file.write_all(&buf[..n])
            .map_err(|e| Error::Io(format!("{}: {}", path, e)))?;
        left -= n as u64;
    }

    file.flush()
        .map_err(|e| Error::Io(format!("{}: {}", path, e)))?;

    // The archive's mode is read and deliberately not applied: this system has
    // no permission bits and no chmod-shaped syscall, and `sys_access` grants
    // X_OK unconditionally, so an installed binary is runnable regardless.
    // Honouring the bit would mean `fs::set_permissions`, which reports
    // Unsupported here.
    let _ = mode;
    Ok(())
}

fn skip(reader: &mut dyn Read, mut count: u64) -> Result<()> {
    let mut buf = [0u8; BLOCK];
    while count > 0 {
        let want = (count.min(BLOCK as u64)) as usize;
        let n = reader
            .read(&mut buf[..want])
            .map_err(|e| Error::Io(format!("read: {}", e)))?;
        if n == 0 {
            break;
        }
        count -= n as u64;
    }
    Ok(())
}

/// Fill `block`. A stream that ends exactly on a block boundary is a finished
/// archive; one that ends part way through a block is a truncated one.
fn read_exact_or_end(reader: &mut dyn Read, block: &mut [u8; BLOCK]) -> Result<bool> {
    let mut filled = 0;
    while filled < BLOCK {
        let n = reader
            .read(&mut block[filled..])
            .map_err(|e| Error::Io(format!("read: {}", e)))?;
        if n == 0 {
            if filled == 0 {
                return Ok(false);
            }
            return Err(Error::Malformed("a truncated archive".to_string()));
        }
        filled += n;
    }
    Ok(true)
}
