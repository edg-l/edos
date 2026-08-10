//! ln - create symbolic links

use std::env;
use std::fs;
use std::io::Error;
use std::process::ExitCode;

/// The final component of a path, with any trailing slashes ignored.
fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some((_, name)) => name,
        None => trimmed,
    }
}

/// Join a directory and a name the way the shell would, without collapsing
/// anything else in either half.
fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{}{}", dir, name)
    } else {
        format!("{}/{}", dir, name)
    }
}

/// Create one link at `link` holding `target` verbatim. Returns false on
/// failure, having already reported it.
fn link_one(target: &str, link: &str, force: bool, verbose: bool) -> bool {
    // A destination that already exists is an error unless -f was given, and
    // even then only a non-directory is removed: replacing a directory with a
    // link would silently discard its contents.
    if let Ok(meta) = fs::symlink_metadata(link) {
        if !force {
            eprintln!("ln: {}: file exists", link);
            return false;
        }
        if meta.is_dir() {
            eprintln!("ln: {}: is a directory", link);
            return false;
        }
        if let Err(e) = fs::remove_file(link) {
            eprintln!("ln: cannot remove {}: {}", link, e);
            return false;
        }
    }

    if edos_lib::io::symlink(target, link) < 0 {
        eprintln!("ln: {} -> {}: {}", link, target, Error::last_os_error());
        return false;
    }
    if verbose {
        println!("'{}' -> '{}'", link, target);
    }
    true
}

fn main() -> ExitCode {
    let mut symbolic = false;
    let mut force = false;
    let mut verbose = false;
    let mut operands: Vec<String> = Vec::new();
    let mut no_more_flags = false;

    for arg in env::args().skip(1) {
        if no_more_flags || !arg.starts_with('-') || arg == "-" {
            operands.push(arg);
            continue;
        }
        if arg == "--" {
            no_more_flags = true;
            continue;
        }
        for c in arg.chars().skip(1) {
            match c {
                's' => symbolic = true,
                'f' => force = true,
                'v' => verbose = true,
                _ => {
                    eprintln!("ln: unknown option '-{}'", c);
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    if operands.is_empty() {
        eprintln!("Usage: ln -s [-f] [-v] <target>... <link|directory>");
        return ExitCode::FAILURE;
    }

    // The kernel has no link(2) and EFS inodes carry no link count, so there
    // is nothing to fall back to when -s is absent.
    if !symbolic {
        eprintln!("ln: hard links are not supported; use -s for a symbolic link");
        return ExitCode::FAILURE;
    }

    // One operand links into the working directory under the target's own
    // name, which is what makes `ln -s /bin/ls` do the obvious thing.
    if operands.len() == 1 {
        let target = operands[0].clone();
        let name = basename(&target).to_string();
        if name.is_empty() {
            eprintln!("ln: {}: cannot infer a link name", target);
            return ExitCode::FAILURE;
        }
        return if link_one(&target, &name, force, verbose) {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }

    // Otherwise the last operand is the destination: a directory to fill, or,
    // with exactly one target, the link's own path.
    let dest = operands.pop().expect("checked non-empty above");
    let dest_is_dir = fs::metadata(&dest).map(|m| m.is_dir()).unwrap_or(false);

    if !dest_is_dir {
        if operands.len() > 1 {
            eprintln!("ln: {}: not a directory", dest);
            return ExitCode::FAILURE;
        }
        return if link_one(&operands[0], &dest, force, verbose) {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }

    let mut failed = false;
    for target in &operands {
        let name = basename(target);
        if name.is_empty() {
            eprintln!("ln: {}: cannot infer a link name", target);
            failed = true;
            continue;
        }
        if !link_one(target, &join(&dest, name), force, verbose) {
            failed = true;
        }
    }

    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
