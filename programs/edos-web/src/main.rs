//! A web browser for EDOS. Stage 1: fetch a URL, parse the HTML, and show the
//! document in a window -- or print it as text with `-d`, which is the only
//! rendering a headless run can assert on.
//!
//! See `doc/design/browser.md` for the stages and what each one is for.

use std::{env, fs, io::Write as _, process};

use edos_http::{Options, url::Url};

mod doc;
mod text;
mod ui;
mod view;

const DEFAULT_WIDTH: usize = 80;

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut width = DEFAULT_WIDTH;
    let mut links = false;
    let mut dump = false;
    let mut target: Option<String> = None;
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                usage();
                process::exit(0);
            }
            "-d" | "--dump" => dump = true,
            "-l" | "--links" => links = true,
            "-w" | "--width" => {
                i += 1;
                match args.get(i).and_then(|w| w.parse().ok()) {
                    Some(w) => width = w,
                    None => fail("-w wants a column count"),
                }
            }
            arg if arg.starts_with('-') && arg.len() > 1 => {
                fail(&format!("unknown option: {}", arg));
            }
            arg => target = Some(arg.to_string()),
        }
        i += 1;
    }

    let Some(target) = target else {
        usage();
        process::exit(1);
    };

    let (html, base) = match load(&target) {
        Ok(loaded) => loaded,
        Err(message) => fail(&message),
    };

    let address = base.to_string();
    let document = doc::parse(&html, base);

    if dump {
        let rendered = text::render(&document, width, links);
        // Writing the whole rendering in one go: a redirect to /dev/klog turns
        // every write into a log line, and a per-line write would interleave
        // with whatever else the kernel is logging.
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        if out.write_all(rendered.as_bytes()).is_err() {
            process::exit(1);
        }
        return;
    }

    // Said on stdout as well as drawn, so a headless run can tell a page that
    // arrived empty from one that never loaded.
    println!(
        "edos-web: {} - {} blocks from {}",
        document.display_title(),
        document.blocks.len(),
        address
    );

    match ui::Browser::open(document, address) {
        Ok(mut browser) => browser.run(),
        Err(err) => fail(&format!("cannot create a window: {err}")),
    }
}

/// Fetch `target`, or read it from disk when it names a local file.
///
/// A path is worth accepting because it separates a parse or layout problem
/// from a network one, which is the first question when a page renders wrong.
pub(crate) fn load(target: &str) -> Result<(Vec<u8>, Url), String> {
    if target.starts_with('/') || target.starts_with("./") {
        let html = fs::read(target).map_err(|e| format!("{}: {}", target, e))?;
        // A local file has no origin, so relative links resolve against the
        // site the page most likely came from only if it says so itself; until
        // stage 2 reads <base>, they resolve against a placeholder.
        let base = Url::parse("http://localhost/").expect("a literal URL parses");
        return Ok((html, base));
    }

    let url = if target.contains("://") {
        target.to_string()
    } else {
        format!("https://{}", target)
    };

    let response = edos_http::get(&url, &Options::default()).map_err(|e| e.to_string())?;
    if !response.head.is_success() {
        return Err(format!(
            "{}: {} {}",
            url, response.head.status, response.head.reason
        ));
    }
    // Redirects mean the document's own URL is where it ended up, not where it
    // was asked for, and every relative link on the page resolves against it.
    let base = Url::parse(&response.head.final_url).map_err(|e| e.to_string())?;
    Ok((response.body, base))
}

fn usage() {
    eprintln!("usage: edos-web [-d [-l] [-w COLUMNS]] URL|FILE");
    eprintln!("  -d, --dump      print the page as text instead of opening a window");
    eprintln!("  -l, --links     number links and list their targets");
    eprintln!(
        "  -w, --width N   wrap to N columns (default {})",
        DEFAULT_WIDTH
    );
    eprintln!("in the window: click a link to follow it, Backspace or Back goes back");
    eprintln!("  arrows, PageUp/PageDown, Home/End scroll; q closes");
}

fn fail(message: &str) -> ! {
    eprintln!("edos-web: {}", message);
    process::exit(1)
}
