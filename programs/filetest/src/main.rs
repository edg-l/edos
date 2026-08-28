//! Regression suite for the file and archive tools: tar, gzip, gunzip, ln
//! and tee.
//!
//! `texttest` covers the nine text coreutils. These five were the other half
//! of the same blind spot: nothing in the tree ran them, and all five have
//! just had their argument parsing replaced, which is exactly the change a
//! compile cannot judge.
//!
//! The assertions are round trips rather than stdout diffs, because that is
//! what these tools are for: an archive that lists and extracts to the bytes
//! that went in, a stream that survives compression and decompression, a link
//! that resolves. A round trip also fails loudly when an option is read as a
//! filename, which is the failure a rewritten parser actually produces.
//!
//! Reports through its exit code, so `make guest-check` can judge it.

use std::fs;
use std::process::exit;

use edos_lib::{
    io::{self, O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY},
    process,
};

const DIR: &str = "/tmp/filetest";
/// Stdin for the tools that ignore it. `spawn` wants a descriptor either way.
const NUL: &str = "/tmp/filetest/nul";
/// Where a tool's stdout is collected. Rewritten per case.
const OUT: &str = "/tmp/filetest/out";

const HELLO: &str = "the quick brown fox\njumps over the lazy dog\n";
/// Long enough that deflate has something to do and a truncated round trip
/// cannot pass by accident.
const BULK_LINE: &str = "pack my box with five dozen liquor jugs\n";

/// Every tool this suite covers. All of them parse through
/// `edos_lib::args`, so all of them answer `--help` and honour `--`.
const TOOLS: &[&str] = &["tar", "gzip", "gunzip", "ln", "tee"];

fn fail(what: &str, detail: &str) -> ! {
    println!("FAIL {what}: {detail}");
    exit(1);
}

/// Run one tool with `stdin` redirected from a file and stdout captured,
/// answering its exit code and the raw bytes it wrote. Raw, because `gzip -c`
/// writes a deflate stream to stdout and reading that as UTF-8 fails on the
/// tool's own correct output.
fn capture_bytes(tool: &str, args: &[&str], stdin_path: &str) -> (i32, Vec<u8>) {
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

    match fs::read(OUT) {
        Ok(bytes) => (code, bytes),
        Err(e) => fail(&what, &format!("reading {OUT}: {e}")),
    }
}

/// `capture_bytes` for the cases whose stdout is text. Lossy rather than
/// fallible: a mismatch should be reported as a diff, not as a read error.
fn capture(tool: &str, args: &[&str], stdin_path: &str) -> (i32, String) {
    let (code, bytes) = capture_bytes(tool, args, stdin_path);
    (code, String::from_utf8_lossy(&bytes).into_owned())
}

/// A case that must succeed. Answers what the tool wrote on stdout.
fn ok(tool: &str, args: &[&str], stdin_path: &str) -> String {
    let what = format!("{tool} {}", args.join(" "));
    let (code, got) = capture(tool, args, stdin_path);
    if code != 0 {
        fail(&what, &format!("exited {code}, stdout \"{}\"", show(&got)));
    }
    println!("PASS {what}");
    got
}

fn show(s: &str) -> String {
    s.replace('\n', "\\n").replace('\t', "\\t")
}

fn read(path: &str) -> String {
    match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => fail(path, &format!("reading it back: {e}")),
    }
}

fn want_file(what: &str, path: &str, want: &str) {
    let got = read(path);
    if got != want {
        fail(
            what,
            &format!(
                "{path}: want \"{}\", got \"{}\"",
                show(want),
                show(&got.chars().take(120).collect::<String>())
            ),
        );
    }
}

fn exists(path: &str) -> bool {
    fs::metadata(path).is_ok()
}

fn fixtures() -> String {
    let _ = fs::remove_dir_all(DIR);
    if let Err(e) = fs::create_dir_all(DIR) {
        fail("fixtures", &format!("creating {DIR}: {e}"));
    }
    for (path, text) in [(NUL, ""), (OUT, "")] {
        if let Err(e) = fs::write(path, text) {
            fail("fixtures", &format!("writing {path}: {e}"));
        }
    }
    BULK_LINE.repeat(200)
}

/// `tee` writes its input to every named file and to stdout, and `-a` appends
/// rather than truncating. Two files at once is the case that catches a parser
/// treating the second one as an option's value.
fn tee_cases() {
    let src = format!("{DIR}/tee-in");
    let a = format!("{DIR}/tee-a");
    let b = format!("{DIR}/tee-b");
    if let Err(e) = fs::write(&src, HELLO) {
        fail("tee", &format!("writing {src}: {e}"));
    }

    let got = ok("tee", &[&a, &b], &src);
    if got != HELLO {
        fail("tee", &format!("stdout was \"{}\"", show(&got)));
    }
    want_file("tee", &a, HELLO);
    want_file("tee", &b, HELLO);

    ok("tee", &["-a", &a], &src);
    want_file("tee -a", &a, &HELLO.repeat(2));

    // Without -a the file is truncated first, so the second write is the
    // whole content again rather than twice over.
    ok("tee", &[&a], &src);
    want_file("tee", &a, HELLO);
}

/// `ln -s` is the only kind of link this kernel has. One operand infers the
/// name from the target, which is the shape the parser has to leave intact.
fn ln_cases() {
    let target = format!("{DIR}/ln-target");
    let link = format!("{DIR}/ln-link");
    if let Err(e) = fs::write(&target, HELLO) {
        fail("ln", &format!("writing {target}: {e}"));
    }

    ok("ln", &["-s", &target, &link], NUL);
    want_file("ln -s", &link, HELLO);

    // -f replaces an existing link rather than refusing.
    ok("ln", &["-sf", &target, &link], NUL);
    want_file("ln -sf", &link, HELLO);

    // -v prints what it made, which proves the cluster reached both flags.
    let got = ok("ln", &["-sfv", &target, &link], NUL);
    if !got.contains("ln-link") {
        fail(
            "ln -sfv",
            &format!("said nothing about the link: \"{}\"", show(&got)),
        );
    }

    // A hard link has nothing to fall back to, so it must be refused rather
    // than silently made into a symlink.
    let hard = format!("{DIR}/ln-hard");
    let (code, _) = capture("ln", &[&target, &hard], NUL);
    if code == 0 {
        fail("ln without -s", "succeeded; hard links are not supported");
    }
    println!("PASS ln without -s is refused");
}

/// `gzip` replaces its input unless `-k`, `-c` writes to stdout and leaves
/// everything alone, and a digit sets the level. `-9k` is the cluster that a
/// parser using `Spec::numeric` for the level would have broken.
fn gzip_cases(bulk: &str) {
    let plain = format!("{DIR}/gz-plain");
    let gz = format!("{DIR}/gz-plain.gz");

    if let Err(e) = fs::write(&plain, bulk) {
        fail("gzip", &format!("writing {plain}: {e}"));
    }

    // -k keeps the input, so both files exist afterwards.
    ok("gzip", &["-k", &plain], NUL);
    if !exists(&plain) {
        fail("gzip -k", "removed the input");
    }
    if !exists(&gz) {
        fail("gzip -k", "wrote no .gz");
    }

    // The round trip is the assertion: gunzip -k must give back the bytes.
    if let Err(e) = fs::remove_file(&plain) {
        fail("gzip", &format!("removing {plain}: {e}"));
    }
    ok("gunzip", &["-k", &gz], NUL);
    want_file("gunzip -k", &plain, bulk);

    // -c writes the compressed stream to stdout and touches no file, so the
    // .gz from the earlier case is still the only one.
    if let Err(e) = fs::remove_file(&gz) {
        fail("gzip", &format!("removing {gz}: {e}"));
    }
    let (code, stream) = capture_bytes("gzip", &["-c", &plain], NUL);
    if code != 0 {
        fail("gzip -c", &format!("exited {code}"));
    }
    // RFC 1952 section 2.3.1: every member begins ID1 = 0x1f, ID2 = 0x8b.
    if stream.first() != Some(&0x1f) || stream.get(1) != Some(&0x8b) {
        fail(
            "gzip -c",
            &format!(
                "stdout did not begin with the gzip magic: {:02x?}",
                &stream[..stream.len().min(4)]
            ),
        );
    }
    if exists(&gz) {
        fail("gzip -c", "wrote a .gz as well as stdout");
    }
    if !exists(&plain) {
        fail("gzip -c", "removed the input");
    }
    println!(
        "PASS gzip -c wrote {} gzip bytes to stdout and no file",
        stream.len()
    );

    // `-9k` is a cluster of a digit and a flag. A level read only from a bare
    // `-<digits>` argument would fail this with "unknown option -9".
    ok("gzip", &["-9k", &plain], NUL);
    if !exists(&plain) || !exists(&gz) {
        fail("gzip -9k", "did not behave as -9 and -k together");
    }
    let small = fs::metadata(&gz).map(|m| m.len()).unwrap_or(0);
    if small == 0 || small as usize >= bulk.len() {
        fail(
            "gzip -9k",
            &format!("compressed {} bytes to {}", bulk.len(), small),
        );
    }
    println!("PASS gzip -9k compressed {} bytes to {small}", bulk.len());

    // gunzip is gzip -d under another name, and -d on the command line has to
    // reach the same switch.
    if let Err(e) = fs::remove_file(&plain) {
        fail("gzip", &format!("removing {plain}: {e}"));
    }
    ok("gzip", &["-dk", &gz], NUL);
    want_file("gzip -dk", &plain, bulk);
}

/// tar's `-f` and `-C` take a separate argument, and `-xzf` is the cluster
/// where the value belongs to the last letter. Create, list and extract, then
/// compare the extracted bytes with what went in.
fn tar_cases(bulk: &str) {
    let src = format!("{DIR}/tar-src");
    let file = format!("{DIR}/tar-src/one.txt");
    let out = format!("{DIR}/tar-out");
    let archive = format!("{DIR}/one.tar");
    let targz = format!("{DIR}/one.tar.gz");

    for d in [&src, &out] {
        if let Err(e) = fs::create_dir_all(d) {
            fail("tar", &format!("creating {d}: {e}"));
        }
    }
    if let Err(e) = fs::write(&file, bulk) {
        fail("tar", &format!("writing {file}: {e}"));
    }

    // -C is what makes the archive hold `one.txt` rather than the whole path.
    ok("tar", &["-cf", &archive, "-C", &src, "one.txt"], NUL);
    let listing = ok("tar", &["-tf", &archive], NUL);
    if !listing.contains("one.txt") {
        fail("tar -tf", &format!("listed \"{}\"", show(&listing)));
    }
    ok("tar", &["-xf", &archive, "-C", &out], NUL);
    want_file("tar -xf", &format!("{out}/one.txt"), bulk);

    // The gzip path, and `-czf` / `-xzf` as clusters ending in the option
    // that takes the value.
    if let Err(e) = fs::remove_file(format!("{out}/one.txt")) {
        fail("tar", &format!("clearing the extract directory: {e}"));
    }
    ok("tar", &["-czf", &targz, "-C", &src, "one.txt"], NUL);
    let packed = fs::metadata(&targz).map(|m| m.len()).unwrap_or(0);
    if packed == 0 || packed as usize >= bulk.len() {
        fail(
            "tar -czf",
            &format!("packed {} bytes into {}", bulk.len(), packed),
        );
    }
    ok("tar", &["-xzf", &targz, "-C", &out], NUL);
    want_file("tar -xzf", &format!("{out}/one.txt"), bulk);

    // -v names each entry, which proves the cluster reached the flag as well
    // as the two options carrying values.
    let noisy = ok("tar", &["-tvf", &archive], NUL);
    if !noisy.contains("one.txt") {
        fail("tar -tvf", &format!("said \"{}\"", show(&noisy)));
    }

    // A mode is required, and one that names none must be refused rather than
    // doing whatever the last run did.
    let (code, _) = capture("tar", &["-f", &archive], NUL);
    if code == 0 {
        fail("tar without a mode", "succeeded");
    }
    println!("PASS tar without -c, -t or -x is refused");
}

fn main() {
    let bulk = fixtures();

    tee_cases();
    ln_cases();
    gzip_cases(&bulk);
    tar_cases(&bulk);

    // Every tool answers `--help` on stdout with exit 0 and names itself in
    // the first line. A tool that parses its own flags by hand answers a usage
    // error instead, which is how this catches one drifting back.
    for tool in TOOLS {
        let what = format!("{tool} --help");
        let (code, got) = capture(tool, &["--help"], NUL);
        if code != 0 {
            fail(&what, &format!("exited {code}"));
        }
        let want_prefix = format!("usage: {tool} ");
        if !got.starts_with(&want_prefix) {
            fail(
                &what,
                &format!(
                    "want a first line starting \"{}\", got \"{}\"",
                    want_prefix,
                    show(&got)
                ),
            );
        }
        println!("PASS {what}");
    }

    println!("filetest: OK");
    exit(0);
}
