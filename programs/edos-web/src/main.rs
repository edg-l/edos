//! A web browser for EDOS. Stage 1: fetch a URL, parse the HTML, and print
//! the document as text.
//!
//! See `doc/design/browser.md` for the stages and what each one is for.

use std::{env, fs, io::Write as _, process};

use edos_http::{Options, url::Url};

mod doc;
mod text;

const DEFAULT_WIDTH: usize = 80;

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut width = DEFAULT_WIDTH;
    let mut links = false;
    let mut target: Option<String> = None;
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                usage();
                process::exit(0);
            }
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

    let document = doc::parse(&html, base);
    let rendered = text::render(&document, width, links);

    // Writing the whole rendering in one go: a redirect to /dev/klog turns
    // every write into a log line, and a per-line write would interleave with
    // whatever else the kernel is logging.
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if out.write_all(rendered.as_bytes()).is_err() {
        process::exit(1);
    }
}

/// Fetch `target`, or read it from disk when it names a local file.
///
/// A path is worth accepting because it separates a parse or layout problem
/// from a network one, which is the first question when a page renders wrong.
fn load(target: &str) -> Result<(Vec<u8>, Url), String> {
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
    eprintln!("usage: edos-web [-l] [-w COLUMNS] URL|FILE");
    eprintln!("  -l, --links     number links and list their targets");
    eprintln!(
        "  -w, --width N   wrap to N columns (default {})",
        DEFAULT_WIDTH
    );
}

fn fail(message: &str) -> ! {
    eprintln!("edos-web: {}", message);
    process::exit(1)
}
