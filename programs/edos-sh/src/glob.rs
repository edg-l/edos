//! Pathname expansion.
//!
//! A word containing `*`, `?` or `[...]` is matched against the filesystem one
//! path component at a time, and replaced by the sorted list of paths that
//! matched. A pattern that matches nothing is left alone, and a word that was
//! quoted or escaped anywhere is never a pattern.

/// Characters that make a word a pattern.
fn has_magic(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// Match a bracket expression starting at `pattern[start]` (a `[`) against `c`.
///
/// Returns whether it matched and the index just past the closing `]`, or
/// `None` when the expression is unterminated, which makes the `[` literal.
fn match_class(pattern: &[char], start: usize, c: char) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let negate = matches!(pattern.get(i), Some('!') | Some('^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    loop {
        let ch = *pattern.get(i)?;
        if ch == ']' && !first {
            break;
        }
        first = false;
        // `a-z`, unless the `-` is the last character before the `]`.
        if pattern.get(i + 1) == Some(&'-') && pattern.get(i + 2).is_some_and(|&e| e != ']') {
            if ch <= c && c <= pattern[i + 2] {
                matched = true;
            }
            i += 3;
        } else {
            if ch == c {
                matched = true;
            }
            i += 1;
        }
    }
    Some((matched != negate, i + 1))
}

/// Match one path component against one pattern component.
///
/// `*` backtracks to the last star rather than recursing, so a pattern with
/// several stars stays linear in the common case.
fn match_component(pattern: &[char], name: &[char]) -> bool {
    let (mut p, mut n) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;

    while n < name.len() {
        let advanced = match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
                true
            }
            Some('?') => {
                p += 1;
                n += 1;
                true
            }
            Some('[') => match match_class(pattern, p, name[n]) {
                Some((true, next)) => {
                    p = next;
                    n += 1;
                    true
                }
                Some((false, _)) => false,
                None => {
                    let lit = name[n] == '[';
                    if lit {
                        p += 1;
                        n += 1;
                    }
                    lit
                }
            },
            Some(&c) if c == name[n] => {
                p += 1;
                n += 1;
                true
            }
            _ => false,
        };
        if advanced {
            continue;
        }
        match star {
            Some((sp, sn)) => {
                star = Some((sp, sn + 1));
                n = sn + 1;
                p = sp + 1;
            }
            None => return false,
        }
    }

    pattern[p..].iter().all(|&c| c == '*')
}

/// The directory to read for a partially built path.
fn dir_of(base: &str, absolute: bool) -> &str {
    if base.is_empty() {
        if absolute { "/" } else { "." }
    } else {
        base
    }
}

/// Append one component to a partially built path.
fn join(base: &str, comp: &str, absolute: bool) -> String {
    if base.is_empty() {
        if absolute {
            format!("/{}", comp)
        } else {
            comp.to_string()
        }
    } else {
        format!("{}/{}", base, comp)
    }
}

/// Expand one word, returning the word unchanged when it is not a pattern or
/// when it matches nothing.
///
/// A leading `.` in a name is only matched by a pattern component that starts
/// with a literal `.`, so `*` does not pick up dotfiles.
pub fn expand_word(word: &str) -> Vec<String> {
    if !has_magic(word) {
        return vec![word.to_string()];
    }

    let absolute = word.starts_with('/');
    let mut paths = vec![String::new()];
    let mut seen_magic = false;
    let mut last_magic = false;

    for comp in word.split('/') {
        if comp.is_empty() {
            continue;
        }
        let mut next = Vec::new();
        last_magic = has_magic(comp);
        if last_magic {
            seen_magic = true;
            let pattern: Vec<char> = comp.chars().collect();
            let dotfiles = comp.starts_with('.');
            for base in &paths {
                let Ok(entries) = std::fs::read_dir(dir_of(base, absolute)) else {
                    continue;
                };
                let mut names: Vec<String> = entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|name| {
                        (dotfiles || !name.starts_with('.'))
                            && match_component(&pattern, &name.chars().collect::<Vec<_>>())
                    })
                    .collect();
                names.sort();
                next.extend(names.iter().map(|name| join(base, name, absolute)));
            }
        } else {
            next.extend(paths.iter().map(|base| join(base, comp, absolute)));
        }
        paths = next;
        if paths.is_empty() {
            break;
        }
    }

    // Components after the last pattern were appended without being read from a
    // directory, so `*/missing` has to be checked before it is returned.
    if seen_magic && !last_magic {
        paths.retain(|p| std::fs::metadata(p).is_ok());
    }

    if paths.is_empty() {
        return vec![word.to_string()];
    }
    if word.ends_with('/') {
        for p in &mut paths {
            p.push('/');
        }
    }
    paths
}

/// Expand a parsed argument list. The flag on each word is true when any part
/// of it was quoted or escaped, which makes the whole word literal.
pub fn expand_words(words: &[(String, bool)]) -> Vec<String> {
    let mut out = Vec::with_capacity(words.len());
    for (word, quoted) in words {
        if *quoted {
            out.push(word.clone());
        } else {
            out.extend(expand_word(word));
        }
    }
    out
}
