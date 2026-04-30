use std::env;
use std::io::{self, Write};
use std::path::Path;

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut paths: Vec<&str> = Vec::new();
    let mut name_filter: Option<String> = None;
    let mut type_filter: Option<char> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-name" if i + 1 < args.len() => {
                i += 1;
                name_filter = Some(args[i].clone());
            }
            "-type" if i + 1 < args.len() => {
                i += 1;
                type_filter = args[i].chars().next();
            }
            _ => paths.push(&args[i]),
        }
        i += 1;
    }

    if paths.is_empty() {
        paths.push(".");
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();

    for path in paths {
        walk(Path::new(path), &name_filter, &type_filter, &mut out);
    }
}

fn walk(
    dir: &Path,
    name_filter: &Option<String>,
    type_filter: &Option<char>,
    out: &mut impl Write,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

        // Print the entry if it passes filters
        if passes_filter(&path, is_dir, name_filter, type_filter) {
            let _ = writeln!(out, "{}", path.display());
        }

        if is_dir {
            walk(&path, name_filter, type_filter, out);
        }
    }
}

fn passes_filter(
    path: &Path,
    is_dir: bool,
    name_filter: &Option<String>,
    type_filter: &Option<char>,
) -> bool {
    if let Some(pat) = name_filter {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !glob_match(pat, name) {
            return false;
        }
    }
    if let Some(t) = type_filter {
        match *t {
            'f' if is_dir => return false,
            'd' if !is_dir => return false,
            _ => {}
        }
    }
    true
}

fn glob_match(pattern: &str, name: &str) -> bool {
    let pat = pattern.as_bytes();
    let s = name.as_bytes();
    let (pn, sn) = (pat.len(), s.len());

    // dp[i][j] = does pat[..i] match s[..j]?
    let mut dp = vec![false; sn + 1];
    dp[0] = true;
    for j in 1..=sn {
        dp[j] = false;
    }

    for i in 1..=pn {
        let pc = pat[i - 1];
        let mut prev = dp[0];
        dp[0] = dp[0] && pc == b'*';
        for j in 1..=sn {
            let old = dp[j];
            if pc == b'*' {
                dp[j] = dp[j] || dp[j - 1];
            } else if pc == b'?' || pc == s[j - 1] {
                dp[j] = prev;
            } else {
                dp[j] = false;
            }
            prev = old;
        }
    }

    dp[sn]
}
