//! Outline fonts for the shell, rasterized on demand and cached.
//!
//! The shell used to be set entirely in one bitmap face at one size and one
//! weight, so every distinction on screen had to be carried by colour. Real
//! faces at real sizes give the interface a second axis.
//!
//! Faces are loaded from `/share/fonts` at startup rather than embedded,
//! because `edos_render` links into the window manager, the panel, the
//! terminal and every widget program, and a megabyte of outlines in each is a
//! megabyte the disk image does not need. A face that fails to load leaves its
//! slot empty and the caller falls back to the bitmap font, so a missing file
//! degrades the type rather than killing the session.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use fontdue::{Font, FontSettings};

/// Where `make filesystem` puts the outlines.
const FONT_DIR: &str = "/share/fonts";

/// Which face a run of text is set in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Family {
    /// Proportional. Everything the user reads *as interface*: titles, labels,
    /// buttons, the panel.
    #[default]
    Sans,
    /// Fixed advance. Reserved for what is genuinely a grid of characters: the
    /// terminal, the editor, a hex dump.
    Mono,
}

/// How heavy a run of text is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Weight {
    #[default]
    Regular,
    Medium,
    Semibold,
}

impl Weight {
    fn index(self) -> usize {
        match self {
            Weight::Regular => 0,
            Weight::Medium => 1,
            Weight::Semibold => 2,
        }
    }
}

/// The type scale. One ladder, so a control cannot invent its own size.
pub mod size {
    /// Secondary text: hints, timestamps, units beside a value.
    pub const CAPTION: u32 = 12;
    /// Everything interactive: button labels, field text, panel entries.
    pub const BODY: u32 = 14;
    /// Window titles and section headings.
    pub const TITLE: u32 = 15;
}

/// A rasterized glyph: an 8-bit coverage mask and where to put it.
pub struct Glyph {
    pub coverage: Vec<u8>,
    pub width: usize,
    pub height: usize,
    /// Offset from the pen position to the left edge of the mask.
    pub left: i32,
    /// Offset from the baseline *down* to the top edge of the mask.
    pub top: i32,
    /// How far the pen moves after drawing this glyph.
    pub advance: f32,
}

struct Faces {
    /// Sans in Regular, Medium, Semibold.
    sans: [Option<Font>; 3],
    mono: Option<Font>,
}

impl Faces {
    fn get(&self, family: Family, weight: Weight) -> Option<&Font> {
        match family {
            // Mono ships one weight: the terminal and the editor are the only
            // users and neither has a heading.
            Family::Mono => self.mono.as_ref(),
            Family::Sans => self.sans[weight.index()]
                .as_ref()
                // A missing weight falls back to Regular rather than to the
                // bitmap font, so a partial install still sets proportional
                // type and only loses the emphasis.
                .or(self.sans[0].as_ref()),
        }
    }
}

fn load(name: &str) -> Option<Font> {
    let bytes = std::fs::read(format!("{FONT_DIR}/{name}")).ok()?;
    Font::from_bytes(bytes, FontSettings::default()).ok()
}

fn faces() -> &'static Faces {
    static FACES: OnceLock<Faces> = OnceLock::new();
    FACES.get_or_init(|| Faces {
        sans: [
            load("Sans-Regular.ttf"),
            load("Sans-Medium.ttf"),
            load("Sans-Semibold.ttf"),
        ],
        mono: load("Mono-Regular.ttf"),
    })
}

/// Whether outline faces are available at all. False means every caller should
/// stay on the bitmap font.
pub fn available() -> bool {
    faces().sans[0].is_some() || faces().mono.is_some()
}

type CacheKey = (Family, Weight, u32, char);

fn cache() -> &'static Mutex<HashMap<CacheKey, &'static Glyph>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, &'static Glyph>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Rasterize a character, or return the cached mask.
///
/// Glyphs are leaked deliberately: a shell draws from a set of a few hundred
/// characters and never stops needing them, so the cache is the working set
/// rather than a growing cost. Leaking lets callers hold a `&'static Glyph`
/// without the cache lock, which matters because drawing a string would
/// otherwise take and drop the lock once per character.
pub fn glyph(family: Family, weight: Weight, px: u32, ch: char) -> Option<&'static Glyph> {
    let key = (family, weight, px, ch);
    if let Some(g) = cache().lock().ok()?.get(&key) {
        return Some(g);
    }

    let font = faces().get(family, weight)?;
    let (metrics, coverage) = font.rasterize(ch, px as f32);
    let leaked: &'static Glyph = Box::leak(Box::new(Glyph {
        coverage,
        width: metrics.width,
        height: metrics.height,
        left: metrics.xmin,
        top: metrics.height as i32 + metrics.ymin,
        advance: metrics.advance_width,
    }));
    cache().lock().ok()?.insert(key, leaked);
    Some(leaked)
}

/// Horizontal advance of a string, in pixels.
pub fn measure(family: Family, weight: Weight, px: u32, text: &str) -> u32 {
    let Some(font) = faces().get(family, weight) else {
        return 0;
    };
    let width: f32 = text
        .chars()
        .map(|ch| font.metrics(ch, px as f32).advance_width)
        .sum();
    width.ceil() as u32
}

/// Distance from the top of a line to the baseline, and the total line height.
///
/// Taken from the face's own metrics rather than a fraction of the size, so a
/// line of text sits where the designer put it and descenders are not clipped.
pub fn line_metrics(family: Family, weight: Weight, px: u32) -> (u32, u32) {
    let Some(font) = faces().get(family, weight) else {
        return (px, px);
    };
    match font.horizontal_line_metrics(px as f32) {
        Some(m) => (m.ascent.ceil() as u32, (m.new_line_size).ceil() as u32),
        None => (px, px),
    }
}

/// Advance of one character in a monospaced face, which is the width of every
/// cell in a character grid.
pub fn mono_advance(px: u32) -> u32 {
    measure(Family::Mono, Weight::Regular, px, "M").max(1)
}
