//! The browser window: a header carrying a back button, the page's title and
//! its address, the laid-out page below it, scrolling, and clickable links.
//!
//! Layout is rebuilt only when the column width changes, so a scroll redraws
//! at the cost of a blit rather than re-measuring every glyph on the page.

use std::time::Duration;

use edos_lib::keymap::{Modifiers, keycode, map_keycode, update_modifiers};
use edos_render::metrics::space;
use edos_render::text::{self, Style, Surface};
use edos_render::theme::Theme;
use edos_render::window::{Window, WindowEvent, WindowEventType};

use crate::css::Viewport;
use crate::doc::Document;
use crate::view::{self, Layout, PAGE_PAD};

/// Opening size: wide enough for a readable measure, short enough to fit the
/// screens the guest boots at.
pub const WIN_W: u32 = 760;
pub const WIN_H: u32 = 560;
/// Pixels one wheel notch or arrow press moves the page.
const SCROLL_STEP: u32 = space(6);
/// The back button's label. Its width is measured, never counted.
const BACK_LABEL: &str = "< Back";

/// A page the browser came from, kept so going back neither refetches it nor
/// forgets where it was scrolled to.
struct Entry {
    document: Document,
    url: String,
    scroll: u32,
}

pub struct Browser {
    window: Window,
    document: Document,
    url: String,
    layout: Layout,
    scroll: u32,
    header_h: u32,
    mods: Modifiers,
    history: Vec<Entry>,
    /// Why the last navigation failed, shown in place of the address. A dead
    /// link leaves the page that carried it on screen.
    status: Option<String>,
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
            history: Vec::new(),
            status: None,
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
            Some(WindowEventType::MouseButton) => {
                // Left button, on the press rather than the release.
                if event.code == 0 && event.data != 0 {
                    self.on_click(event.x, event.y);
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
                    keycode::BACKSPACE => self.back(),
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

    /// Take a new window size. A media query is answered against the window, so
    /// a resize can change the document itself and not only where its lines
    /// break; the document decides, and says so only when an answer moved.
    fn relayout(&mut self) {
        if let Some(document) = self
            .document
            .reflow(self.viewport(), &crate::fetch_subresource)
        {
            self.document = document;
            self.layout = Layout::build(&self.document, self.window.width);
            let _ = self.window.set_title(&self.window_title());
            // Said on stdout, as a navigation is, so a headless run can tell a
            // re-cascade from a reflow that only moved the line breaks.
            println!(
                "edos-web: ~ {}x{} - {} blocks",
                self.window.width,
                self.viewport_h(),
                self.document.blocks.len()
            );
        } else if self.layout.width != self.window.width {
            self.layout = Layout::build(&self.document, self.window.width);
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// A click in the chrome works the back button; one in the page follows
    /// whatever link is under it.
    fn on_click(&mut self, x: i32, y: i32) {
        if y < self.header_h as i32 {
            if !self.history.is_empty() && x >= back_x(0) && x < back_x(1) {
                self.back();
            }
            return;
        }
        let page_y = y - self.header_h as i32 + self.scroll as i32;
        let Some(target) = self.layout.link_at(x, page_y).map(str::to_string) else {
            return;
        };
        self.navigate(&target);
    }

    /// The window as a media query sees it: the page's own area, so a query the
    /// page writes about the viewport is answered with the space it gets.
    fn viewport(&self) -> Viewport {
        Viewport::new(
            self.window.width,
            self.window.height.saturating_sub(self.header_h),
            crate::doc::ROOT_PX,
        )
    }

    /// Fetch `target`, show it, and push the page it replaced onto the history.
    fn navigate(&mut self, target: &str) {
        let viewport = self.viewport();
        let loaded = crate::load(target).map(|(html, base)| {
            let address = base.to_string();
            (
                crate::doc::parse(&html, base, &crate::fetch_subresource, viewport),
                address,
            )
        });
        let (document, address) = match loaded {
            Ok(loaded) => loaded,
            Err(message) => {
                println!("edos-web: {} - {}", target, message);
                self.status = Some(message);
                return;
            }
        };
        // Said on stdout as well as drawn, so a headless run can see which
        // page a click actually reached.
        println!(
            "edos-web: -> {} - {} blocks from {}",
            document.display_title(),
            document.blocks.len(),
            address
        );
        let previous = Entry {
            document: std::mem::replace(&mut self.document, document),
            url: std::mem::replace(&mut self.url, address),
            scroll: self.scroll,
        };
        self.history.push(previous);
        self.after_load();
    }

    fn back(&mut self) {
        let Some(entry) = self.history.pop() else {
            return;
        };
        println!("edos-web: <- {}", entry.url);
        self.document = entry.document;
        self.url = entry.url;
        self.after_load();
        self.scroll = entry.scroll.min(self.max_scroll());
    }

    /// Adopt a document the browser has just switched to: it needs its own
    /// layout at the current width, not the one the previous page was measured
    /// at, so this cannot go through `relayout`.
    fn after_load(&mut self) {
        self.status = None;
        self.layout = Layout::build(&self.document, self.window.width);
        self.scroll = 0;
        let _ = self.window.set_title(&self.window_title());
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
        let chrome = Chrome {
            title: title.trim_end_matches(" - edos-web").to_string(),
            address: self.status.clone().unwrap_or_else(|| self.url.clone()),
            failed: self.status.is_some(),
            back: !self.history.is_empty(),
        };
        self.window.fill(Theme::DEFAULT.background.raw());

        let layout = &self.layout;
        let scroll = self.scroll;
        let Some(buffer) = self.window.buffer_mut() else {
            return;
        };

        view::draw(layout, buffer, width, height, header_h, scroll);

        // The header is drawn last so a page scrolled under it is covered.
        header(buffer, width, height, header_h, &chrome);
        self.window.swap_buffers();
    }
}

/// What the header shows about the page currently on screen.
struct Chrome {
    title: String,
    address: String,
    /// The address slot is carrying an error rather than a URL.
    failed: bool,
    /// There is somewhere to go back to.
    back: bool,
}

fn back_style(enabled: bool) -> Style {
    let color = if enabled {
        Theme::DEFAULT.title_text
    } else {
        Theme::DEFAULT.text_disabled
    };
    Style::new(color.raw()).with_px(edos_render::font::size::CAPTION)
}

/// The back button's left edge at `edge == 0` and its right edge at `edge == 1`,
/// so the hit test and the draw cannot disagree about where it is.
fn back_x(edge: u32) -> i32 {
    let width = text::width(BACK_LABEL, back_style(true)) + space(2);
    (PAGE_PAD + width * edge) as i32
}

/// The strip carrying the back button, the page title and its address.
fn header(buffer: &mut [u32], width: u32, height: u32, header_h: u32, chrome: &Chrome) {
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

    // The button keeps its slot when there is nowhere to go back to, drawn
    // disabled, so the title does not move under the pointer on the first
    // navigation.
    let back = back_style(chrome.back);
    text::draw(&mut surface, back_x(0), y + 2, BACK_LABEL, back);

    let title_x = back_x(1) + space(2) as i32;
    text::draw(&mut surface, title_x, y, &chrome.title, title_style);

    // The address is right-aligned so a long title does not push it off, and
    // it is measured rather than counted because the face is proportional.
    let url_style = Style::new(if chrome.failed {
        Theme::DEFAULT.warning.raw()
    } else {
        Theme::DEFAULT.label_text.raw()
    })
    .with_px(edos_render::font::size::CAPTION);
    let url_w = text::width(&chrome.address, url_style);
    let title_w = text::width(&chrome.title, title_style);
    let x = width.saturating_sub(PAGE_PAD + url_w) as i32;
    if x > title_x + (title_w + space(2)) as i32 {
        text::draw(&mut surface, x, y + 2, &chrome.address, url_style);
    }
}
