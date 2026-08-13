//! What is in a directory, which volume it sits on, and how to say either out
//! loud.

use std::fs;
use std::time::UNIX_EPOCH;

use edos_lib::mounts::{self, Filesystem, Mount};
use edos_lib::time::{ClockTime, utc_offset_seconds};

/// What a listed name is. Classified without following it, so a link to a
/// directory still reads as a link.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The way back out of this directory. Always the first row.
    Parent,
    Dir,
    File,
    Link,
    /// A device node, or anything else that is neither a file nor a directory.
    Special,
}

impl Kind {
    /// The word for this kind in the kind column, for everything except a file:
    /// a file is named by its extension, which says more.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Parent => "up",
            Kind::Dir => "dir",
            Kind::File => "file",
            Kind::Link => "link",
            Kind::Special => "dev",
        }
    }
}

/// One row of a listing.
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    /// Bytes, for the things a size means something for.
    pub size: Option<u64>,
    /// Where a symbolic link points, written as the link writes it.
    pub target: Option<String>,
    /// Whether opening this row leads into a directory. True for a directory,
    /// for the parent row, and for a link that resolves to one.
    pub opens_directory: bool,
    /// Seconds since the Unix epoch, when the filesystem records one.
    pub modified: Option<i64>,
}

impl Entry {
    /// What the kind column says: the extension for a file, the kind for
    /// everything else.
    pub fn kind_label(&self) -> String {
        if self.kind != Kind::File {
            return self.kind.label().to_string();
        }
        match extension(&self.name) {
            Some(ext) if ext.len() <= 5 => ext.to_lowercase(),
            _ => "file".to_string(),
        }
    }

    /// Whether this is an image the picture viewer can open.
    pub fn is_image(&self) -> bool {
        extension(&self.name).is_some_and(|ext| ext.eq_ignore_ascii_case("bmp"))
    }
}

/// The part of a name after its last dot, when it has one that is not the whole
/// name. A leading dot makes a hidden file, not an extension.
pub fn extension(name: &str) -> Option<&str> {
    let at = name.rfind('.')?;
    if at == 0 || at + 1 == name.len() {
        return None;
    }
    Some(&name[at + 1..])
}

/// Read a directory into sorted rows, with the way out on top.
///
/// The error is a sentence for the status bar rather than an error type: there
/// is one caller and what it does with a failure is say so.
pub fn read_dir(path: &str) -> Result<Vec<Entry>, String> {
    let reader = fs::read_dir(path).map_err(|err| format!("Can't open {path}: {err}"))?;

    let mut entries = Vec::new();
    if path != "/" {
        entries.push(Entry {
            name: "..".to_string(),
            kind: Kind::Parent,
            size: None,
            target: None,
            opens_directory: true,
            modified: None,
        });
    }

    for item in reader.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        let full = join(path, &name);
        let file_type = item.file_type().ok();

        let kind = match file_type {
            Some(t) if t.is_symlink() => Kind::Link,
            Some(t) if t.is_dir() => Kind::Dir,
            Some(t) if t.is_file() => Kind::File,
            _ => Kind::Special,
        };

        // One stat of the link itself carries the size and the timestamp; the
        // second, which follows the link, is asked only of links, and only to
        // learn whether opening one lands in a directory.
        let meta = fs::symlink_metadata(&full).ok();
        let resolved = (kind == Kind::Link)
            .then(|| fs::metadata(&full).ok())
            .flatten();

        entries.push(Entry {
            name,
            kind,
            size: (kind == Kind::File)
                .then(|| meta.as_ref().map(|m| m.len()))
                .flatten(),
            target: (kind == Kind::Link)
                .then(|| fs::read_link(&full).ok())
                .flatten()
                .map(|t| t.to_string_lossy().into_owned()),
            opens_directory: match kind {
                Kind::Dir | Kind::Parent => true,
                Kind::Link => resolved.is_some_and(|m| m.is_dir()),
                _ => false,
            },
            modified: meta.as_ref().and_then(unix_seconds),
        });
    }

    // Directories first, then everything else, each group by name. Case is
    // ignored so `Boot` does not sort ahead of every lowercase name.
    entries.sort_by(|a, b| {
        let group = |e: &Entry| match e.kind {
            Kind::Parent => 0,
            Kind::Dir => 1,
            _ => 2,
        };
        group(a)
            .cmp(&group(b))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

/// Seconds since the Unix epoch for a file's modification time, when the
/// filesystem keeps one.
fn unix_seconds(meta: &fs::Metadata) -> Option<i64> {
    let modified = meta.modified().ok()?;
    let since = modified.duration_since(UNIX_EPOCH).ok()?;
    Some(since.as_secs() as i64)
}

/// Join a directory and a name into a path, without doubling the separator at
/// the root.
pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The directory holding `path`, or `path` itself when it is already the root.
pub fn parent(path: &str) -> String {
    match path.rfind('/') {
        None | Some(0) => "/".to_string(),
        Some(at) => path[..at].to_string(),
    }
}

/// The components of a path, in order, each with the full path that reaches it.
/// The root is always the first, named by its own separator.
pub fn breadcrumbs(path: &str) -> Vec<(String, String)> {
    let mut crumbs = vec![("/".to_string(), "/".to_string())];
    let mut walked = String::new();
    for part in path.split('/').filter(|part| !part.is_empty()) {
        walked = join(&walked, part);
        crumbs.push((part.to_string(), walked.clone()));
    }
    crumbs
}

/// A row in the places rail: one mounted filesystem, or the home directory.
pub struct Place {
    pub label: String,
    pub path: String,
    pub note: String,
    pub is_home: bool,
    /// Whether there is storage behind it, which decides the icon.
    pub persistent: bool,
}

/// The rail's contents: home first, since it is where a person starts, then
/// every mount the kernel reports, in the order it reports them.
pub fn places(mounts: &[Mount]) -> Vec<Place> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let mut places = vec![Place {
        label: "Home".to_string(),
        path: home,
        note: String::new(),
        is_home: true,
        persistent: true,
    }];

    for mount in mounts {
        places.push(Place {
            label: mount_label(&mount.path),
            path: mount.path.clone(),
            note: mount.filesystem.name().to_string(),
            is_home: false,
            persistent: mount.filesystem.is_persistent(),
        });
    }
    places
}

/// What to call a mount point in the rail. The four the system always has get
/// names a person would use; anything else is called by its own last component.
fn mount_label(path: &str) -> String {
    match path {
        "/" => "Root".to_string(),
        "/dev" => "Devices".to_string(),
        "/proc" => "Kernel".to_string(),
        "/tmp" => "Temporary".to_string(),
        _ => {
            let last = path.rsplit('/').next().unwrap_or(path);
            let mut chars = last.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => path.to_string(),
            }
        }
    }
}

/// The volume a path is served by, with how full it is.
pub struct Volume {
    pub filesystem: Filesystem,
    pub mount_point: String,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

impl Volume {
    /// How full the volume is, or None when it reports no size, which is what
    /// the filesystems with nothing behind them do.
    pub fn used_fraction(&self) -> Option<f32> {
        if self.total_bytes == 0 {
            return None;
        }
        Some((self.total_bytes - self.free_bytes) as f32 / self.total_bytes as f32)
    }
}

/// Which volume `path` lives on, asked of the mount table rather than guessed
/// from the path.
pub fn volume_of(mounts: &[Mount], path: &str) -> Option<Volume> {
    let mount = mounts::covering(mounts, path)?;
    let stat = mounts::statfs(&mount.path);
    Some(Volume {
        filesystem: mount.filesystem,
        mount_point: mount.path.clone(),
        free_bytes: stat.as_ref().map_or(0, |s| s.free_bytes()),
        total_bytes: stat.as_ref().map_or(0, |s| s.total_bytes()),
    })
}

/// A size in the shortest form that stays honest: whole bytes below a kibibyte,
/// one decimal above it.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1024 * 1024 * 1024, "G"), (1024 * 1024, "M"), (1024, "K")];
    for (scale, suffix) in UNITS {
        if bytes >= scale {
            return format!("{:.1}{suffix}", bytes as f64 / scale as f64);
        }
    }
    format!("{bytes}")
}

/// The exact byte count, grouped in threes so a long number can be read.
pub fn format_exact(bytes: u64) -> String {
    let digits = bytes.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(' ');
        }
        grouped.push(digit);
    }
    format!("{grouped} bytes")
}

/// A timestamp as the local date and time.
pub fn format_time(unix_seconds: i64) -> String {
    let local = ClockTime::from_unix_secs(unix_seconds + utc_offset_seconds() as i64);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.year, local.month, local.day, local.hour, local.minute
    )
}
