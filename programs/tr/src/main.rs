use edos_lib::args::{Opt, Spec};
use std::io::{self, Read, Write};

fn expand_set(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'n' => result.push(b'\n'),
                b't' => result.push(b'\t'),
                b'\\' => result.push(b'\\'),
                c => result.push(c),
            }
            i += 2;
        } else if i + 2 < bytes.len() && bytes[i + 1] == b'-' {
            let start = bytes[i];
            let end = bytes[i + 2];
            if start <= end {
                for c in start..=end {
                    result.push(c);
                }
            }
            i += 3;
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }
    result
}

const SPEC: Spec = Spec::new(
    "tr",
    "[-ds] SET1 [SET2]",
    &[
        Opt::flag('d', "delete", "delete every character in SET1"),
        Opt::flag(
            's',
            "squeeze-repeats",
            "collapse a run of one output character",
        ),
    ],
);

fn main() {
    let m = SPEC.parse_env();
    let delete = m.is_set('d');
    let squeeze = m.is_set('s');
    let sets = m.positional();
    if sets.is_empty() {
        SPEC.fail("no set given");
    }

    let set1 = expand_set(&sets[0]);
    let set2 = if !delete && sets.len() > 1 {
        expand_set(&sets[1])
    } else {
        Vec::new()
    };

    // Build translation table: byte -> Option<u8> (None = delete)
    let mut table: [Option<u8>; 256] = [None; 256];
    // Initialize identity
    for b in 0u8..=255 {
        table[b as usize] = Some(b);
    }

    if delete {
        for &b in &set1 {
            table[b as usize] = None;
        }
    } else {
        for (i, &b) in set1.iter().enumerate() {
            let mapped = if set2.is_empty() {
                b
            } else {
                *set2.get(i).unwrap_or_else(|| set2.last().unwrap())
            };
            table[b as usize] = Some(mapped);
        }
    }

    let mut input = Vec::new();
    if let Err(e) = io::stdin().read_to_end(&mut input) {
        eprintln!("tr: read: {}", e);
        std::process::exit(1);
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut last_out: Option<u8> = None;

    for &b in &input {
        match table[b as usize] {
            None => {}
            Some(mapped) => {
                if squeeze && set2.contains(&mapped) && last_out == Some(mapped) {
                    continue;
                }
                if out.write_all(&[mapped]).is_err() {
                    break;
                }
                last_out = Some(mapped);
            }
        }
    }
}
