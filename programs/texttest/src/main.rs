//! Regression suite for the text coreutils: uniq, sort, cut, tr, wc, head,
//! tail, sed and grep.
//!
//! 138 binaries ship in `/bin` and, before this, no gate ran a single one of
//! them. Two real bugs -- `uniq` spinning forever on a persistent read error
//! and `top`'s interval wait -- lived in exactly that blind spot, because a
//! tool that compiles is indistinguishable from a tool that works until
//! something executes it.
//!
//! Each case feeds a fixture through one tool and compares its whole stdout
//! against a string held here. Redirection is by descriptor rather than by
//! pipe: a pipe deadlocks when a tool writes more than the pipe buffer before
//! anybody reads, and the point of the suite is the tool's output, not the
//! plumbing carrying it.
//!
//! Reports through its exit code, so `make guest-check` can judge it. The
//! first mismatch names the tool, the argument vector, and both strings.

use std::fs;
use std::process::exit;

use edos_lib::{
    io::{self, O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY},
    process,
};

const DIR: &str = "/tmp/texttest";
/// Six lines, deliberately unsorted and with a repeat, so `sort` and `uniq`
/// each have something to do to it.
const WORDS: &str = "/tmp/texttest/words.txt";
/// The same six lines already sorted, which is what `uniq` is defined against.
const SORTED: &str = "/tmp/texttest/sorted.txt";
/// Colon-separated records for `cut`.
const FIELDS: &str = "/tmp/texttest/fields.txt";
/// Where a tool's stdout is collected. Rewritten per case.
const OUT: &str = "/tmp/texttest/out.txt";

const WORDS_TEXT: &str = "banana\napple\napple\ncherry\nbanana\napple\n";
const SORTED_TEXT: &str = "apple\napple\napple\nbanana\nbanana\ncherry\n";
const FIELDS_TEXT: &str = "one:two:three\nalpha:beta:gamma\n";

/// One tool, one argument vector, one fixture on stdin, one expected stdout.
struct Case {
    tool: &'static str,
    args: &'static [&'static str],
    stdin: &'static str,
    want: &'static str,
}

const CASES: &[Case] = &[
    // `wc` pads each count to eight columns and prints no name when the input
    // came in on stdin, so the whole line is the number.
    Case {
        tool: "wc",
        args: &["-l"],
        stdin: WORDS,
        want: "       6\n",
    },
    Case {
        tool: "wc",
        args: &["-c"],
        stdin: WORDS,
        want: "      39\n",
    },
    Case {
        tool: "head",
        args: &["-n", "2"],
        stdin: WORDS,
        want: "banana\napple\n",
    },
    Case {
        tool: "head",
        args: &["-c", "7"],
        stdin: WORDS,
        want: "banana\n",
    },
    Case {
        tool: "tail",
        args: &["-n", "3"],
        stdin: WORDS,
        want: "cherry\nbanana\napple\n",
    },
    Case {
        tool: "sort",
        args: &[],
        stdin: WORDS,
        want: SORTED_TEXT,
    },
    Case {
        tool: "sort",
        args: &["-u"],
        stdin: WORDS,
        want: "apple\nbanana\ncherry\n",
    },
    Case {
        tool: "sort",
        args: &["-r"],
        stdin: WORDS,
        want: "cherry\nbanana\nbanana\napple\napple\napple\n",
    },
    // `uniq` collapses adjacent runs only, which is why it is fed the sorted
    // fixture. `-c` pads the count to seven columns and separates with a space.
    Case {
        tool: "uniq",
        args: &[],
        stdin: SORTED,
        want: "apple\nbanana\ncherry\n",
    },
    Case {
        tool: "uniq",
        args: &["-c"],
        stdin: SORTED,
        want: "      3 apple\n      2 banana\n      1 cherry\n",
    },
    Case {
        tool: "uniq",
        args: &["-d"],
        stdin: SORTED,
        want: "apple\nbanana\n",
    },
    Case {
        tool: "uniq",
        args: &["-u"],
        stdin: SORTED,
        want: "cherry\n",
    },
    Case {
        tool: "cut",
        args: &["-d:", "-f2"],
        stdin: FIELDS,
        want: "two\nbeta\n",
    },
    Case {
        tool: "cut",
        args: &["-d:", "-f1"],
        stdin: FIELDS,
        want: "one\nalpha\n",
    },
    Case {
        tool: "tr",
        args: &["a-z", "A-Z"],
        stdin: WORDS,
        want: "BANANA\nAPPLE\nAPPLE\nCHERRY\nBANANA\nAPPLE\n",
    },
    Case {
        tool: "tr",
        args: &["-d", "an"],
        stdin: WORDS,
        want: "b\npple\npple\ncherry\nb\npple\n",
    },
    Case {
        tool: "grep",
        args: &["an"],
        stdin: WORDS,
        want: "banana\nbanana\n",
    },
    Case {
        tool: "grep",
        args: &["-n", "an"],
        stdin: WORDS,
        want: "1:banana\n5:banana\n",
    },
    Case {
        tool: "grep",
        args: &["-v", "an"],
        stdin: WORDS,
        want: "apple\napple\ncherry\napple\n",
    },
    Case {
        tool: "grep",
        args: &["-c", "apple"],
        stdin: WORDS,
        want: "3\n",
    },
    // `s` without `g` rewrites the first match on each line and leaves the rest.
    Case {
        tool: "sed",
        args: &["s/an/AN/"],
        stdin: WORDS,
        want: "bANana\napple\napple\ncherry\nbANana\napple\n",
    },
    Case {
        tool: "sed",
        args: &["s/an/AN/g"],
        stdin: WORDS,
        want: "bANANa\napple\napple\ncherry\nbANANa\napple\n",
    },
    Case {
        tool: "sed",
        args: &["-n", "2,3p"],
        stdin: WORDS,
        want: "apple\napple\n",
    },
    Case {
        tool: "sed",
        args: &["/apple/d"],
        stdin: WORDS,
        want: "banana\ncherry\nbanana\n",
    },
];

/// One line, on stdout. `println!` writes it whole; `eprintln!` is flushed per
/// fragment, so the same message on stderr arrives at `/dev/klog` split across
/// four log lines and is far harder to read out of a gate's artifact.
fn fail(what: &str, detail: &str) -> ! {
    println!("FAIL {what}: {detail}");
    exit(1);
}

/// Write the fixtures. Anything already there is from an earlier run of this
/// suite in the same boot and is replaced, so a case never reads a leftover.
fn fixtures() {
    let _ = fs::create_dir_all(DIR);
    for (path, text) in [
        (WORDS, WORDS_TEXT),
        (SORTED, SORTED_TEXT),
        (FIELDS, FIELDS_TEXT),
    ] {
        if let Err(e) = fs::write(path, text) {
            fail("fixtures", &format!("writing {path}: {e}"));
        }
    }
}

/// Run one tool with `stdin` redirected from a fixture and stdout captured,
/// answering its exit code and everything it wrote.
fn capture(tool: &str, args: &[&str], stdin_path: &str) -> (i32, String) {
    let what = format!("{tool} {}", args.join(" "));

    let inp = match io::open(stdin_path, O_RDONLY) {
        Ok(fd) => fd,
        Err(e) => fail(&what, &format!("opening {stdin_path}: {e:?}")),
    };
    let outp = match io::open(OUT, O_WRONLY | O_CREAT | O_TRUNC) {
        Ok(fd) => fd,
        Err(e) => fail(&what, &format!("opening {OUT}: {e:?}")),
    };

    let path = format!("/bin/{tool}");
    let pid = match process::spawn(&path, args, inp, outp, 2) {
        Ok(pid) => pid,
        Err(e) => fail(&what, &format!("spawn of {path} failed: {e:?}")),
    };
    let code = process::waitpid(pid);

    let _ = io::close(inp);
    let _ = io::close(outp);

    match fs::read_to_string(OUT) {
        Ok(text) => (code, text),
        Err(e) => fail(&what, &format!("reading {OUT}: {e}")),
    }
}

/// Render a string so a mismatch is readable on one line: the interesting
/// difference is usually a newline or a run of padding spaces.
fn show(s: &str) -> String {
    s.replace('\n', "\\n").replace('\t', "\\t")
}

fn main() {
    fixtures();

    for case in CASES {
        let what = format!("{} {}", case.tool, case.args.join(" "));
        let (code, got) = capture(case.tool, case.args, case.stdin);
        if code != 0 {
            fail(&what, &format!("exited {code}"));
        }
        if got != case.want {
            fail(
                &what,
                &format!("want \"{}\", got \"{}\"", show(case.want), show(&got)),
            );
        }
        println!("PASS {what}");
    }

    println!("texttest: OK, {} cases passed", CASES.len());
    exit(0);
}
