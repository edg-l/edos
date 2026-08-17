//! Write what the display is showing to a BMP.
//!
//! The compositor's picture is the only one a screenshot can reach. A cursor
//! the display holds on its own plane is not in the framebuffer at all, so a
//! pointer that appears here is one somebody composited, and a pointer that
//! does not is the plane's. That distinction is the whole reason to have this:
//! from inside the guest it is otherwise impossible to tell which of the two a
//! cursor on screen is.
//!
//! A double-buffered display is captured a page at a time, since the two pages
//! are only identical when every frame repaints both. A region drawn into one
//! and not the other alternates on screen at the flip rate.

use std::fs::File;
use std::io::{BufWriter, Write};

use edos_render::graphics::Framebuffer;

/// Bytes of header before the pixel data: 14 of file header and 40 of
/// BITMAPINFOHEADER.
const PIXEL_OFFSET: u32 = 54;
const BITMAPINFOHEADER: u32 = 40;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| "/tmp/screen.bmp".to_string());

    let framebuffer = Framebuffer::new();
    let info = match framebuffer.mmap_info() {
        Ok(info) => info,
        Err(e) => {
            eprintln!("screenshot: mmap info: {e:?}");
            std::process::exit(1);
        }
    };
    if info.is_identity == 0 {
        eprintln!("screenshot: the framebuffer is not 32-bit BGRX, nothing here reads it");
        std::process::exit(1);
    }
    let vram = match framebuffer.mmap_vram() {
        Ok(vram) => vram,
        Err(e) => {
            eprintln!("screenshot: map framebuffer: {e:?}");
            std::process::exit(1);
        }
    };

    let width = vram.width;
    let height = vram.height;
    let pitch = vram.pitch_pixels;

    if vram.double_buffered {
        // Both pages are copied before either is written. Writing one and then
        // reading the other compares two different instants, and a caret
        // blinking between them looks exactly like the staleness this is here
        // to detect.
        let pages: [Vec<u32>; 2] = [vram.page(0).to_vec(), vram.page(1).to_vec()];

        // Named for the page rather than numbered from the caller's point of
        // view: which one is on screen alternates every flip, so a file called
        // "the front page" would name a different page each time it was
        // written.
        for (page, pixels) in pages.iter().enumerate() {
            let page_path = numbered(&path, page);
            match write_bmp(&page_path, pixels, width, height, pitch) {
                Ok(()) => println!("{page_path}: {width}x{height}"),
                Err(e) => {
                    eprintln!("screenshot: {page_path}: {e}");
                    std::process::exit(1);
                }
            }
        }
        report_divergence(&pages[0], &pages[1], width, height, pitch);
    } else {
        let pixels = vram.page(0);
        match write_bmp(&path, pixels, width, height, pitch) {
            Ok(()) => println!("{path}: {width}x{height}"),
            Err(e) => {
                eprintln!("screenshot: {path}: {e}");
                std::process::exit(1);
            }
        }
    }
}

/// Say whether the two pages hold the same picture, and where they do not.
///
/// One frame of difference is normal and not a fault: the page just painted
/// carries the newest damage, and the other receives it on the next flip. So
/// the box below is ordinarily the last thing that changed on screen, and the
/// way to read it is to ask whether it is.
///
/// A box that is *not* this frame's damage is the interesting one, because the
/// display alternates pages: a region painted into one and never into the other
/// is shown every other frame, which is something blinking in place while
/// nothing on screen is changing.
fn report_divergence(a: &[u32], b: &[u32], width: usize, height: usize, pitch: usize) {
    let mut differing = 0usize;
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (usize::MAX, usize::MAX, 0usize, 0usize);
    for y in 0..height {
        let row = y * pitch;
        for x in 0..width {
            if a[row + x] != b[row + x] {
                differing += 1;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    if differing == 0 {
        println!("pages agree: both hold the same picture");
        return;
    }
    println!(
        "pages differ by {differing} pixels, box {}x{} at {},{}",
        max_x - min_x + 1,
        max_y - min_y + 1,
        min_x,
        min_y
    );
    println!("expected when that box is the last thing that changed; a blink if it is not");
}

/// `shot.bmp` becomes `shot.page0.bmp`, keeping the extension where a viewer
/// looks for it.
fn numbered(path: &str, page: usize) -> String {
    match path.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}.page{page}.{ext}"),
        None => format!("{path}.page{page}"),
    }
}

/// A 32-bit BI_RGB bitmap, rows bottom-up, which is the form every decoder
/// reads including this system's own.
fn write_bmp(
    path: &str,
    pixels: &[u32],
    width: usize,
    height: usize,
    pitch: usize,
) -> std::io::Result<()> {
    let row_bytes = width * 4;
    let size = PIXEL_OFFSET + (row_bytes * height) as u32;
    let mut out = BufWriter::new(File::create(path)?);

    out.write_all(b"BM")?;
    out.write_all(&size.to_le_bytes())?;
    out.write_all(&0u32.to_le_bytes())?;
    out.write_all(&PIXEL_OFFSET.to_le_bytes())?;

    out.write_all(&BITMAPINFOHEADER.to_le_bytes())?;
    out.write_all(&(width as i32).to_le_bytes())?;
    out.write_all(&(height as i32).to_le_bytes())?;
    out.write_all(&1u16.to_le_bytes())?; // planes
    out.write_all(&32u16.to_le_bytes())?; // bits per pixel
    out.write_all(&0u32.to_le_bytes())?; // BI_RGB
    out.write_all(&((row_bytes * height) as u32).to_le_bytes())?;
    for _ in 0..4 {
        out.write_all(&0u32.to_le_bytes())?;
    }

    let mut row = vec![0u8; row_bytes];
    for y in (0..height).rev() {
        let src = &pixels[y * pitch..y * pitch + width];
        for (x, px) in src.iter().enumerate() {
            // The framebuffer word is 0x00RRGGBB and BMP wants B, G, R, A.
            row[x * 4] = (*px & 0xFF) as u8;
            row[x * 4 + 1] = ((*px >> 8) & 0xFF) as u8;
            row[x * 4 + 2] = ((*px >> 16) & 0xFF) as u8;
            row[x * 4 + 3] = 0xFF;
        }
        out.write_all(&row)?;
    }
    out.flush()
}
