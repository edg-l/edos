//! A web browser for EDOS. Stage 1: fetch a URL, parse the HTML, and show the
//! document in a window -- or print it as text with `-d`, which is the only
//! rendering a headless run can assert on.
//!
//! See `doc/design/browser.md` for the stages and what each one is for.

use std::{env, fs, io::Write as _, process};

use edos_http::{Options, url::Url};

use crate::css::Viewport;

mod css;
mod doc;
mod net;
mod text;
mod ui;
mod view;

const DEFAULT_WIDTH: usize = 80;

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut width = DEFAULT_WIDTH;
    let mut links = false;
    let mut dump = false;
    let mut reader = true;
    let mut target: Option<String> = None;
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                usage();
                process::exit(0);
            }
            "-d" | "--dump" => dump = true,
            "-a" | "--all" => reader = false,
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

    if dump {
        let (html, base) = match load(&target) {
            Ok(loaded) => loaded,
            Err(message) => fail(&message),
        };
        // The window the page would have opened in answers its media queries,
        // which is the honest answer for `-d` too: the text dump is the same
        // document, read out rather than drawn.
        let viewport = Viewport::new(ui::WIN_W, ui::WIN_H, doc::ROOT_PX);
        let context = doc::Context::new(viewport, reader);
        let document = doc::parse(&html, base, &fetch_subresource, &context);
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

    // The window comes up before the first byte is asked for, so the loading
    // view covers the first page the way it covers every later one.
    match ui::Browser::open(doc::Document::blank(), String::new(), reader) {
        Ok(mut browser) => {
            browser.navigate(&target);
            browser.run();
        }
        Err(err) => fail(&format!("cannot create a window: {err}")),
    }
}

/// Fetch `target`, or read it from disk when it names a local file.
///
/// A path is worth accepting because it separates a parse or layout problem
/// from a network one, which is the first question when a page renders wrong.
pub(crate) fn load(target: &str) -> Result<(Vec<u8>, Url), String> {
    if let Some(path) = local_path(target) {
        let bytes = fs::read(&path).map_err(|e| format!("{}: {}", path, e))?;
        // A local document's origin is this machine, so its base is its own
        // path under the host that means the filesystem. Both `logo.bmp` beside
        // the page and `/share/icons/edos.svg` then resolve to a URL this
        // function reads straight back off disk.
        let base = Url::parse(&format!("http://{}{}", LOCAL_HOST, path))
            .map_err(|e| format!("{}: {}", path, e))?;
        return Ok((bytes, base));
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

/// The host a locally-read document is given, so that what it refers to
/// relative to itself comes back to this filesystem rather than to a network.
const LOCAL_HOST: &str = "localhost";

/// The file `target` names, for a path or for a URL on [`LOCAL_HOST`].
fn local_path(target: &str) -> Option<String> {
    if let Some(rest) = target.strip_prefix("./") {
        let cwd = env::current_dir().unwrap_or_else(|_| "/".into());
        return Some(cwd.join(rest).to_string_lossy().into_owned());
    }
    if target.starts_with('/') {
        return Some(target.to_string());
    }
    let rest = target
        .strip_prefix("http://")?
        .strip_prefix(LOCAL_HOST)
        .filter(|rest| rest.starts_with('/'))?;
    Some(rest.to_string())
}

/// Fetch something the document refers to by absolute URL: a stylesheet or an
/// image.
///
/// A subresource that cannot be had is not an error for the page -- it renders
/// with whatever styling and pictures did arrive -- so this reports nothing and
/// returns `None`.
pub(crate) fn fetch_subresource(url: &str) -> Option<Vec<u8>> {
    load(url).ok().map(|(bytes, _)| bytes)
}

fn usage() {
    eprintln!("usage: edos-web [-d [-l] [-w COLUMNS]] URL|FILE");
    eprintln!("  -d, --dump      print the page as text instead of opening a window");
    eprintln!("  -l, --links     number links and list their targets");
    eprintln!("  -a, --all       lay out the whole page, not just its <main>");
    eprintln!(
        "  -w, --width N   wrap to N columns (default {})",
        DEFAULT_WIDTH
    );
    eprintln!("in the window: click a link to follow it, or type an address and press Enter");
    eprintln!("  Ctrl+L address bar, F5 or Ctrl+R reload, Esc stops a load");
    eprintln!("  m shows the whole page rather than its main content, and back");
    eprintln!("  Backspace goes back, Shift+Backspace forward");
    eprintln!("  arrows, PageUp/PageDown, Home/End scroll; q closes");
}

fn fail(message: &str) -> ! {
    eprintln!("edos-web: {}", message);
    process::exit(1)
}
