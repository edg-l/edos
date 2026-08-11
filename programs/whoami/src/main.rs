//! Print the name of the current user.
//!
//! There is no password database, so the name comes from the fixed table in
//! `edos_lib::process::id_name`. An id with no name prints as its number, which
//! is what `whoami` does on a system whose passwd entry has gone missing.

use edos_lib::process;

fn main() {
    let uid = process::getuid();
    match process::id_name(uid) {
        Some(name) => println!("{name}"),
        None => println!("{uid}"),
    }
}
