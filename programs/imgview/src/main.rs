//! An image viewer: one window, one picture, scaled to the window or shown at
//! its own size. It reads BMP and SVG.
//!
//! The decoding and the scaling live in `edos_render::image`, the same two
//! functions the compositor uses for a wallpaper, so a viewer is the window
//! syscalls plus a policy about where the picture sits inside the frame.
//!
//! The two kinds of source are not the same thing wearing different clothes. A
//! raster is resampled, so fitting it to a larger window costs detail and the
//! viewer refuses to magnify it; a vector is re-rendered at whatever size the
//! window is, so fitting costs nothing and filling the window is the right
//! default. That difference is the whole reason `Source` is an enum rather
//! than a decode step that lands on `Image` either way.

use std::time::Duration;

use edos_lib::keymap::{Modifiers, map_keycode, update_modifiers};
use edos_render::image::{Image, ImageError, Svg, decode_raster, looks_like_svg};
use edos_render::metrics::{TEXT_CELL_HEIGHT, space};
use edos_render::theme::Theme;
use edos_render::widgets::{Label, Widget, colors};
use edos_render::window::{Window, WindowEvent, WindowEventType};

/// A window larger than this is unlikely to fit the screen it opens on, and the
/// compositor does not tell a client how big that screen is.
const MAX_WIN_W: u32 = 900;
const MAX_WIN_H: u32 = 650;
/// Small enough to still carry a title bar and the footer.
const MIN_WIN_W: u32 = 240;
const MIN_WIN_H: u32 = 160;

/// Padding above and below the footer text.
const FOOTER_PAD: u32 = space(1);
/// Height of the strip that carries the status line.
const FOOTER_H: u32 = TEXT_CELL_HEIGHT + FOOTER_PAD * 2;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Scale to the window, never past the image's own size.
    Fit,
    /// One image pixel per screen pixel, centred and cropped.
    Actual,
}

/// What was opened: pixels that can only be resampled, or a document that can
/// be drawn again at any size.
enum Source {
    Raster(Image),
    /// Boxed: an `Svg` is 392 bytes against `Image`'s 32, and every
    /// `Source` would otherwise be sized for the larger of the two.
    Vector(Box<Svg>),
}

impl Source {
    /// The size the file claims for itself, which is what the title bar and the
    /// footer report and what the window opens at.
    fn intrinsic_size(&self) -> (u32, u32) {
        match self {
            Source::Raster(image) => (image.width, image.height),
            Source::Vector(svg) => svg.intrinsic_size(),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Source::Raster(_) => "BMP",
            Source::Vector(_) => "SVG",
        }
    }

    /// The size a fitted picture occupies in a viewport this big.
    fn fit_size(&self, view_w: u32, view_h: u32) -> (u32, u32) {
        match self {
            Source::Raster(image) => image.fit_size(view_w, view_h),
            Source::Vector(svg) => svg.fit_size(view_w, view_h),
        }
    }
}

/// The picture prepared for one window size, kept so a redraw at 60Hz does not
/// rescale a megapixel, or re-render a document, every frame.
struct Scaled {
    pixels: Vec<u32>,
    width: u32,
    height: u32,
    /// The (mode, viewport) this was produced for.
    key: (Mode, u32, u32),
}

impl Scaled {
    fn build(source: &Source, mode: Mode, view_w: u32, view_h: u32) -> Self {
        let key = (mode, view_w, view_h);
        let (width, height) = match mode {
            Mode::Fit => source.fit_size(view_w, view_h),
            Mode::Actual => source.intrinsic_size(),
        };

        let pixels = match source {
            Source::Raster(image) => match mode {
                Mode::Fit => image.scaled_to_fit(width, height),
                // Nothing to resample: the source is what gets drawn, and the
                // blit crops it.
                Mode::Actual => image.pixels.clone(),
            },
            // Rendered at the size it will be shown at, over the ground it will
            // be shown on, so nothing is ever scaled twice.
            Source::Vector(svg) => match svg.render(width, height, Theme::DEFAULT.background) {
                Ok(image) => image.pixels,
                Err(_) => vec![colors::BACKGROUND; width as usize * height as usize],
            },
        };

        Scaled {
            pixels,
            width,
            height,
            key,
        }
    }
}

/// Copy `src` into `dst` centred on the viewport, cropping whatever hangs out.
fn blit_centred(dst: &mut [u32], dst_w: u32, dst_h: u32, src: &Scaled, view_w: u32, view_h: u32) {
    // A negative origin is the crop case: the image is wider or taller than the
    // viewport, so drawing starts partway into the source.
    let origin_x = (view_w as i64 - src.width as i64) / 2;
    let origin_y = (view_h as i64 - src.height as i64) / 2;

    for y in 0..src.height as i64 {
        let dy = origin_y + y;
        if dy < 0 || dy >= view_h.min(dst_h) as i64 {
            continue;
        }
        let row = &src.pixels[y as usize * src.width as usize..][..src.width as usize];
        for (x, px) in row.iter().enumerate() {
            let dx = origin_x + x as i64;
            if dx < 0 || dx >= view_w.min(dst_w) as i64 {
                continue;
            }
            dst[dy as usize * dst_w as usize + dx as usize] = *px;
        }
    }
}

/// Window size for a freshly opened picture: its own size, bounded both ways.
fn initial_size(source: &Source) -> (u32, u32) {
    let (own_w, own_h) = source.intrinsic_size();
    let box_w = MAX_WIN_W;
    let box_h = MAX_WIN_H - FOOTER_H;
    // A drawing smaller than the box opens at its own size, whichever kind it
    // is: a window that opened larger than the file asks for would be claiming
    // detail the raster case has not got.
    let (w, h) = if own_w <= box_w && own_h <= box_h {
        (own_w, own_h)
    } else {
        source.fit_size(box_w, box_h)
    };
    (
        w.clamp(MIN_WIN_W, MAX_WIN_W),
        (h + FOOTER_H).clamp(MIN_WIN_H, MAX_WIN_H),
    )
}

/// Read a file and work out what it is from its bytes.
fn open(path: &str) -> Result<Source, String> {
    let bytes = std::fs::read(path).map_err(|err| format!("{path}: {err}"))?;
    if looks_like_svg(&bytes) {
        return Svg::parse(&bytes)
            .map(|svg| Source::Vector(Box::new(svg)))
            .map_err(|err| match err {
                ImageError::Svg(message) => format!("{path}: {message}"),
                other => format!("{path}: {other:?}"),
            });
    }
    decode_raster(&bytes)
        .map(Source::Raster)
        .map_err(|err| match err {
            ImageError::Malformed => {
                format!("{path}: not a picture this can read, or truncated")
            }
            ImageError::Unsupported => {
                format!("{path}: compressed or paletted BMPs are not supported")
            }
            ImageError::Raster(message) => format!("{path}: {message}"),
            other => format!("{path}: {other:?}"),
        })
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: imgview <file.bmp|file.svg>");
        eprintln!("  q quit, f fit to window, 1 actual size");
        std::process::exit(2);
    };

    let source = match open(&path) {
        Ok(source) => source,
        Err(message) => {
            eprintln!("imgview: {message}");
            std::process::exit(1);
        }
    };

    let name = path.rsplit('/').next().unwrap_or(&path).to_string();
    let (own_w, own_h) = source.intrinsic_size();
    let (win_w, win_h) = initial_size(&source);
    let mut window = match Window::new(120, 120, win_w, win_h) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("imgview: cannot create a window: {e:?}");
            std::process::exit(1);
        }
    };
    let _ = window.set_title(&format!("{name} - {own_w}x{own_h}"));
    if let Err(e) = window.show() {
        eprintln!("imgview: cannot show the window: {e:?}");
        std::process::exit(1);
    }
    println!(
        "imgview: {name} {} {own_w}x{own_h} - q quit, f fit, 1 actual size",
        source.kind()
    );

    let mut mode = Mode::Fit;
    let mut scaled = Scaled::build(&source, mode, window.width, window.height - FOOTER_H);
    let mut events = [WindowEvent::default(); 16];
    let mut mods = Modifiers::default();

    loop {
        if let Ok(count) = window.poll_events(&mut events) {
            for event in &events[..count] {
                match event.event_type() {
                    Some(WindowEventType::CloseRequested) => return,
                    Some(WindowEventType::Resize) => {
                        let _ = window.resize(event.x as u32, event.y as u32);
                    }
                    // The kernel routes scancodes and has no keyboard layout,
                    // so a client that wants letters maps them itself.
                    Some(WindowEventType::KeyPress) => {
                        if update_modifiers(&mut mods, event.code, true) {
                            continue;
                        }
                        // Alt marks a chord as the window manager's, so `q`
                        // held with Alt is not this program's quit key.
                        if mods.alt {
                            continue;
                        }
                        match map_keycode(event.code, &mods) {
                            Some('q') => return,
                            Some('f') => mode = Mode::Fit,
                            Some('1') => mode = Mode::Actual,
                            _ => {}
                        }
                    }
                    Some(WindowEventType::KeyRelease) => {
                        update_modifiers(&mut mods, event.code, false);
                    }
                    _ => {}
                }
            }
        }

        let view_h = window.height.saturating_sub(FOOTER_H);
        if scaled.key != (mode, window.width, view_h) {
            scaled = Scaled::build(&source, mode, window.width, view_h);
        }

        // The letterbox is the window background, so an image of another aspect
        // ratio sits on the same ground the rest of the shell uses.
        window.fill(colors::BACKGROUND);
        let footer = format!(
            "{name}  {}  {own_w}x{own_h}  {}",
            source.kind(),
            match mode {
                Mode::Fit => format!("fit {}%", zoom_percent(own_w, scaled.width)),
                Mode::Actual => String::from("100%"),
            }
        );
        let (w, h) = (window.width, window.height);
        if let Some(buf) = window.buffer_mut() {
            blit_centred(buf, w, h, &scaled, w, view_h);
            Label::with_color(
                space(1) as i32,
                (view_h + FOOTER_PAD) as i32,
                &footer,
                colors::LABEL_TEXT,
            )
            .draw(buf, w, h);
        }
        window.swap_buffers();
        std::thread::sleep(Duration::from_millis(16));
    }
}

/// Displayed scale, rounded, of `shown` pixels standing in for `source`.
fn zoom_percent(source: u32, shown: u32) -> u32 {
    if source == 0 {
        return 100;
    }
    ((shown as u64 * 100 + source as u64 / 2) / source as u64) as u32
}
