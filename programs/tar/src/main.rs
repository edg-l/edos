//! tar - create, list and extract ustar archives

mod header;

use header::{BLOCK, Decoded, Entry, Kind};
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::UNIX_EPOCH;

const USAGE: &str = "usage: tar -c|-t|-x [-v] [-f archive] [-C dir] [path...]";

#[derive(PartialEq, Eq, Clone, Copy)]
enum Mode {
    Create,
    List,
    Extract,
}

struct Options {
    mode: Option<Mode>,
    verbose: bool,
    archive: Option<String>,
    directory: Option<String>,
    paths: Vec<String>,
}

fn main() -> ExitCode {
    let opts = match parse_args(env::args().skip(1)) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("tar: {}\n{}", e, USAGE);
            return ExitCode::FAILURE;
        }
    };

    let Some(mode) = opts.mode else {
        eprintln!("tar: one of -c, -t or -x is required\n{}", USAGE);
        return ExitCode::FAILURE;
    };

    if let Some(dir) = &opts.directory
        && let Err(e) = env::set_current_dir(dir)
    {
        eprintln!("tar: {}: {}", dir, e);
        return ExitCode::FAILURE;
    }

    let result = match mode {
        Mode::Create => create(&opts),
        Mode::List | Mode::Extract => read_archive(&opts, mode == Mode::Extract),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tar: {}", e);
            ExitCode::FAILURE
        }
    }
}

/// Accept both the clustered form (`-xvf a.tar`) and separate flags. `f` and
/// `C` take the next argument when they end a cluster.
fn parse_args(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut opts = Options {
        mode: None,
        verbose: false,
        archive: None,
        directory: None,
        paths: Vec::new(),
    };
    let mut pending: Option<char> = None;

    for arg in args {
        if let Some(flag) = pending.take() {
            match flag {
                'f' => opts.archive = Some(arg),
                _ => opts.directory = Some(arg),
            }
            continue;
        }

        if arg.starts_with('-') && arg.len() > 1 {
            for c in arg[1..].chars() {
                match c {
                    'c' => opts.mode = Some(Mode::Create),
                    't' => opts.mode = Some(Mode::List),
                    'x' => opts.mode = Some(Mode::Extract),
                    'v' => opts.verbose = true,
                    'f' | 'C' => pending = Some(c),
                    _ => return Err(format!("unknown option '-{}'", c)),
                }
            }
        } else {
            opts.paths.push(arg);
        }
    }

    if let Some(flag) = pending {
        return Err(format!("option '-{}' needs an argument", flag));
    }
    Ok(opts)
}

/// `-f -`, or no `-f` at all, means the standard stream.
fn is_stdio(archive: &Option<String>) -> bool {
    archive.as_deref().map(|a| a == "-").unwrap_or(true)
}

fn create(opts: &Options) -> Result<(), String> {
    if opts.paths.is_empty() {
        return Err("nothing to archive".to_string());
    }

    let stdout = io::stdout();
    let mut out: Box<dyn Write> = if is_stdio(&opts.archive) {
        Box::new(stdout.lock())
    } else {
        let path = opts.archive.as_deref().unwrap();
        Box::new(File::create(path).map_err(|e| format!("{}: {}", path, e))?)
    };

    let mut failed = false;
    for path in &opts.paths {
        // A leading slash is dropped so an archive never writes outside the
        // directory it is unpacked in.
        let name = path.trim_start_matches('/').trim_end_matches('/');
        if name.is_empty() {
            eprintln!("tar: {}: refusing to archive the root", path);
            failed = true;
            continue;
        }
        if let Err(e) = archive_path(&mut out, path, name, opts.verbose) {
            eprintln!("tar: {}: {}", path, e);
            failed = true;
        }
    }

    // Two zero blocks close the archive.
    out.write_all(&[0u8; BLOCK * 2])
        .map_err(|e| format!("write: {}", e))?;
    out.flush().map_err(|e| format!("write: {}", e))?;

    if failed {
        Err("archive is incomplete".to_string())
    } else {
        Ok(())
    }
}

/// Add `path` to the archive under the stored name `name`, recursing into
/// directories.
fn archive_path(
    out: &mut dyn Write,
    path: &str,
    name: &str,
    verbose: bool,
) -> Result<(), io::Error> {
    // `read_link` succeeding is what identifies a symlink: this target's
    // `symlink_metadata` follows links, so the file type cannot say.
    if let Ok(target) = fs::read_link(path) {
        let entry = Entry {
            name: name.to_string(),
            kind: Kind::Symlink,
            size: 0,
            mtime: mtime_of(path),
            mode: 0o777,
            link: target.to_string_lossy().into_owned(),
        };
        return write_entry(out, &entry, verbose);
    }

    let meta = fs::metadata(path)?;
    if meta.is_dir() {
        let entry = Entry {
            name: format!("{}/", name),
            kind: Kind::Dir,
            size: 0,
            mtime: mtime_of(path),
            mode: 0o755,
            link: String::new(),
        };
        write_entry(out, &entry, verbose)?;

        let mut children: Vec<String> = Vec::new();
        for child in fs::read_dir(path)? {
            children.push(child?.file_name().to_string_lossy().into_owned());
        }
        children.sort();
        for child in children {
            archive_path(
                out,
                &format!("{}/{}", path.trim_end_matches('/'), child),
                &format!("{}/{}", name, child),
                verbose,
            )?;
        }
        return Ok(());
    }

    let entry = Entry {
        name: name.to_string(),
        kind: Kind::File,
        size: meta.len(),
        mtime: mtime_of(path),
        mode: 0o644,
        link: String::new(),
    };
    write_entry(out, &entry, verbose)?;
    copy_into_archive(out, path, meta.len())
}

fn write_entry(out: &mut dyn Write, entry: &Entry, verbose: bool) -> Result<(), io::Error> {
    let block = header::encode(entry)
        .ok_or_else(|| io::Error::other(format!("{}: name too long for ustar", entry.name)))?;
    out.write_all(&block)?;
    if verbose {
        eprintln!("{}", entry.name);
    }
    Ok(())
}

/// Stream the file's contents, padded to the next block boundary. The header
/// already promised `size` bytes, so a file that changed underneath us is
/// truncated or zero-filled rather than desynchronizing the archive.
fn copy_into_archive(out: &mut dyn Write, path: &str, size: u64) -> Result<(), io::Error> {
    let mut file = File::open(path)?;
    let mut buf = [0u8; 8192];
    let mut written = 0u64;
    while written < size {
        let want = ((size - written) as usize).min(buf.len());
        let n = file.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        written += n as u64;
    }
    while written < size {
        let n = ((size - written) as usize).min(buf.len());
        buf[..n].fill(0);
        out.write_all(&buf[..n])?;
        written += n as u64;
    }
    write_padding(out, size)
}

fn write_padding(out: &mut dyn Write, size: u64) -> Result<(), io::Error> {
    let pad = (BLOCK - (size as usize % BLOCK)) % BLOCK;
    if pad > 0 {
        out.write_all(&[0u8; BLOCK][..pad])?;
    }
    Ok(())
}

fn mtime_of(path: &str) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// List or extract, driven by the same header walk so the two can never
/// disagree about where the next header is.
fn read_archive(opts: &Options, extract: bool) -> Result<(), String> {
    let stdin = io::stdin();
    let mut input: Box<dyn Read> = if is_stdio(&opts.archive) {
        Box::new(stdin.lock())
    } else {
        let path = opts.archive.as_deref().unwrap();
        Box::new(File::open(path).map_err(|e| format!("{}: {}", path, e))?)
    };

    let wanted: Vec<&str> = opts.paths.iter().map(|p| p.as_str()).collect();
    let mut failed = false;

    loop {
        let mut block = [0u8; BLOCK];
        match read_block(&mut input, &mut block) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => return Err(format!("read: {}", e)),
        }

        let entry = match header::decode(&block) {
            Ok(Decoded::End) => break,
            Ok(Decoded::Entry(e)) => e,
            Err(e) => return Err(e),
        };

        let selected = wanted.is_empty() || wanted.iter().any(|w| selects(w, &entry.name));
        if !selected {
            skip(&mut input, entry.size).map_err(|e| format!("read: {}", e))?;
            continue;
        }

        if !extract {
            if opts.verbose {
                println!(
                    "{} {:>10} {}",
                    type_char(entry.kind),
                    entry.size,
                    entry.name
                );
            } else {
                println!("{}", entry.name);
            }
            skip(&mut input, entry.size).map_err(|e| format!("read: {}", e))?;
            continue;
        }

        match extract_entry(&mut input, &entry) {
            Ok(()) => {
                if opts.verbose {
                    eprintln!("{}", entry.name);
                }
            }
            Err(e) => {
                eprintln!("tar: {}: {}", entry.name, e);
                failed = true;
                skip(&mut input, entry.size).map_err(|e| format!("read: {}", e))?;
            }
        }
    }

    if failed {
        Err("extraction is incomplete".to_string())
    } else {
        Ok(())
    }
}

/// A named operand selects the entry itself and everything under it.
fn selects(wanted: &str, name: &str) -> bool {
    let wanted = wanted.trim_start_matches('/').trim_end_matches('/');
    let stripped = name.trim_end_matches('/');
    stripped == wanted || stripped.starts_with(&format!("{}/", wanted))
}

fn type_char(kind: Kind) -> char {
    match kind {
        Kind::File => '-',
        Kind::Dir => 'd',
        Kind::Symlink => 'l',
    }
}

/// Fill `block` from the archive. `Ok(false)` means a clean end of input;
/// a short block is a truncated archive.
fn read_block(input: &mut dyn Read, block: &mut [u8; BLOCK]) -> Result<bool, io::Error> {
    let mut filled = 0;
    while filled < BLOCK {
        let n = input.read(&mut block[filled..])?;
        if n == 0 {
            if filled == 0 {
                return Ok(false);
            }
            return Err(io::Error::other("truncated archive"));
        }
        filled += n;
    }
    Ok(true)
}

/// Step over an entry's data, rounded up to the block it ends in.
fn skip(input: &mut dyn Read, size: u64) -> Result<(), io::Error> {
    let mut left = size.div_ceil(BLOCK as u64) * BLOCK as u64;
    let mut buf = [0u8; 8192];
    while left > 0 {
        let want = (left as usize).min(buf.len());
        let n = input.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::other("truncated archive"));
        }
        left -= n as u64;
    }
    Ok(())
}

fn extract_entry(input: &mut dyn Read, entry: &Entry) -> Result<(), io::Error> {
    let name = safe_name(&entry.name)?;
    let path = Path::new(&name);

    match entry.kind {
        Kind::Dir => {
            fs::create_dir_all(path)?;
        }
        Kind::Symlink => {
            make_parents(path)?;
            let _ = fs::remove_file(path);
            let rc = edos_lib::io::symlink(&entry.link, &name);
            if rc < 0 {
                return Err(io::Error::from_raw_os_error(-rc as i32));
            }
        }
        Kind::File => {
            make_parents(path)?;
            let mut file = File::create(path)?;
            let mut left = entry.size;
            let mut buf = [0u8; 8192];
            while left > 0 {
                let want = (left as usize).min(buf.len());
                let n = input.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(io::Error::other("truncated archive"));
                }
                file.write_all(&buf[..n])?;
                left -= n as u64;
            }
            // The data is followed by padding to the block boundary, which
            // belongs to this entry and not to the next header.
            skip_padding(input, entry.size)?;
            let times = fs::FileTimes::new()
                .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(entry.mtime));
            let _ = file.set_times(times);
        }
    }
    Ok(())
}

fn skip_padding(input: &mut dyn Read, size: u64) -> Result<(), io::Error> {
    let pad = (BLOCK - (size as usize % BLOCK)) % BLOCK;
    if pad > 0 {
        let mut buf = [0u8; BLOCK];
        input.read_exact(&mut buf[..pad])?;
    }
    Ok(())
}

fn make_parents(path: &Path) -> Result<(), io::Error> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// Refuse a member that would escape the extraction directory. A leading
/// slash is stripped, as GNU tar does; a `..` component is fatal for that
/// member.
fn safe_name(name: &str) -> Result<String, io::Error> {
    let trimmed = name.trim_start_matches('/').trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(io::Error::other("empty member name"));
    }
    if trimmed.split('/').any(|c| c == "..") {
        return Err(io::Error::other("member escapes the archive directory"));
    }
    Ok(trimmed.to_string())
}
