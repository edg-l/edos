use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: diff <file1> <file2>");
        std::process::exit(1);
    }

    let (path1, path2) = (&args[1], &args[2]);

    let content1 = match std::fs::read(path1) {
        Ok(c) => String::from_utf8_lossy(&c).into_owned(),
        Err(e) => {
            eprintln!("diff: {}: {}", path1, e);
            std::process::exit(1);
        }
    };
    let content2 = match std::fs::read(path2) {
        Ok(c) => String::from_utf8_lossy(&c).into_owned(),
        Err(e) => {
            eprintln!("diff: {}: {}", path2, e);
            std::process::exit(1);
        }
    };

    let mut lines1: Vec<&str> = content1.lines().collect();
    let mut lines2: Vec<&str> = content2.lines().collect();

    // Strip a trailing empty element from lines() if the file ends with a newline
    if lines1.last() == Some(&"") && (content1.is_empty() || content1.ends_with('\n')) {
        lines1.pop();
    }
    if lines2.last() == Some(&"") && (content2.is_empty() || content2.ends_with('\n')) {
        lines2.pop();
    }

    let edits = compute_edit_script(&lines1, &lines2);
    if !has_changes(&edits) {
        return;
    }

    print_unified_diff(path1, path2, &edits, &lines1, &lines2);
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Edit {
    Keep,
    Remove,
    Add,
}

fn has_changes(edits: &[Edit]) -> bool {
    edits.iter().any(|e| *e != Edit::Keep)
}

fn compute_edit_script(lines1: &[&str], lines2: &[&str]) -> Vec<Edit> {
    let n = lines1.len();
    let m = lines2.len();

    // dp[i][j] = LCS length of lines1[..i], lines2[..j]
    // Use u16 to save memory; assumes <=65535 lines
    let mut dp = vec![vec![0u16; m + 1]; n + 1];
    for i in 1..=n {
        for j in 1..=m {
            if lines1[i - 1] == lines2[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }

    // Backtrack
    let mut edits = Vec::new();
    let mut i = n;
    let mut j = m;
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && lines1[i - 1] == lines2[j - 1] {
            edits.push(Edit::Keep);
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            edits.push(Edit::Add);
            j -= 1;
        } else if i > 0 {
            edits.push(Edit::Remove);
            i -= 1;
        } else {
            break;
        }
    }
    edits.reverse();
    edits
}

/// Print the edit script as unified diff with context hunks.
fn print_unified_diff(path1: &str, path2: &str, edits: &[Edit], lines1: &[&str], lines2: &[&str]) {
    println!("--- {}", path1);
    println!("+++ {}", path2);

    // Find ranges of consecutive non-Keep edits, expand by 3 context lines each side.
    // Then merge overlapping ranges.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < edits.len() {
        if edits[i] != Edit::Keep {
            let start = i;
            while i < edits.len() && edits[i] != Edit::Keep {
                i += 1;
            }
            ranges.push((start, i)); // [start, end)
        } else {
            i += 1;
        }
    }

    // Expand each range by context and merge
    let context = 3;
    let mut expanded: Vec<(usize, usize)> = Vec::new();
    for (s, e) in &ranges {
        let start = s.saturating_sub(context);
        let end = (e + context).min(edits.len());
        expanded.push((start, end));
    }

    // Merge overlapping
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in &expanded {
        if let Some(last) = merged.last_mut() {
            if *start <= last.1 {
                last.1 = *end;
                continue;
            }
        }
        merged.push((*start, *end));
    }

    // Print each hunk
    if merged.is_empty() {
        // Print the first hunk spanning everything if we have changes
        // (shouldn't happen if has_changes returned true, but be safe)
        merged.push((0, edits.len()));
    }

    for (hunk_start, hunk_end) in &merged {
        let hunk_edits = &edits[*hunk_start..*hunk_end];

        // Count old-file lines before this hunk (Remove + Keep)
        let old_base = edits[..*hunk_start]
            .iter()
            .filter(|e| **e != Edit::Add)
            .count();

        // Count new-file lines before this hunk (Add + Keep)
        let new_base = edits[..*hunk_start]
            .iter()
            .filter(|e| **e != Edit::Remove)
            .count();

        let mut old_len = 0usize;
        let mut new_len = 0usize;
        for e in hunk_edits {
            match e {
                Edit::Keep => {
                    old_len += 1;
                    new_len += 1;
                }
                Edit::Remove => old_len += 1,
                Edit::Add => new_len += 1,
            }
        }

        println!(
            "@@ -{},{} +{},{} @@",
            old_base + 1,
            old_len,
            new_base + 1,
            new_len,
        );

        let mut old_idx = old_base;
        let mut new_idx = new_base;
        for e in hunk_edits {
            match e {
                Edit::Keep => {
                    println!(" {}", lines1[old_idx]);
                    old_idx += 1;
                    new_idx += 1;
                }
                Edit::Remove => {
                    println!("-{}", lines1[old_idx]);
                    old_idx += 1;
                }
                Edit::Add => {
                    println!("+{}", lines2[new_idx]);
                    new_idx += 1;
                }
            }
        }
    }
}
