//! The browser window: a header carrying the page's title and address, the
//! laid-out page below it, and scrolling.
//!
//! Layout is rebuilt only when the column width changes, so a scroll redraws
//! at the cost of a blit rather than re-measuring every glyph on the page.

use std::time::Duration;

use edos_lib::keymap::{Modifiers, keycode, map_keycode, update_modifiers};
use edos_render::metrics::space;
use edos_render::text::{self, Style, Surface};
use edos_render::theme::Theme;
use edos_render::window::{Window, WindowEvent, WindowEventType};

use crate::doc::Document;
use crate::view::{self, Layout, PAGE_PAD};

/// Opening size: wide enough for a readable measure, short enough to fit the
/// screens the guest boots at.
const WIN_W: u32 = 760;
const WIN_H: u32 = 560;
/// Pixels one wheel notch or arrow press moves the page.
const SCROLL_STEP: u32 = space(6);

pub struct Browser {
    window: Window,
    document: Document,
    url: String,
    layout: Layout,
    scroll: u32,
    header_h: u32,
    mods: Modifiers,
}

impl Browser {
    pub fn open(document: Document, url: String) -> Result<Browser, i64> {
        let window = Window::new(80, 60, WIN_W, WIN_H)?;
        let header_h = text::line_height(Style::new(0)) + space(2) * 2 + 1;
        let layout = Layout::build(&document, WIN_W);
        let mut browser = Browser {
            window,
            document,
            url,
            layout,
            scroll: 0,
            header_h,
            mods: Modifiers::default(),
        };
        browser.window.set_title(&browser.window_title())?;
        browser.window.show()?;
        Ok(browser)
    }

    fn window_title(&self) -> String {
        if self.document.title.is_empty() {
            "edos-web".to_string()
        } else {
            format!("{} - edos-web", self.document.title)
        }
    }

    pub fn run(&mut self) {
        let mut events = [WindowEvent::default(); 16];
        loop {
            if let Ok(count) = self.window.poll_events(&mut events) {
                for index in 0..count {
                    if !self.handle(events[index]) {
                        return;
                    }
                }
            }
            self.draw();
            std::thread::sleep(Duration::from_millis(16));
        }
    }

    /// Returns false when the window should close.
    fn handle(&mut self, event: WindowEvent) -> bool {
        match event.event_type() {
            Some(WindowEventType::CloseRequested) => return false,
            Some(WindowEventType::Resize) => {
                let (width, height) = (event.x as u32, event.y as u32);
                if self.window.resize(width, height).is_ok() {
                    self.relayout();
                }
            }
            Some(WindowEventType::MouseScroll) => {
                // Positive is a scroll up, which moves the page towards its top.
                let delta = event.data as i32;
                if delta > 0 {
                    self.scroll_by(-(SCROLL_STEP as i32));
                } else if delta < 0 {
                    self.scroll_by(SCROLL_STEP as i32);
                }
            }
            Some(WindowEventType::KeyRelease) => {
                update_modifiers(&mut self.mods, event.code, false);
            }
            Some(WindowEventType::KeyPress) => {
                if update_modifiers(&mut self.mods, event.code, true) {
                    return true;
                }
                // Alt marks a chord as the window manager's.
                if self.mods.alt {
                    return true;
                }
                let page = self.viewport_h().saturating_sub(SCROLL_STEP) as i32;
                // The kernel routes scancodes, so a letter key is what the
                // layout says it is, not what the scancode is called.
                if map_keycode(event.code, &self.mods) == Some('q') {
                    return false;
                }
                match event.code {
                    keycode::ESCAPE => return false,
                    keycode::ARROW_DOWN => self.scroll_by(SCROLL_STEP as i32),
                    keycode::ARROW_UP => self.scroll_by(-(SCROLL_STEP as i32)),
                    keycode::PAGE_DOWN | keycode::SPACEBAR => self.scroll_by(page),
                    keycode::PAGE_UP => self.scroll_by(-page),
                    keycode::HOME => self.scroll = 0,
                    keycode::END => self.scroll = self.max_scroll(),
                    _ => {}
                }
            }
            _ => {}
        }
        true
    }

    fn relayout(&mut self) {
        if self.layout.width != self.window.width {
            self.layout = Layout::build(&self.document, self.window.width);
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    fn viewport_h(&self) -> u32 {
        self.window.height.saturating_sub(self.header_h)
    }

    fn max_scroll(&self) -> u32 {
        self.layout.height.saturating_sub(self.viewport_h())
    }

    fn scroll_by(&mut self, delta: i32) {
        let next = self.scroll as i64 + delta as i64;
        self.scroll = next.clamp(0, self.max_scroll() as i64) as u32;
    }

    fn draw(&mut self) {
        let (width, height, header_h) = (self.window.width, self.window.height, self.header_h);
        let title = self.window_title();
        let address = self.url.clone();
        self.window.fill(Theme::DEFAULT.background.raw());

        let layout = &self.layout;
        let scroll = self.scroll;
        let Some(buffer) = self.window.buffer_mut() else {
            return;
        };

        view::draw(layout, buffer, width, height, header_h, scroll);

        // The header is drawn last so a page scrolled under it is covered.
        header(buffer, width, height, header_h, &title, &address);
        self.window.swap_buffers();
    }
}

/// The strip carrying the page title and the address it came from.
fn header(buffer: &mut [u32], width: u32, height: u32, header_h: u32, title: &str, url: &str) {
    for y in 0..header_h.min(height) {
        let color = if y + 1 == header_h {
            Theme::DEFAULT.window_border_highlight.raw()
        } else {
            Theme::DEFAULT.title_inactive_top.raw()
        };
        let row = y as usize * width as usize;
        buffer[row..row + width as usize].fill(color);
    }

    let mut surface = Surface::new(buffer, width, height);
    surface.clip = Some((0, 0, width as i32, header_h as i32 - 1));
    let y = space(2) as i32;
    let title_style = Style::new(Theme::DEFAULT.title_text.raw())
        .with_weight(edos_render::font::Weight::Semibold);
    let title = title.trim_end_matches(" - edos-web");
    text::draw(&mut surface, PAGE_PAD as i32, y, title, title_style);

    // The address is right-aligned so a long title does not push it off, and
    // it is measured rather than counted because the face is proportional.
    let url_style =
        Style::new(Theme::DEFAULT.label_text.raw()).with_px(edos_render::font::size::CAPTION);
    let url_w = text::width(url, url_style);
    let title_w = text::width(title, title_style);
    let x = width.saturating_sub(PAGE_PAD + url_w) as i32;
    if x > (PAGE_PAD + title_w + space(2)) as i32 {
        text::draw(&mut surface, x, y + 2, url, url_style);
    }
}
