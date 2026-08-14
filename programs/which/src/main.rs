//! Resolve a command name the way the shell would: the first executable of
//! that name along `PATH`.

use std::env;
use std::path::{Path, PathBuf};

/// Search `PATH` for `name`, returning the first entry that exists.
///
/// A name containing a separator is a path already and is only checked for
/// existence, which is also what the shell does with it.
fn resolve(name: &str, path: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let candidate = PathBuf::from(name);
        return candidate.is_file().then_some(candidate);
    }
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| candidate.is_file())
}

fn main() {
    let names: Vec<String> = env::args().skip(1).collect();
    if names.is_empty() {
        eprintln!("usage: which NAME...");
        std::process::exit(2);
    }

    let path = env::var("PATH").unwrap_or_else(|_| "/bin".to_string());
    let mut missing = false;
    for name in &names {
        match resolve(name, &path) {
            Some(found) => println!("{}", found.display()),
            None => missing = true,
        }
    }

    if missing {
        std::process::exit(1);
    }
}
