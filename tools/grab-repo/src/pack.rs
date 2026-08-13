//! Writing a package archive.
//!
//! Every field that does not describe the contents is pinned to a constant, so
//! packing the same files twice produces the same bytes. Without that, a
//! republish of an unchanged repository changes every SHA-256 in the index and
//! a client is told to download software that did not change.

use flate2::{Compression, GzBuilder};
use std::{fs, path::Path, path::PathBuf};

pub struct Item {
    /// Where the file lands, relative to `/`.
    pub path: String,
    pub source: PathBuf,
    pub mode: u32,
}

pub fn write_archive(archive: &Path, items: &[Item]) -> Result<(), String> {
    let mut tarball = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tarball);
        for item in items {
            let contents =
                fs::read(&item.source).map_err(|e| format!("{}: {}", item.source.display(), e))?;

            // ustar rather than the tar crate's default GNU format: it is what
            // the guest's own `tar` decodes.
            let mut header = tar::Header::new_ustar();
            header
                .set_path(&item.path)
                .map_err(|e| format!("{}: {}", item.path, e))?;
            header.set_size(contents.len() as u64);
            header.set_mode(item.mode);
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();

            builder
                .append(&header, contents.as_slice())
                .map_err(|e| format!("{}: {}", item.path, e))?;
        }
        builder.finish().map_err(|e| e.to_string())?;
    }

    // The gzip header carries an mtime of its own, which would otherwise change
    // on every run for reasons that have nothing to do with the contents.
    let file = fs::File::create(archive).map_err(|e| format!("{}: {}", archive.display(), e))?;
    let mut encoder = GzBuilder::new().mtime(0).write(file, Compression::best());
    std::io::Write::write_all(&mut encoder, &tarball).map_err(|e| e.to_string())?;
    encoder.finish().map_err(|e| e.to_string())?;

    Ok(())
}
