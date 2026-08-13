//! Reading `pkg.toml`, the file that says a program is a package.

use serde::Deserialize;
use std::{fs, path::Path, path::PathBuf};

/// What one program declares about itself.
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub summary: String,
    pub category: String,
    /// Icon path, relative to the program's own directory.
    pub icon: Option<String>,
    pub shipped: bool,
    pub directory: PathBuf,
}

#[derive(Deserialize)]
struct PkgFile {
    summary: String,
    category: Option<String>,
    icon: Option<String>,
    /// `false` means the program is published rather than put on the image.
    /// Absent means shipped, so adding a `pkg.toml` for its metadata alone
    /// never removes a program from the image by surprise.
    shipped: Option<bool>,
}

#[derive(Deserialize)]
struct CargoFile {
    package: CargoPackage,
}

#[derive(Deserialize)]
struct CargoPackage {
    name: String,
    version: String,
}

/// Every `pkg.toml` under `programs`, in name order.
pub fn scan(programs: &Path) -> Result<Vec<Manifest>, String> {
    let entries = fs::read_dir(programs).map_err(|e| format!("{}: {}", programs.display(), e))?;

    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let directory = entry.path();
        let pkg = directory.join("pkg.toml");
        if !pkg.exists() {
            continue;
        }
        found.push(read_one(&directory, &pkg)?);
    }

    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

fn read_one(directory: &Path, pkg: &Path) -> Result<Manifest, String> {
    let text = fs::read_to_string(pkg).map_err(|e| format!("{}: {}", pkg.display(), e))?;
    let declared: PkgFile =
        toml::from_str(&text).map_err(|e| format!("{}: {}", pkg.display(), e))?;

    // The version comes from Cargo.toml rather than from pkg.toml. Two places
    // to write a version is one place for them to disagree, and cargo's is the
    // one that built the binary.
    let cargo_path = directory.join("Cargo.toml");
    let cargo_text =
        fs::read_to_string(&cargo_path).map_err(|e| format!("{}: {}", cargo_path.display(), e))?;
    let cargo: CargoFile =
        toml::from_str(&cargo_text).map_err(|e| format!("{}: {}", cargo_path.display(), e))?;

    Ok(Manifest {
        name: cargo.package.name,
        version: cargo.package.version,
        summary: declared.summary,
        category: declared.category.unwrap_or_else(|| "misc".to_string()),
        icon: declared.icon,
        shipped: declared.shipped.unwrap_or(true),
        directory: directory.to_path_buf(),
    })
}
