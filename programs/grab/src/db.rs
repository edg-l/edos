//! What this machine has installed.
//!
//! Plain text under `/var/lib/grab/db`, one directory per package: `meta` for
//! what it is, `files` for exactly what it put on the disk. `files` is what
//! makes removal exact rather than a guess from the package's name.

use crate::{Error, Result};
use grab_index::Package;
use std::fs;

pub const DB: &str = "/var/lib/grab/db";

#[derive(Debug, Clone)]
pub struct Record {
    pub name: String,
    pub version: String,
    pub summary: String,
    pub category: String,
    pub files: Vec<String>,
}

pub fn write(package: &Package, files: &[String]) -> Result<()> {
    let dir = format!("{}/{}", DB, package.name);
    fs::create_dir_all(&dir).map_err(|e| Error::Io(format!("{}: {}", dir, e)))?;

    let meta = format!(
        "Package: {}\nVersion: {}\nSummary: {}\nCategory: {}\n",
        package.name, package.version, package.summary, package.category
    );
    fs::write(format!("{}/meta", dir), meta)
        .map_err(|e| Error::Io(format!("{}/meta: {}", dir, e)))?;

    let mut listing = files.join("\n");
    listing.push('\n');
    fs::write(format!("{}/files", dir), listing)
        .map_err(|e| Error::Io(format!("{}/files: {}", dir, e)))
}

pub fn read(name: &str) -> Result<Option<Record>> {
    let dir = format!("{}/{}", DB, name);
    let Ok(meta) = fs::read_to_string(format!("{}/meta", dir)) else {
        return Ok(None);
    };

    let field = |key: &str| -> String {
        meta.lines()
            .find_map(|line| line.strip_prefix(&format!("{}: ", key)))
            .unwrap_or_default()
            .to_string()
    };

    let files = fs::read_to_string(format!("{}/files", dir))
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    Ok(Some(Record {
        name: field("Package"),
        version: field("Version"),
        summary: field("Summary"),
        category: field("Category"),
        files,
    }))
}

/// Every installed package, in name order.
pub fn installed() -> Result<Vec<Record>> {
    let Ok(entries) = fs::read_dir(DB) else {
        // No database directory means nothing has been installed yet, which is
        // an ordinary state rather than a failure.
        return Ok(Vec::new());
    };

    let mut records = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(record) = read(&name)? {
            records.push(record);
        }
    }

    records.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(records)
}

pub fn forget(name: &str) -> Result<()> {
    let dir = format!("{}/{}", DB, name);
    fs::remove_dir_all(&dir).map_err(|e| Error::Io(format!("{}: {}", dir, e)))
}

/// Which installed package owns `path`, if any.
///
/// This is what keeps a package from replacing a file it did not put there:
/// anything not in this map either belongs to the base system or to nobody,
/// and in both cases a package has no business writing over it.
pub fn owner_of(path: &str) -> Result<Option<String>> {
    for record in installed()? {
        if record.files.iter().any(|f| f == path) {
            return Ok(Some(record.name));
        }
    }
    Ok(None)
}
