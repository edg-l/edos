//! The browser window: a toolbar carrying back, forward, reload and an address
//! bar that can be typed into, the laid-out page below it, scrolling, and
//! clickable links.
//!
//! Layout is rebuilt only when the column width changes, so a scroll redraws
//! at the cost of a blit rather than re-measuring every glyph on the page.
//!
//! A page is loaded by [`crate::net::Loader`] on a thread of its own, and the
//! page on screen gives way to the loading view the moment the load starts:
//! the old page left standing through a slow fetch says nothing happened, and
//! the click that started it looks lost.

use std::mem;
use std::time::{Duration, Instant};

use edos_lib::keymap::{Modifiers, keycode, map_keycode, update_modifiers};
use edos_render::icons;
use edos_render::metrics::{CONTROL_HEIGHT, space};
use edos_render::text::{self, Style, Surface};
use edos_render::theme::Theme;
use edos_render::widgets::{TextInput, Widget, WidgetEvent, draw_rect};
use edos_render::window::{Window, WindowEvent, WindowEventType};

use crate::css::Viewport;
use crate::doc::Document;
use crate::net::{Loader, Page, Update};
use crate::view::{self, Layout, PAGE_PAD};

/// Opening size: wide enough for a readable measure, short enough to fit the
/// screens the guest boots at.
pub const WIN_W: u32 = 760;
pub const WIN_H: u32 = 560;
/// Pixels one wheel notch or arrow press moves the page.
const SCROLL_STEP: u32 = space(6);

/// A toolbar button, in the order they sit in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Button {
    Back,
    Forward,
    /// Reloads the page, or stops the load in flight.
    Reload,
    /// Lays out the page's `<main>` alone, or the whole document.
    Reader,
}

const BUTTON_COUNT: usize = 4;
const BUTTONS: [Button; BUTTON_COUNT] = [
    Button::Back,
    Button::Forward,
    Button::Reload,
    Button::Reader,
];

/// A page the browser came from or went back from, kept so moving through the
/// history neither refetches a page nor forgets where it was scrolled to.
struct Entry {
    document: Document,
    url: String,
    scroll: u32,
}

/// A load in flight, and everything the loading view says about it.
struct Loading {
    /// Where it is going. The address bar shows this while it runs, so the
    /// window says where it is headed before it gets there.
    target: String,
    /// The resource on the wire right now, which is the document itself first
    /// and then each stylesheet and image it turns out to refer to.
    current: String,
    /// How many fetches this load has started.
    fetches: usize,
    started: Instant,
    /// Whether the page it produces pushes the current one onto the history. A
    /// reload replaces instead.
    push: bool,
    /// Where to leave the page when it arrives. A reload keeps its place; a
    /// navigation starts at the top.
    scroll: u32,
    /// The place in the page the address named, which is where it opens once
    /// the layout knows where that is.
    fragment: Option<String>,
}

pub struct Browser {
    window: Window,
    document: Document,
    url: String,
    layout: Layout,
    scroll: u32,
    toolbar_h: u32,
    mods: Modifiers,
    history: Vec<Entry>,
    /// Pages gone back from, so forward is the exact undo of back.
    forward: Vec<Entry>,
    /// Why the last navigation failed, shown in place of the address. A dead
    /// link leaves the page that carried it on screen.
    status: Option<String>,
    loader: Loader,
    /// The load in flight. While there is one the page is not drawn at all;
    /// the loading view stands in its place.
    loading: Option<Loading>,
    address: TextInput,
    /// Whether pages are laid out as their `<main>` alone. It belongs to the
    /// window rather than to a page, since it is how the reader wants to read
    /// and outlives whatever is on screen.
    reader: bool,
}

impl Browser {
    /// A window showing `document`, which for the first load is the empty page
    /// the loading view covers.
    pub fn open(document: Document, url: String, reader: bool) -> Result<Browser, i64> {
        let window = Window::new(80, 60, WIN_W, WIN_H)?;
        let toolbar_h = CONTROL_HEIGHT + space(2) * 2 + 1;
        let layout = Layout::build(&document, WIN_W);
        let mut address = TextInput::with_placeholder(
            0,
            address_x(),
            space(2) as i32,
            address_w(WIN_W),
            "address",
        );
        address.set_text(&url);
        let mut browser = Browser {
            window,
            document,
            url,
            layout,
            scroll: 0,
            toolbar_h,
            mods: Modifiers::default(),
            history: Vec::new(),
            forward: Vec::new(),
            status: None,
            loader: Loader::new(),
            loading: None,
            address,
            reader,
        };
        browser.window.set_title(&browser.window_title())?;
        browser.window.show()?;
        Ok(browser)
    }

    fn window_title(&self) -> String {
        if let Some(loading) = &self.loading {
            return format!("Loading {} - edos-web", loading.target);
        }
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
                for event in events.iter().take(count) {
                    if !self.handle(*event) {
                        return;
                    }
                }
            }
            self.pump();
            self.draw();
            std::thread::sleep(Duration::from_millis(16));
        }
    }

    /// Take whatever the loader has to say since the last frame.
    fn pump(&mut self) {
        while let Some(update) = self.loader.poll() {
            match update {
                Update::Fetching(url) => {
                    if let Some(loading) = &mut self.loading {
                        loading.current = url;
                        loading.fetches += 1;
                    }
                }
                Update::Loaded(page) => self.install(*page),
                Update::Failed(message) => {
                    let target = self
                        .loading
                        .take()
                        .map(|loading| loading.target)
                        .unwrap_or_default();
                    println!("edos-web: {} - {}", target, message);
                    self.status = Some(message);
                    self.address.set_text(&self.url);
                    let _ = self.window.set_title(&self.window_title());
                }
            }
        }
    }

    /// Put a loaded page on screen in place of the loading view.
    fn install(&mut self, page: Page) {
        let Some(loading) = self.loading.take() else {
            return;
        };
        // Said on stdout as well as drawn, so a headless run can see which
        // page a click actually reached.
        println!(
            "edos-web: -> {} - {} blocks in a tree of {} boxes {} deep, from {}",
            page.document.display_title(),
            page.document.blocks.len(),
            page.document.root.count(),
            page.document.root.depth(),
            page.address
        );
        let previous = Entry {
            document: mem::replace(&mut self.document, page.document),
            url: mem::replace(&mut self.url, page.address),
            scroll: self.scroll,
        };
        if loading.push {
            self.history.push(previous);
            // Going somewhere new is what discards the pages forward led to.
            self.forward.clear();
        }
        self.status = None;
        self.layout = Layout::build(&self.document, self.window.width);
        self.scroll = loading.scroll.min(self.max_scroll());
        if let Some(fragment) = &loading.fragment {
            self.scroll_to(fragment);
        }
        self.address.set_text(&self.url);
        let _ = self.window.set_title(&self.window_title());
    }

    /// Returns false when the window should close.
    fn handle(&mut self, event: WindowEvent) -> bool {
        match event.event_type() {
            Some(WindowEventType::CloseRequested) => return false,
            Some(WindowEventType::Resize) => {
                let (width, height) = (event.x as u32, event.y as u32);
                if self.window.resize(width, height).is_ok() {
                    self.address.set_width(address_w(width));
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
                if self.loading.is_some() {
                    return true;
                }
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
            Some(WindowEventType::KeyPress) => return self.on_key(event),
            _ => {}
        }
        true
    }

    /// Returns false when the window should close.
    fn on_key(&mut self, event: WindowEvent) -> bool {
        // A modifier press must not return before the field sees it, or a
        // chord's release arrives without its press.
        if update_modifiers(&mut self.mods, event.code, true) {
            return true;
        }
        // Alt marks a chord as the window manager's.
        if self.mods.alt {
            return true;
        }
        if self.mods.ctrl {
            match map_keycode(event.code, &Modifiers::default()) {
                Some('l') => self.focus_address(),
                Some('r') => self.reload(),
                _ => {}
            }
            return true;
        }
        if self.address.focused() {
            return self.on_address_key(event);
        }
        if event.code == keycode::ESCAPE {
            // While a page is on its way, Escape is the stop button.
            if self.loading.is_some() {
                self.stop();
                return true;
            }
            return false;
        }
        let page = self.viewport_h().saturating_sub(SCROLL_STEP) as i32;
        // The kernel routes scancodes, so a letter key is what the layout says
        // it is, not what the scancode is called.
        if map_keycode(event.code, &self.mods) == Some('q') {
            return false;
        }
        match event.code {
            keycode::F5 => self.reload(),
            keycode::M => self.toggle_reader(),
            // Shift+Backspace is forward: Alt+Left and Alt+Right, which a
            // browser normally uses, belong to the window manager here.
            keycode::BACKSPACE if self.mods.shift => self.go_forward(),
            keycode::BACKSPACE => self.back(),
            keycode::ARROW_DOWN => self.scroll_by(SCROLL_STEP as i32),
            keycode::ARROW_UP => self.scroll_by(-(SCROLL_STEP as i32)),
            keycode::PAGE_DOWN | keycode::SPACEBAR => self.scroll_by(page),
            keycode::PAGE_UP => self.scroll_by(-page),
            keycode::HOME => self.scroll = 0,
            keycode::END => self.scroll = self.max_scroll(),
            _ => {}
        }
        true
    }

    /// A key pressed while the address bar has the focus. It all belongs to the
    /// field: a page shortcut firing while an address is half-typed is the
    /// oldest bug in text entry.
    fn on_address_key(&mut self, event: WindowEvent) -> bool {
        if event.code == keycode::ESCAPE {
            // Abandon the edit, and say what is actually on screen again.
            let address = self.current_address().to_string();
            self.address.set_text(&address);
            self.address.set_focused(false);
            return true;
        }
        if let Some(WidgetEvent::Submit(target)) = self.address.on_key(event.code, true) {
            self.address.set_focused(false);
            let target = target.trim().to_string();
            if !target.is_empty() {
                self.navigate(&target);
            }
            return true;
        }
        // Everything the field reads as an edit or a cursor move is spent.
        if matches!(
            event.code,
            keycode::BACKSPACE
                | keycode::DELETE
                | keycode::ARROW_LEFT
                | keycode::ARROW_RIGHT
                | keycode::HOME
                | keycode::END
                | keycode::RETURN
                | keycode::NUMPAD_ENTER
        ) {
            return true;
        }
        if let Some(ch) = map_keycode(event.code, &self.mods) {
            self.address.on_char(ch);
        }
        true
    }

    /// What the address bar should say: where a load is going while one is in
    /// flight, and where the page came from otherwise.
    fn current_address(&self) -> &str {
        match &self.loading {
            Some(loading) => &loading.target,
            None => &self.url,
        }
    }

    /// Put the caret in the address bar, empty.
    ///
    /// A browser selects the whole address instead, so that typing replaces it
    /// and an arrow key keeps it; this field has no selection, and of the two
    /// behaviours it can express, replacing is the one the shortcut is
    /// reached for. Clicking into the bar keeps the address, which is what
    /// editing one asks for.
    fn focus_address(&mut self) {
        self.address.set_text("");
        self.address.set_focused(true);
    }

    /// A click in the toolbar works its buttons or lands in the address bar;
    /// one in the page follows whatever link is under it.
    fn on_click(&mut self, x: i32, y: i32) {
        if y < self.toolbar_h as i32 {
            for (index, button) in BUTTONS.iter().enumerate() {
                if x >= button_x(index) && x < button_x(index) + CONTROL_HEIGHT as i32 {
                    self.press(*button);
                    return;
                }
            }
            if x >= address_x() {
                self.address.set_focused(true);
                self.address.on_mouse_button(x, y, true);
            }
            return;
        }
        // A click in the page is a click out of the address bar.
        self.address.set_focused(false);
        if self.loading.is_some() {
            return;
        }
        let page_y = y - self.chrome_h() as i32 + self.scroll as i32;
        let Some(target) = self.layout.link_at(x, page_y).map(str::to_string) else {
            return;
        };
        self.navigate(&target);
    }

    fn press(&mut self, button: Button) {
        match button {
            Button::Back => self.back(),
            Button::Forward => self.go_forward(),
            Button::Reload if self.loading.is_some() => self.stop(),
            Button::Reload => self.reload(),
            Button::Reader => self.toggle_reader(),
        }
    }

    /// Whether a button would do anything, which is what draws it enabled.
    fn armed(&self, button: Button) -> bool {
        match button {
            Button::Back => !self.history.is_empty(),
            Button::Forward => !self.forward.is_empty(),
            Button::Reload => true,
            // Lit when it is showing the whole document, which is the state
            // that differs from what this browser does by default.
            Button::Reader => !self.reader && self.document.has_main(),
        }
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

    /// The window as a media query sees it: the page's own area, so a query the
    /// page writes about the viewport is answered with the space it gets.
    fn viewport(&self) -> Viewport {
        Viewport::new(
            self.window.width,
            self.window.height.saturating_sub(self.chrome_h()),
            crate::doc::ROOT_PX,
        )
    }

    /// The strip a failure takes under the toolbar, which is nothing at all
    /// when there is none. It is its own row rather than the address bar's
    /// place, because that bar has to keep holding an address the reader can
    /// edit and retry.
    fn notice_h(&self) -> u32 {
        match self.status {
            Some(_) => text::line_height(notice_style()) + space(1) * 2,
            None => 0,
        }
    }

    /// Everything above the page.
    fn chrome_h(&self) -> u32 {
        self.toolbar_h + self.notice_h()
    }

    /// Start loading `target`. The page on screen gives way to the loading view
    /// at once; what arrives replaces it, and what fails leaves it standing.
    ///
    /// A target naming a place in the page already on screen is neither: it is
    /// a scroll. Every heading link and every table of contents on a
    /// documentation site is one of those, and fetching the page again to land
    /// back at its top is the wrong answer twice over.
    pub fn navigate(&mut self, target: &str) {
        let (page, fragment) = split_target(target);
        // Only a fragment short-circuits. A link to the page's own address
        // carrying no fragment is a reload, which is what a browser does with
        // one and what the page that wrote it meant.
        if let Some(fragment) = fragment
            && self.loading.is_none()
            && (page.is_empty() || page == self.url)
        {
            self.scroll_to(fragment);
            return;
        }
        self.begin(page, true, 0, fragment.map(str::to_string));
    }

    /// Put the place `fragment` names at the top of the window.
    fn scroll_to(&mut self, fragment: &str) {
        let Some(y) = self.layout.anchors.get(fragment).copied() else {
            // A fragment naming nothing is not a failure to report: it is a
            // page that moved its anchors, and the page it points at is the
            // one already on screen.
            println!("edos-web: # {} - no such place in this page", fragment);
            return;
        };
        self.scroll = y.min(self.max_scroll());
    }

    /// Switch between the page's main content and the whole document, and
    /// rebuild what is on screen in the new mode.
    ///
    /// Nothing is fetched: the document keeps its bytes and its subresources,
    /// so this is a parse and a layout.
    fn toggle_reader(&mut self) {
        self.reader = !self.reader;
        if self.loading.is_some() {
            return;
        }
        self.document = self
            .document
            .set_reader(self.reader, &crate::fetch_subresource);
        self.layout = Layout::build(&self.document, self.window.width);
        self.scroll = self.scroll.min(self.max_scroll());
        println!(
            "edos-web: {} - {} blocks",
            if self.reader {
                "main only"
            } else {
                "whole page"
            },
            self.document.blocks.len()
        );
    }

    fn reload(&mut self) {
        // Reloading keeps the reader where they were: the page is expected to
        // come back the same, which is the whole reason to ask for it again.
        let (url, scroll) = (self.url.clone(), self.scroll);
        self.begin(&url, false, scroll, None);
    }

    fn begin(&mut self, target: &str, push: bool, scroll: u32, fragment: Option<String>) {
        self.status = None;
        self.loader.start(target, self.viewport(), self.reader);
        self.loading = Some(Loading {
            target: target.to_string(),
            current: target.to_string(),
            fetches: 0,
            started: Instant::now(),
            push,
            scroll,
            fragment,
        });
        self.address.set_focused(false);
        self.address.set_text(target);
        let _ = self.window.set_title(&self.window_title());
    }

    /// Abandon the load in flight, leaving the page that was on screen.
    fn stop(&mut self) {
        let Some(loading) = self.loading.take() else {
            return;
        };
        self.loader.stop();
        println!("edos-web: x {}", loading.target);
        self.status = Some(format!("stopped loading {}", loading.target));
        self.address.set_text(&self.url);
        let _ = self.window.set_title(&self.window_title());
    }

    fn back(&mut self) {
        let Some(entry) = self.history.pop() else {
            return;
        };
        println!("edos-web: <- {}", entry.url);
        let leaving = self.leave();
        self.forward.push(leaving);
        self.arrive(entry);
    }

    fn go_forward(&mut self) {
        let Some(entry) = self.forward.pop() else {
            return;
        };
        println!("edos-web: -> {}", entry.url);
        let leaving = self.leave();
        self.history.push(leaving);
        self.arrive(entry);
    }

    /// The page on screen, as a history entry.
    fn leave(&mut self) -> Entry {
        Entry {
            document: mem::take(&mut self.document),
            url: mem::take(&mut self.url),
            scroll: self.scroll,
        }
    }

    /// Show a page the history already holds. Nothing is fetched, so there is
    /// no loading view: the document is the one that was parsed on the way
    /// through.
    fn arrive(&mut self, entry: Entry) {
        // A load in flight is not what the reader asked for any more.
        if self.loading.take().is_some() {
            self.loader.stop();
        }
        self.document = entry.document;
        self.url = entry.url;
        self.status = None;
        self.layout = Layout::build(&self.document, self.window.width);
        self.scroll = entry.scroll.min(self.max_scroll());
        self.address.set_text(&self.url);
        let _ = self.window.set_title(&self.window_title());
    }

    fn viewport_h(&self) -> u32 {
        self.window.height.saturating_sub(self.chrome_h())
    }

    fn max_scroll(&self) -> u32 {
        self.layout.height.saturating_sub(self.viewport_h())
    }

    fn scroll_by(&mut self, delta: i32) {
        let next = self.scroll as i64 + delta as i64;
        self.scroll = next.clamp(0, self.max_scroll() as i64) as u32;
    }

    fn draw(&mut self) {
        let (width, height, toolbar_h) = (self.window.width, self.window.height, self.toolbar_h);
        let top = self.chrome_h();
        let chrome = Chrome {
            status: self.status.clone(),
            armed: BUTTONS.map(|button| self.armed(button)),
            stopping: self.loading.is_some(),
        };
        self.window.fill(Theme::DEFAULT.background.raw());

        let layout = &self.layout;
        let scroll = self.scroll;
        let loading = self.loading.as_ref();
        let address = &self.address;
        let Some(buffer) = self.window.buffer_mut() else {
            return;
        };

        match loading {
            Some(loading) => loading_view(buffer, width, height, top, loading),
            None => view::draw(layout, buffer, width, height, top, scroll),
        }

        // The toolbar is drawn last so a page scrolled under it is covered.
        toolbar(buffer, width, height, toolbar_h, top, &chrome);
        address.draw(buffer, width, height);
        self.window.swap_buffers();
    }
}

/// What the toolbar shows about the page currently on screen.
struct Chrome {
    /// A message standing in for the address, which the address bar itself
    /// cannot show because it holds what the reader may be typing.
    status: Option<String>,
    /// Which buttons would do something, in [`BUTTONS`] order.
    armed: [bool; BUTTON_COUNT],
    /// The reload button is a stop button while a load is in flight.
    stopping: bool,
}

/// Split a target into the page to fetch and the place in it to go.
///
/// Textual rather than through `Url`, because a target typed into the address
/// bar is not a URL yet -- `edos.edgl.dev` becomes `https://edos.edgl.dev`
/// only once `crate::load` reads it, and parsing here would default it to
/// `http` behind the reader's back. A `#` cannot appear unescaped anywhere
/// else, so splitting on the first one is exact.
fn split_target(target: &str) -> (&str, Option<&str>) {
    match target.split_once('#') {
        Some((page, fragment)) if !fragment.is_empty() => (page, Some(fragment)),
        Some((page, _)) => (page, None),
        None => (target, None),
    }
}

/// The left edge of toolbar button `index`.
fn button_x(index: usize) -> i32 {
    (PAGE_PAD + index as u32 * (CONTROL_HEIGHT + space(1))) as i32
}

/// The left edge of the address bar, which is the whole toolbar past the
/// buttons.
fn address_x() -> i32 {
    button_x(BUTTONS.len()) + space(2) as i32
}

fn address_w(width: u32) -> u32 {
    width
        .saturating_sub(address_x() as u32 + PAGE_PAD)
        .max(space(10))
}

/// The style a failure is said in.
fn notice_style() -> Style {
    Style::new(Theme::DEFAULT.warning.raw()).with_px(edos_render::font::size::CAPTION)
}

/// The strip carrying the buttons and the address bar, and under it the row a
/// failure takes for as long as it stands.
fn toolbar(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    toolbar_h: u32,
    bottom: u32,
    chrome: &Chrome,
) {
    for y in 0..toolbar_h.min(height) {
        let color = if y + 1 == toolbar_h {
            Theme::DEFAULT.window_border_highlight.raw()
        } else {
            Theme::DEFAULT.title_inactive_top.raw()
        };
        let row = y as usize * width as usize;
        buffer[row..row + width as usize].fill(color);
    }

    let y = space(2) as i32;
    for (index, button) in BUTTONS.iter().enumerate() {
        let armed = chrome.armed[index];
        let color = if armed {
            Theme::DEFAULT.title_text.raw()
        } else {
            Theme::DEFAULT.text_disabled.raw()
        };
        let mask = match button {
            Button::Back => &icons::CHEVRON_LEFT,
            Button::Forward => &icons::CHEVRON_RIGHT,
            Button::Reload if chrome.stopping => &icons::STOP,
            Button::Reload => &icons::RELOAD,
            Button::Reader => &icons::DOCUMENT,
        };
        // The icon is centred in a slot the size of the address bar's height,
        // so the hit test is the slot and the drawing cannot disagree with it.
        let inset = (CONTROL_HEIGHT - icons::SIZE as u32) / 2;
        icons::draw(
            buffer,
            width,
            height,
            button_x(index) + inset as i32,
            y + inset as i32,
            mask,
            color,
        );
    }

    if let Some(status) = &chrome.status {
        draw_rect(
            buffer,
            width,
            height,
            0,
            toolbar_h as i32,
            width,
            bottom.saturating_sub(toolbar_h),
            Theme::DEFAULT.title_inactive_bottom.raw(),
        );
        let mut surface = Surface::new(buffer, width, height);
        surface.clip = Some((0, toolbar_h as i32, width as i32, bottom as i32));
        text::draw(
            &mut surface,
            PAGE_PAD as i32,
            (toolbar_h + space(1)) as i32,
            status,
            notice_style(),
        );
    }
}

/// The page's area while a load is in flight.
///
/// It replaces the page rather than covering part of it: the reader asked for
/// somewhere else, and a browser that leaves the old page up through a ten
/// second fetch is one that looks like it ignored the click.
fn loading_view(buffer: &mut [u32], width: u32, height: u32, top: u32, loading: &Loading) {
    let elapsed = loading.started.elapsed();
    let middle = top as i32 + (height.saturating_sub(top) / 2) as i32;

    let title = Style::new(Theme::DEFAULT.text_primary.raw());
    let caption =
        Style::new(Theme::DEFAULT.label_text.raw()).with_px(edos_render::font::size::CAPTION);

    let track_w = (width * 3 / 5)
        .max(space(20))
        .min(width.saturating_sub(space(8)));
    let track_x = ((width - track_w) / 2) as i32;
    let track_y = middle;
    let track_h = space(1).max(2);

    let mut surface = Surface::new(buffer, width, height);
    let heading = "Loading";
    let heading_x = ((width - text::width(heading, title).min(width)) / 2) as i32;
    text::draw(
        &mut surface,
        heading_x,
        track_y - (text::line_height(title) + space(3)) as i32,
        heading,
        title,
    );

    draw_rect(
        buffer,
        width,
        height,
        track_x,
        track_y,
        track_w,
        track_h,
        Theme::DEFAULT.input_bg.raw(),
    );
    // Indeterminate, because nothing knows how many resources a page refers to
    // until it has been parsed, and parsing needs the document that is still
    // on its way. A band that moves is an honest "something is happening";
    // a bar that fills would be a number this cannot know.
    let band = track_w / 4;
    let span = track_w - band;
    let step = (elapsed.as_millis() as u32 / 4) % (span * 2).max(1);
    let offset = if step < span { step } else { span * 2 - step };
    draw_rect(
        buffer,
        width,
        height,
        track_x + offset as i32,
        track_y,
        band,
        track_h,
        Theme::DEFAULT.title_accent.raw(),
    );

    let mut surface = Surface::new(buffer, width, height);
    let lines = [
        elide(&loading.current, track_w, caption),
        format!(
            "{} fetched - {:.1}s - Esc to stop",
            loading.fetches.saturating_sub(1),
            elapsed.as_secs_f32()
        ),
    ];
    for (index, line) in lines.iter().enumerate() {
        let x = ((width - text::width(line, caption).min(width)) / 2) as i32;
        let y = track_y
            + (track_h + space(2)) as i32
            + index as i32 * text::line_height(caption) as i32;
        text::draw(&mut surface, x, y, line, caption);
    }
}

/// `text` cut to fit `width`, measured rather than counted because the face is
/// proportional.
fn elide(text: &str, width: u32, style: Style) -> String {
    if text::width(text, style) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    for ch in text.chars() {
        let mut probe = out.clone();
        probe.push(ch);
        probe.push_str("...");
        if text::width(&probe, style) > width {
            break;
        }
        out.push(ch);
    }
    out.push_str("...");
    out
}
