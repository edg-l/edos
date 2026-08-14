//! A three-way merge, which is what carries a corrected default onto a machine
//! whose copy of it was edited.
//!
//! Two stages, tried in order, and a file that survives neither is left to the
//! machine. There is nobody to ask: this runs on a worker thread with no
//! terminal attached, so the only two outcomes are a result nobody has to check
//! and a refusal that says so.
//!
//! **A line merge**, the diff3 one: line up each side against the base through
//! a longest common subsequence, take the lines all three agree on as anchors,
//! and resolve each region between them from whichever side changed it. This is
//! what handles a settings file with several lines in it — a keyword added to
//! the default while the machine changed a different keyword — as long as at
//! least one line neither side touched separates the two edits. Two edits with
//! nothing unchanged between them are one region, and diff3 will not split it.
//!
//! **A documentation graft**, because the line merge alone would close almost
//! nothing here. A setting is one value with its comment above it
//! (`edos_lib::config`), so the case this whole thing exists for — the package
//! rewords the comment, the machine changed the value — is two edits on
//! adjacent lines with nothing unchanged between them, and diff3 calls that a
//! conflict. GNU diff3 and git both do; it is not a shortcoming of this
//! implementation. So when one side changed no significant line at all, its
//! edit was documentation and cannot mean anything else: the other side's
//! values are grafted into it and the result keeps both. When both sides
//! changed a value, that is a real disagreement and the answer is the machine's
//! copy, untouched.
//!
//! A significant line is one that is neither blank nor a `#` comment, which is
//! what both `/etc` formats read: the one-value files `edos_lib::config` owns,
//! and the `keyword value` files `edos-init` reads out of `/etc/services`.

/// The largest file this will merge, in lines.
///
/// The line merge is quadratic, so this is what keeps a package from handing
/// the installer a pathological file to chew on. A setting that is one value
/// with its comment is nowhere near the bound; something that reaches it has
/// stopped being a setting, and declining costs only the automatic upgrade.
const MAX_LINES: usize = 1024;

pub enum Merged {
    /// Every difference was attributable to one side. The bytes are the result.
    Clean(Vec<u8>),
    /// Both sides changed the same setting, differently. Nothing is produced:
    /// the caller keeps what the machine has.
    Conflict,
}

/// Merge the machine's file (`ours`) and the new default (`theirs`) over the
/// default they both came from (`base`).
pub fn merge(base: &[u8], ours: &[u8], theirs: &[u8]) -> Merged {
    let base = lines(base);
    let ours = lines(ours);
    let theirs = lines(theirs);

    if base.len().max(ours.len()).max(theirs.len()) > MAX_LINES {
        return Merged::Conflict;
    }

    if let Some(merged) = line_merge(&base, &ours, &theirs) {
        return Merged::Clean(merged);
    }

    // Whichever side left every significant line of the base alone changed
    // documentation and nothing else, so the other side's values go into it.
    // Both tests can hold at once only if neither side touched a value, and
    // then the line merge above would have resolved it.
    let (scaffold, values) = if significant(&theirs) == significant(&base) {
        (&theirs, significant(&ours))
    } else if significant(&ours) == significant(&base) {
        (&ours, significant(&theirs))
    } else {
        return Merged::Conflict;
    };

    Merged::Clean(graft(scaffold, &values))
}

/// Split into lines that carry their own newline, so concatenating them
/// reproduces the input and a file with no final newline stays that way.
fn lines(data: &[u8]) -> Vec<&[u8]> {
    data.split_inclusive(|&byte| byte == b'\n').collect()
}

/// The lines that carry a setting: neither blank nor a comment.
fn significant<'a>(file: &[&'a [u8]]) -> Vec<&'a [u8]> {
    file.iter()
        .copied()
        .filter(|line| {
            let trimmed = line.trim_ascii_start();
            !trimmed.is_empty() && !trimmed.starts_with(b"#")
        })
        .collect()
}

/// diff3: `None` where a region was changed on both sides and differently.
fn line_merge(base: &[&[u8]], ours: &[&[u8]], theirs: &[&[u8]]) -> Option<Vec<u8>> {
    let to_ours = pairings(base, ours);
    let to_theirs = pairings(base, theirs);

    let mut out: Vec<u8> = Vec::new();
    let (mut b, mut o, mut t) = (0usize, 0usize, 0usize);

    while b < base.len() || o < ours.len() || t < theirs.len() {
        // A base line both sides kept, in the position both sides kept it in:
        // all three agree, so it passes through and anchors what came before.
        if b < base.len() && to_ours[b] == Some(o) && to_theirs[b] == Some(t) {
            out.extend_from_slice(base[b]);
            b += 1;
            o += 1;
            t += 1;
            continue;
        }

        // Everything up to the next such anchor is one region, and its three
        // slices are what the base said, what the machine says, and what the
        // package now says.
        let next = (b..base.len()).find(|&i| to_ours[i].is_some() && to_theirs[i].is_some());
        let (end_b, end_o, end_t) = match next {
            Some(i) => (i, to_ours[i]?, to_theirs[i]?),
            None => (base.len(), ours.len(), theirs.len()),
        };

        let (was, mine, yours) = (&base[b..end_b], &ours[o..end_o], &theirs[t..end_t]);
        let resolved = if mine == was {
            yours
        } else if yours == was || yours == mine {
            mine
        } else {
            return None;
        };
        for line in resolved {
            out.extend_from_slice(line);
        }

        b = end_b;
        o = end_o;
        t = end_t;
    }

    Some(out)
}

/// Rebuild `scaffold` with its significant lines replaced by `values`, keeping
/// every comment and blank line where it stands.
///
/// A value list longer than the scaffold has slots for is appended after the
/// last one it filled, which keeps a keyword the machine added; a shorter one
/// leaves the trailing slots empty, which is how a keyword it deleted stays
/// deleted.
fn graft(scaffold: &[&[u8]], values: &[&[u8]]) -> Vec<u8> {
    let mut out: Vec<&[u8]> = Vec::new();
    let mut next = 0;
    let mut last_slot = None;

    for line in scaffold {
        let trimmed = line.trim_ascii_start();
        if trimmed.is_empty() || trimmed.starts_with(b"#") {
            out.push(line);
            continue;
        }
        if let Some(value) = values.get(next) {
            out.push(value);
            next += 1;
            last_slot = Some(out.len());
        }
    }

    let tail = last_slot.unwrap_or(out.len());
    for (i, value) in values[next..].iter().enumerate() {
        out.insert(tail + i, value);
    }

    join(&out)
}

/// Concatenate, giving every line but the last the newline it needs: a line
/// grafted from elsewhere may have been that file's last one.
fn join(lines: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        out.extend_from_slice(line);
        if i + 1 < lines.len() && !line.ends_with(b"\n") {
            out.push(b'\n');
        }
    }
    out
}

/// For each line of `a`, the line of `b` it is paired with in a longest common
/// subsequence of the two, or `None` where it is in neither.
fn pairings(a: &[&[u8]], b: &[&[u8]]) -> Vec<Option<usize>> {
    let (n, m) = (a.len(), b.len());
    let at = |i: usize, j: usize| i * (m + 1) + j;
    let mut len = vec![0u32; (n + 1) * (m + 1)];

    for i in (0..n).rev() {
        for j in (0..m).rev() {
            len[at(i, j)] = if a[i] == b[j] {
                len[at(i + 1, j + 1)] + 1
            } else {
                len[at(i + 1, j)].max(len[at(i, j + 1)])
            };
        }
    }

    let mut paired = vec![None; n];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            paired[i] = Some(j);
            i += 1;
            j += 1;
        } else if len[at(i + 1, j)] >= len[at(i, j + 1)] {
            i += 1;
        } else {
            j += 1;
        }
    }
    paired
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(base: &str, ours: &str, theirs: &str) -> String {
        match merge(base.as_bytes(), ours.as_bytes(), theirs.as_bytes()) {
            Merged::Clean(bytes) => String::from_utf8(bytes).expect("merged bytes are not utf-8"),
            Merged::Conflict => panic!("expected a clean merge"),
        }
    }

    fn conflicts(base: &str, ours: &str, theirs: &str) -> bool {
        matches!(
            merge(base.as_bytes(), ours.as_bytes(), theirs.as_bytes()),
            Merged::Conflict
        )
    }

    #[test]
    fn takes_the_new_default_where_nothing_local_changed() {
        assert_eq!(
            clean("# old\nus\n", "# old\nus\n", "# new\nus\n"),
            "# new\nus\n"
        );
    }

    #[test]
    fn keeps_the_local_value_where_the_default_did_not_move() {
        assert_eq!(clean("# c\nus\n", "# c\nde\n", "# c\nus\n"), "# c\nde\n");
    }

    /// The case the whole thing exists for, and the one the line merge alone
    /// calls a conflict: a reworded comment reaching a machine that had changed
    /// the value under it.
    #[test]
    fn a_reworded_comment_reaches_an_edited_value() {
        assert_eq!(
            clean(
                "# the keyboard layout\nus\n",
                "# the keyboard layout\nde\n",
                "# the keyboard layout, as a two-letter code\nus\n",
            ),
            "# the keyboard layout, as a two-letter code\nde\n",
        );
    }

    #[test]
    fn a_local_comment_survives_a_changed_default_value() {
        assert_eq!(
            clean("# c\nus\n", "# mine\nus\n", "# c\nde\n"),
            "# mine\nde\n"
        );
    }

    #[test]
    fn identical_edits_on_both_sides_are_not_a_conflict() {
        assert_eq!(clean("us\n", "de\n", "de\n"), "de\n");
    }

    #[test]
    fn the_same_value_changed_differently_conflicts() {
        assert!(conflicts("us\n", "de\n", "fr\n"));
        assert!(conflicts("# c\nus\n", "# c\nde\n", "# other\nfr\n"));
    }

    #[test]
    fn separated_edits_in_a_keyword_file_both_land() {
        let base = "command /bin/httpd\nargs -p 80\nrequires net\nessential no\n";
        let ours = "command /bin/httpd\nargs -p 8080\nrequires net\nessential no\n";
        let theirs = "command /bin/httpd\nargs -p 80\nrequires net\nessential yes\n";
        assert_eq!(
            clean(base, ours, theirs),
            "command /bin/httpd\nargs -p 8080\nrequires net\nessential yes\n"
        );
    }

    /// Two keywords changed with nothing unchanged between them are one region
    /// to diff3, and it will not split it. GNU diff3 and git both answer the
    /// same way; the graft cannot help either, because both sides changed a
    /// value. The machine keeps its file.
    #[test]
    fn adjacent_keyword_edits_conflict() {
        let base = "command /bin/httpd\nargs -p 80\nessential no\n";
        let ours = "command /bin/httpd\nargs -p 8080\nessential no\n";
        let theirs = "command /bin/httpd\nargs -p 80\nessential yes\n";
        assert!(conflicts(base, ours, theirs));
    }

    #[test]
    fn a_keyword_the_machine_added_survives_a_reworded_comment() {
        let base = "# a service\ncommand /bin/httpd\n";
        let ours = "# a service\ncommand /bin/httpd\nargs -p 8080\n";
        let theirs = "# a service, supervised by init\ncommand /bin/httpd\n";
        assert_eq!(
            clean(base, ours, theirs),
            "# a service, supervised by init\ncommand /bin/httpd\nargs -p 8080\n"
        );
    }

    #[test]
    fn a_keyword_the_machine_deleted_stays_deleted() {
        let base = "# a service\ncommand /bin/httpd\nargs -p 80\n";
        let ours = "# a service\ncommand /bin/httpd\n";
        let theirs = "# a service, supervised by init\ncommand /bin/httpd\nargs -p 80\n";
        assert_eq!(
            clean(base, ours, theirs),
            "# a service, supervised by init\ncommand /bin/httpd\n"
        );
    }

    #[test]
    fn an_insertion_on_each_side_is_kept() {
        assert_eq!(
            clean("a\nb\n", "a\nb\nours\n", "theirs\na\nb\n"),
            "theirs\na\nb\nours\n",
        );
    }

    #[test]
    fn a_deletion_on_one_side_is_kept() {
        assert_eq!(clean("a\nb\nc\n", "a\nc\n", "a\nb\nc\n"), "a\nc\n");
    }

    #[test]
    fn a_missing_final_newline_survives() {
        assert_eq!(clean("# c\nus", "# c\nus", "# c\nde"), "# c\nde");
    }

    /// A value grafted out of a file that ended without one must not run into
    /// the line below it.
    #[test]
    fn a_grafted_value_gains_the_newline_it_needs() {
        assert_eq!(
            clean(
                "# c\nus\n# trailing\n",
                "# c\nde",
                "# new\nus\n# trailing\n"
            ),
            "# new\nde\n# trailing\n"
        );
    }

    #[test]
    fn an_empty_base_takes_the_one_side_that_wrote_anything() {
        assert_eq!(clean("", "", "theirs\n"), "theirs\n");
        assert!(conflicts("", "ours\n", "theirs\n"));
    }

    #[test]
    fn a_file_past_the_line_bound_is_declined() {
        let big = "x\n".repeat(MAX_LINES + 1);
        assert!(conflicts(&big, &big, &big));
    }
}
