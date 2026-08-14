//! The `grab` package manager, as a library so the CLI and the GUI run the
//! same code rather than the GUI shelling out to the CLI and parsing it back.

pub mod db;
pub mod install;
pub mod repo;
pub mod trust;

use grab_index::{Index, Package};
use std::fmt;

/// Where the repository lives unless `/etc/grab/repo` names another.
pub const DEFAULT_REPO: &str = "https://edos.edgl.dev/pkg";
/// Base URL of the repository, as a setting that outlives a boot.
pub const REPO_SETTING: &str = "/etc/grab/repo";

#[derive(Debug)]
pub enum Error {
    Http(edos_http::Error),
    Io(String),
    /// The index or a package failed verification. Kept separate from every
    /// other failure because it is the one that means someone may be lying to
    /// this machine, rather than that something is merely broken.
    Untrusted(String),
    NotFound(String),
    Conflict(String),
    Malformed(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Http(e) => write!(f, "{}", e),
            Error::Io(what) => write!(f, "{}", what),
            Error::Untrusted(what) => write!(f, "refusing to trust this: {}", what),
            Error::NotFound(what) => write!(f, "{}", what),
            Error::Conflict(what) => write!(f, "{}", what),
            Error::Malformed(what) => write!(f, "{}", what),
        }
    }
}

impl std::error::Error for Error {}

impl From<edos_http::Error> for Error {
    fn from(e: edos_http::Error) -> Self {
        Error::Http(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// What a caller wants told about a long operation.
pub trait Progress {
    fn message(&mut self, text: &str);
    fn transfer(&mut self, _done: u64, _total: Option<u64>) {}
}

/// A progress sink that discards everything, for callers that do not report.
pub struct Silent;

impl Progress for Silent {
    fn message(&mut self, _text: &str) {}
}

/// The base URL of the repository this machine uses.
pub fn repo_url() -> String {
    edos_lib::config::read(REPO_SETTING).unwrap_or_else(|| DEFAULT_REPO.to_string())
}

/// Fetch, verify and cache the index.
pub fn update(progress: &mut dyn Progress) -> Result<Index> {
    let url = repo_url();
    progress.message(&format!("fetching {}/index", url));
    let index = repo::fetch_index(&url)?;
    progress.message(&format!(
        "index serial {}, {} package(s)",
        index.serial,
        index.packages.len()
    ));
    Ok(index)
}

/// The cached index, fetching one if there is none.
pub fn index(progress: &mut dyn Progress) -> Result<Index> {
    match repo::cached_index() {
        Some(index) => Ok(index),
        None => update(progress),
    }
}

pub fn install(name: &str, progress: &mut dyn Progress) -> Result<()> {
    let index = index(progress)?;
    let package = index
        .get(name)
        .ok_or_else(|| Error::NotFound(format!("no package named {}", name)))?;

    if let Some(installed) = db::read(name)?
        && installed.version == package.version
    {
        progress.message(&format!(
            "{} {} is already installed",
            name, package.version
        ));
        return Ok(());
    }

    install_package(&repo_url(), package, progress)
}

pub fn install_package(base: &str, package: &Package, progress: &mut dyn Progress) -> Result<()> {
    progress.message(&format!(
        "fetching {} {} ({} bytes)",
        package.name, package.version, package.size
    ));
    let archive = repo::fetch_package(base, package, progress)?;

    progress.message(&format!("installing {}", package.name));
    let installed = install::apply(&package.name, &archive)?;
    let seeded = seed(&package.name, &installed, progress)?;

    db::write(package, &installed, &seeded)?;
    progress.message(&format!("{} {} installed", package.name, package.version));
    Ok(())
}

/// Copy the package's defaults into `/etc` and report every one that landed.
///
/// Each is announced rather than done quietly: a seeded
/// `/etc/services/<name>.conf` is what makes `edos-init` supervise a packaged
/// daemon, so an install can change what the machine runs at boot and the
/// person running it has to be able to see that.
///
/// An upgrade seeds nothing, because the destinations exist by then, so the
/// previous install's list carries forward — otherwise removal would forget the
/// settings the first install created.
fn seed(name: &str, installed: &[String], progress: &mut dyn Progress) -> Result<Vec<String>> {
    let mut seeded = db::read(name)?.map(|r| r.seeded).unwrap_or_default();

    for path in install::seed_etc(installed)? {
        progress.message(&format!("wrote {}, which had no setting before", path));
        if !seeded.contains(&path) {
            seeded.push(path);
        }
    }

    Ok(seeded)
}

/// Install a local archive without checking any signature.
///
/// This is the one path that skips verification, and it exists so a package can
/// be tested before it is published. It takes a filesystem path and never a
/// URL: the moment an unsigned install could be pointed at the network, the
/// guarantee that everything `grab` fetches is signed would be gone.
pub fn install_local(path: &str, progress: &mut dyn Progress) -> Result<()> {
    if path.contains("://") {
        return Err(Error::Untrusted(format!(
            "{} is a URL; --allow-unsigned takes a local file only",
            path
        )));
    }

    let archive = std::fs::read(path).map_err(|e| Error::Io(format!("{}: {}", path, e)))?;
    let (name, version) = name_and_version(path);

    progress.message(&format!(
        "installing {} {} from {} WITHOUT verifying a signature",
        name, version, path
    ));

    let installed = install::apply(&name, &archive)?;
    let seeded = seed(&name, &installed, progress)?;
    let package = Package {
        name: name.clone(),
        version,
        summary: format!("installed from {}", path),
        category: "local".to_string(),
        size: archive.len() as u64,
        sha256: String::new(),
        file: path.to_string(),
        icon: None,
        installs: installed.clone(),
    };
    db::write(&package, &installed, &seeded)?;

    progress.message(&format!("{} installed", name));
    Ok(())
}

/// Split `<name>-<version>.tar.gz`, the form the repository publishes.
///
/// A file that does not follow it still installs; it is recorded as version
/// `local` so `upgrade` has nothing to compare and leaves it alone.
fn name_and_version(path: &str) -> (String, String) {
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = file
        .strip_suffix(".tar.gz")
        .or_else(|| file.strip_suffix(".tgz"))
        .unwrap_or(file);

    match stem.rsplit_once('-') {
        Some((name, version))
            if !name.is_empty() && version.starts_with(|c: char| c.is_ascii_digit()) =>
        {
            (name.to_string(), version.to_string())
        }
        _ => (stem.to_string(), "local".to_string()),
    }
}

pub fn remove(name: &str, progress: &mut dyn Progress) -> Result<()> {
    let record =
        db::read(name)?.ok_or_else(|| Error::NotFound(format!("{} is not installed", name)))?;

    // Before the files: this compares each seeded setting against the packaged
    // default it came from, and that default is one of the files below.
    for kept in install::unseed(&record.seeded, &record.files)? {
        progress.message(&format!("kept {}, which has been edited since", kept));
    }

    install::remove_files(&record.files)?;
    db::forget(name)?;
    progress.message(&format!("{} removed", name));
    Ok(())
}

/// Install every package whose repository version differs from the installed
/// one. Named packages, or all of them when none are named.
pub fn upgrade(names: &[String], progress: &mut dyn Progress) -> Result<usize> {
    let index = update(progress)?;
    let base = repo_url();

    let wanted: Vec<String> = if names.is_empty() {
        db::installed()?.into_iter().map(|r| r.name).collect()
    } else {
        names.to_vec()
    };

    let mut changed = 0;
    for name in wanted {
        let Some(record) = db::read(&name)? else {
            progress.message(&format!("{} is not installed", name));
            continue;
        };
        let Some(package) = index.get(&name) else {
            progress.message(&format!("{} is no longer in the repository", name));
            continue;
        };
        if package.version == record.version {
            continue;
        }
        progress.message(&format!(
            "{}: {} -> {}",
            name, record.version, package.version
        ));
        install_package(&base, package, progress)?;
        changed += 1;
    }

    Ok(changed)
}
