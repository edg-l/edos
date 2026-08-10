//! A BMP viewer: one window, one image, scaled to the window or shown at its
//! own size.
//!
//! The decoding and the scaling both live in `edos_render::image`, the same two
//! functions the compositor uses for a wallpaper, so a viewer is the window
//! syscalls plus a policy about where the picture sits inside the frame.

use std::time::Duration;

use edos_lib::keymap::{Modifiers, map_keycode, update_modifiers};
use edos_render::image::{Image, ImageError, decode_bmp};
use edos_render::metrics::{TEXT_CELL_HEIGHT, space};
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

/// The image resampled for one window size, kept so a redraw at 60Hz does not
/// rescale a megapixel every frame.
struct Scaled {
    pixels: Vec<u32>,
    width: u32,
    height: u32,
    /// The (mode, viewport) this was produced for.
    key: (Mode, u32, u32),
}

impl Scaled {
    fn build(image: &Image, mode: Mode, view_w: u32, view_h: u32) -> Self {
        let key = (mode, view_w, view_h);
        match mode {
            Mode::Fit => {
                let (w, h) = image.fit_size(view_w, view_h);
                Scaled {
                    pixels: image.scaled_to_fit(w, h),
                    width: w,
                    height: h,
                    key,
                }
            }
            // Nothing to resample: the source is what gets drawn, and the blit
            // crops it.
            Mode::Actual => Scaled {
                pixels: image.pixels.clone(),
                width: image.width,
                height: image.height,
                key,
            },
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

/// Window size for a freshly opened image: its own size, bounded both ways.
fn initial_size(image: &Image) -> (u32, u32) {
    let (w, h) = image.fit_size(MAX_WIN_W, MAX_WIN_H - FOOTER_H);
    (
        w.clamp(MIN_WIN_W, MAX_WIN_W),
        (h + FOOTER_H).clamp(MIN_WIN_H, MAX_WIN_H),
    )
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: imgview <file.bmp>");
        eprintln!("  q quit, f fit to window, 1 actual size");
        std::process::exit(2);
    };

    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("imgview: {path}: {e}");
            std::process::exit(1);
        }
    };
    let image = match decode_bmp(&bytes) {
        Ok(i) => i,
        Err(ImageError::Malformed) => {
            eprintln!("imgview: {path}: not a BMP, or truncated");
            std::process::exit(1);
        }
        Err(ImageError::Unsupported) => {
            eprintln!("imgview: {path}: compressed or paletted BMPs are not supported");
            std::process::exit(1);
        }
    };

    let name = path.rsplit('/').next().unwrap_or(&path).to_string();
    let (win_w, win_h) = initial_size(&image);
    let mut window = match Window::new(120, 120, win_w, win_h) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("imgview: cannot create a window: {e:?}");
            std::process::exit(1);
        }
    };
    let _ = window.set_title(&format!("{name} - {}x{}", image.width, image.height));
    if let Err(e) = window.show() {
        eprintln!("imgview: cannot show the window: {e:?}");
        std::process::exit(1);
    }
    println!(
        "imgview: {name} {}x{} - q quit, f fit, 1 actual size",
        image.width, image.height
    );

    let mut mode = Mode::Fit;
    let mut scaled = Scaled::build(&image, mode, window.width, window.height - FOOTER_H);
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
            scaled = Scaled::build(&image, mode, window.width, view_h);
        }

        // The letterbox is the window background, so an image of another aspect
        // ratio sits on the same ground the rest of the shell uses.
        window.fill(colors::BACKGROUND);
        let footer = format!(
            "{name}  {}x{}  {}",
            image.width,
            image.height,
            match mode {
                Mode::Fit => format!("fit {}%", zoom_percent(image.width, scaled.width)),
                Mode::Actual => String::from("100%"),
            }
        );
        let (w, h) = (window.width, window.height);
        if let Some(buf) = window.buffer_mut() {
            blit_centred(buf, w, h, &scaled, w, view_h);
            Label::with_color(
                0,
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
