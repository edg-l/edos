//! Settings that outlive a boot.
//!
//! One setting per file under `/etc`, holding one value, with `#` comments
//! allowed above it. That is smaller than a registry and smaller than an ini
//! parser, and it is what the shell can already edit with `echo` and read with
//! `cat` when the graphical program that owns a setting will not start.
//!
//! `/etc` is on the root filesystem, so this persists on an installed machine
//! and is forgotten by a live session, whose root is a ramdisk. That is the
//! right split: a live session should leave the machine as it found it.

use std::{fs, io, path::Path};

/// The directory every setting lives in.
pub const ETC: &str = "/etc";

/// Keyboard layout name; see [`crate::keymap`].
pub const KEYMAP: &str = "/etc/keymap";

/// Path of the desktop background, or the name of a generated one.
pub const WALLPAPER: &str = "/etc/wallpaper";

/// Command history, kept out of `/etc` because it is a record rather than a
/// setting, and on the root filesystem rather than in `/tmp`, which is memfs
/// and forgets at every boot.
pub const SH_HISTORY: &str = "/root/.sh_history";

/// The value in `path`, or None if the file is absent or holds only comments.
///
/// The first line that is neither blank nor a comment is the value, so a file
/// can explain itself to whoever opens it next.
pub fn read(path: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
}

/// Record `value` in `path`, with `comment` above it.
///
/// Creates the parent directory: a root that never had `/etc` should gain one
/// by being configured rather than refuse the setting.
pub fn write(path: &str, value: &str, comment: &str) -> io::Result<()> {
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let mut contents = String::new();
    for line in comment.lines() {
        contents.push_str("# ");
        contents.push_str(line);
        contents.push('\n');
    }
    contents.push_str(value);
    contents.push('\n');
    fs::write(path, contents)
}
