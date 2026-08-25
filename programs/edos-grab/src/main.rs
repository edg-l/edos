//! edos-grab: the graphical face of the package manager.
//!
//! A search field, a scrolling list of what the repository publishes, a detail
//! pane for whatever is selected, and Install/Remove/Update with a progress
//! line along the foot.
//!
//! It links the `grab` library rather than driving the command-line tool, so a
//! failure arrives as an `Error` it can colour and a download reports bytes as
//! they land. Everything that touches the network runs on a worker thread and
//! reports back over a channel: the window keeps drawing while a package is
//! being fetched.

mod view;

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;
use std::time::Duration;

use edos_lib::keymap::{Modifiers, keycode, update_modifiers};
use edos_render::image::Svg;
use edos_render::theme::Theme;
use edos_render::widgets::{Rect, TextInput, WidgetContainer, WidgetEvent, WidgetId};
use edos_render::window::{Window, WindowEvent, WindowEventType};
use grab::Progress;
use grab_index::Package;

use edos_render::widgets::Canvas;
use view::Layout;

/// Opening size.
const WIN_W: u32 = 920;
const WIN_H: u32 = 620;

/// Rows moved per notch of the wheel.
const SCROLL_STEP: usize = 3;

/// An icon is small; anything larger than this from the repository is already
/// wrong, and refusing it costs nothing.
const MAX_ICON_BYTES: u64 = 256 * 1024;

/// What a worker thread has to say. Every field the window draws that comes
/// from the network arrives as one of these.
enum Update {
    /// A line for the progress strip.
    Message(String),
    /// Bytes of a download so far, and the total when it is known.
    Transfer(u64, Option<u64>),
    /// The catalogue, and what this machine has installed of it.
    Catalogue {
        packages: Vec<Package>,
        installed: Vec<(String, String)>,
    },
    /// A rendered package icon.
    Icon {
        name: String,
        width: u32,
        height: u32,
        pixels: Vec<u32>,
    },
    /// The operation ended, with the last word on how it went.
    Done { text: String, failed: bool },
}

/// A [`Progress`] sink that forwards to the window.
struct Reporter(Sender<Update>);

impl Progress for Reporter {
    fn message(&mut self, text: &str) {
        let _ = self.0.send(Update::Message(text.to_string()));
    }

    fn transfer(&mut self, done: u64, total: Option<u64>) {
        let _ = self.0.send(Update::Transfer(done, total));
    }
}

/// Send the catalogue and the installed set as one message, so the list and
/// the "installed" marks can never disagree on screen.
fn send_catalogue(tx: &Sender<Update>, packages: Vec<Package>) {
    let installed = grab::db::installed()
        .unwrap_or_default()
        .into_iter()
        .map(|record| (record.name, record.version))
        .collect();
    let _ = tx.send(Update::Catalogue {
        packages,
        installed,
    });
}

/// Fetch and rasterize every icon the catalogue names.
///
/// The repository serves icons separately from packages, which is what lets
/// the list draw itself without downloading anything it is listing.
fn fetch_icons(tx: &Sender<Update>, base: &str, packages: &[Package]) {
    let opts = edos_http::Options {
        max_body: MAX_ICON_BYTES,
        ..edos_http::Options::default()
    };

    for package in packages {
        let Some(icon) = &package.icon else { continue };
        let Ok(response) = edos_http::get(&format!("{}/{}", base, icon), &opts) else {
            continue;
        };
        if !response.head.is_success() {
            continue;
        }
        let Ok(svg) = Svg::parse(&response.body) else {
            continue;
        };
        let (width, height) = svg.fit_size(view::ICON, view::ICON);
        let Ok(image) = svg.render(width, height, Theme::DEFAULT.background) else {
            continue;
        };
        let _ = tx.send(Update::Icon {
            name: package.name.clone(),
            width: image.width,
            height: image.height,
            pixels: image.pixels,
        });
    }
}

/// What a worker thread was asked to do.
enum Job {
    /// Read the catalogue, fetching one only if none is cached.
    Load,
    /// Fetch the catalogue afresh.
    Refresh,
    Install(String),
    Remove(String),
}

fn run_job(tx: Sender<Update>, job: Job) {
    let mut report = Reporter(tx.clone());
    let base = grab::repo_url();

    let outcome = match job {
        Job::Load => grab::index(&mut report).map(|index| (index, "ready".to_string())),
        Job::Refresh => grab::update(&mut report).map(|index| {
            let note = format!("{} package(s) in the repository", index.packages.len());
            (index, note)
        }),
        Job::Install(name) => grab::install(&name, &mut report)
            .and_then(|()| grab::index(&mut report))
            .map(|index| (index, format!("{} installed", name))),
        Job::Remove(name) => grab::remove(&name, &mut report)
            .and_then(|()| grab::index(&mut report))
            .map(|index| (index, format!("{} removed", name))),
    };

    match outcome {
        Ok((index, text)) => {
            send_catalogue(&tx, index.packages.clone());
            let _ = tx.send(Update::Done {
                text,
                failed: false,
            });
            fetch_icons(&tx, &base, &index.packages);
        }
        Err(err) => {
            send_catalogue(&tx, Vec::new());
            let _ = tx.send(Update::Done {
                text: err.to_string(),
                failed: true,
            });
        }
    }
}

/// A rasterized icon, kept at the size the list draws it.
struct Icon {
    width: u32,
    height: u32,
    pixels: Vec<u32>,
}

struct App {
    window: Window,
    widgets: WidgetContainer,
    search: WidgetId,
    /// What the search field holds. The field owns the text; this is the copy
    /// the filter reads, kept current from the field's own `TextChanged`.
    query: String,
    /// Everything the repository publishes, in the order the index lists it.
    packages: Vec<Package>,
    /// Installed name to installed version.
    installed: HashMap<String, String>,
    icons: HashMap<String, Icon>,
    /// Indices into `packages` that match the search field, recomputed
    /// whenever either changes rather than kept in step by hand.
    matches: Vec<usize>,
    /// Index into `matches` of the selected row.
    selected: Option<usize>,
    /// First row of `matches` on screen.
    scroll: usize,
    hover: Option<usize>,
    hover_button: Option<Button>,
    status: String,
    status_failed: bool,
    /// Whether a worker thread is running: one operation at a time, since two
    /// installs at once would race over the same database.
    busy: bool,
    mods: Modifiers,
    tx: Sender<Update>,
    rx: Receiver<Update>,
}

/// The three buttons that are drawn rather than being widgets, because their
/// rectangles come from the same [`Layout`] the list rows do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Button {
    Refresh,
    Install,
    Remove,
}

impl App {
    fn new(window: Window) -> Self {
        let (tx, rx) = channel();
        let layout = Layout::new(window.width, window.height);

        let mut widgets = WidgetContainer::new();
        let search = widgets.add(TextInput::with_placeholder(
            layout.search.x,
            layout.search.y,
            layout.search.width,
            "Search packages",
        ));
        widgets.set_focus(search);

        Self {
            window,
            widgets,
            search,
            query: String::new(),
            packages: Vec::new(),
            installed: HashMap::new(),
            icons: HashMap::new(),
            matches: Vec::new(),
            selected: None,
            scroll: 0,
            hover: None,
            hover_button: None,
            status: "reading the catalogue".to_string(),
            status_failed: false,
            busy: false,
            mods: Modifiers::default(),
            tx,
            rx,
        }
    }

    fn layout(&self) -> Layout {
        Layout::new(self.window.width, self.window.height)
    }

    /// The package under the selection, if there is one.
    fn selection(&self) -> Option<&Package> {
        let index = *self.matches.get(self.selected?)?;
        self.packages.get(index)
    }

    /// Recompute which packages the search field admits, keeping the selected
    /// package selected when it survives the filter.
    fn refilter(&mut self) {
        let needle = self.query.to_lowercase();
        let previous = self.selection().map(|package| package.name.clone());

        self.matches = self
            .packages
            .iter()
            .enumerate()
            .filter(|(_, package)| {
                needle.is_empty()
                    || package.name.to_lowercase().contains(&needle)
                    || package.summary.to_lowercase().contains(&needle)
                    || package.category.to_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect();

        self.selected = previous.and_then(|name| {
            self.matches
                .iter()
                .position(|&index| self.packages[index].name == name)
        });
        if self.selected.is_none() && !self.matches.is_empty() {
            self.selected = Some(0);
        }
        self.clamp_scroll();
    }

    fn clamp_scroll(&mut self) {
        let rows = self.layout().visible_rows().max(1);
        let last = self.matches.len().saturating_sub(rows);
        self.scroll = self.scroll.min(last);
        if let Some(selected) = self.selected {
            if selected < self.scroll {
                self.scroll = selected;
            } else if selected >= self.scroll + rows {
                self.scroll = selected + 1 - rows;
            }
        }
    }

    /// Start a worker, unless one is already running.
    fn start(&mut self, job: Job) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status_failed = false;
        let tx = self.tx.clone();
        thread::spawn(move || run_job(tx, job));
    }

    /// Drain whatever the worker has said since the last frame.
    fn pump(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(Update::Message(text)) => self.status = text,
                Ok(Update::Transfer(done, total)) => {
                    self.status = match total {
                        Some(total) if total > 0 => format!(
                            "{} / {} bytes ({}%)",
                            done,
                            total,
                            done.saturating_mul(100) / total
                        ),
                        _ => format!("{} bytes", done),
                    };
                }
                Ok(Update::Catalogue {
                    packages,
                    installed,
                }) => {
                    // An empty catalogue is what a failed fetch reports, and
                    // it must not throw away icons already rasterized: the
                    // next successful refresh names the same packages.
                    if !packages.is_empty() {
                        self.packages = packages;
                    }
                    self.installed = installed.into_iter().collect();
                    self.refilter();
                }
                Ok(Update::Icon {
                    name,
                    width,
                    height,
                    pixels,
                }) => {
                    self.icons.insert(
                        name,
                        Icon {
                            width,
                            height,
                            pixels,
                        },
                    );
                }
                Ok(Update::Done { text, failed }) => {
                    self.status = text;
                    self.status_failed = failed;
                    self.busy = false;
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return,
            }
        }
    }

    /// Whether Install and Remove can act on the selection right now.
    fn actions(&self) -> (bool, bool) {
        let Some(package) = self.selection() else {
            return (false, false);
        };
        let installed = self.installed.get(&package.name);
        let can_install = !self.busy && installed != Some(&package.version);
        let can_remove = !self.busy && installed.is_some();
        (can_install, can_remove)
    }

    fn button_at(&self, layout: &Layout, x: i32, y: i32) -> Option<Button> {
        if layout.refresh.contains(x, y) {
            Some(Button::Refresh)
        } else if layout.install.contains(x, y) {
            Some(Button::Install)
        } else if layout.remove.contains(x, y) {
            Some(Button::Remove)
        } else {
            None
        }
    }

    fn press(&mut self, button: Button) {
        let (can_install, can_remove) = self.actions();
        let name = self.selection().map(|package| package.name.clone());
        match button {
            Button::Refresh if !self.busy => self.start(Job::Refresh),
            Button::Install if can_install => {
                if let Some(name) = name {
                    self.start(Job::Install(name));
                }
            }
            Button::Remove if can_remove => {
                if let Some(name) = name {
                    self.start(Job::Remove(name));
                }
            }
            _ => {}
        }
    }

    /// Move the selection by `delta` rows.
    fn move_selection(&mut self, delta: isize) {
        if self.matches.is_empty() {
            return;
        }
        let last = self.matches.len() - 1;
        let next = match self.selected {
            Some(current) => (current as isize + delta).clamp(0, last as isize) as usize,
            None => 0,
        };
        self.selected = Some(next);
        self.clamp_scroll();
    }

    /// Returns false when the window should close.
    fn handle(&mut self, event: &WindowEvent) -> bool {
        let layout = self.layout();

        match event.event_type() {
            Some(WindowEventType::CloseRequested) => return false,
            Some(WindowEventType::Resize) => {
                let width = event.x.max(1) as u32;
                let height = event.y.max(1) as u32;
                let _ = self.window.resize(width, height);
                let layout = self.layout();
                if let Some(field) = self.widgets.get_mut(self.search) {
                    field.set_position(layout.search.x, layout.search.y);
                }
                self.clamp_scroll();
            }
            Some(WindowEventType::MouseMove) => {
                self.hover = layout
                    .row_at(event.x, event.y)
                    .and_then(|slot| (self.scroll + slot < self.matches.len()).then_some(slot));
                self.hover_button = self.button_at(&layout, event.x, event.y);
            }
            Some(WindowEventType::MouseButton) if event.data != 0 => {
                if let Some(button) = self.button_at(&layout, event.x, event.y) {
                    self.press(button);
                } else if let Some(slot) = layout.row_at(event.x, event.y) {
                    let row = self.scroll + slot;
                    if row < self.matches.len() {
                        self.selected = Some(row);
                    }
                }
            }
            Some(WindowEventType::MouseScroll) => {
                let delta = event.data as i32;
                if delta > 0 {
                    self.scroll = self.scroll.saturating_sub(SCROLL_STEP);
                } else if delta < 0 {
                    self.scroll += SCROLL_STEP;
                }
                let rows = layout.visible_rows().max(1);
                self.scroll = self.scroll.min(self.matches.len().saturating_sub(rows));
            }
            Some(WindowEventType::KeyPress) => {
                // A modifier press changes state and produces no key of its
                // own, but it must still reach the container below: returning
                // early here would leave it seeing every Ctrl release and
                // never the matching press.
                //
                // Alt marks a chord as the window manager's. The manager claims
                // the ones it acts on and those never arrive here, but an
                // unclaimed Alt chord still does, and it is not a binding this
                // program made: Alt+PageDown is not a request to page the list.
                // Only these shortcuts are skipped; the container below still
                // sees the event.
                if !update_modifiers(&mut self.mods, event.code, true) && !self.mods.alt {
                    let field_focused = self.widgets.focused() == Some(self.search);
                    match event.code {
                        keycode::F5 if !self.busy => self.start(Job::Refresh),
                        keycode::ARROW_UP if !field_focused => self.move_selection(-1),
                        keycode::ARROW_DOWN if !field_focused => self.move_selection(1),
                        keycode::PAGE_UP => self.move_selection(-(layout.visible_rows() as isize)),
                        keycode::PAGE_DOWN => self.move_selection(layout.visible_rows() as isize),
                        _ => {}
                    }
                }
            }
            // Without this the modifier set is a latch: anything ever pressed
            // stays pressed, and the first Alt chord to arrive leaves every
            // shortcut above dead for the rest of the session.
            Some(WindowEventType::KeyRelease) => {
                update_modifiers(&mut self.mods, event.code, false);
            }
            _ => {}
        }

        for (id, widget_event) in self.widgets.handle_event(event) {
            if id != self.search {
                continue;
            }
            if let WidgetEvent::TextChanged(text) | WidgetEvent::Submit(text) = widget_event {
                self.query = text;
                self.refilter();
            }
        }

        true
    }

    fn draw(&mut self) {
        let layout = self.layout();
        let width = self.window.width;
        let height = self.window.height;
        let rows = layout.visible_rows();
        let (can_install, can_remove) = self.actions();
        let hover_button = self.hover_button;
        let hover_row = self.hover;
        let selected = self.selected;
        let scroll = self.scroll;
        let status = self.status.clone();
        let status_failed = self.status_failed;
        let empty = self.packages.is_empty();

        let visible: Vec<(usize, usize)> = self
            .matches
            .iter()
            .enumerate()
            .skip(scroll)
            .take(rows)
            .map(|(row, &index)| (row, index))
            .collect();

        let detail = self.selection().cloned();
        let detail_installed = detail
            .as_ref()
            .and_then(|package| self.installed.get(&package.name).cloned());

        let Some(buf) = self.window.buffer_mut() else {
            return;
        };
        let mut canvas = Canvas { buf, width, height };
        canvas.fill(
            Rect::new(0, 0, width, height),
            Theme::DEFAULT.background.raw(),
        );

        canvas.fill(layout.list, Theme::DEFAULT.input_bg.raw());
        canvas.outline(layout.list, Theme::DEFAULT.input_border.raw());

        for (row, index) in &visible {
            let package = &self.packages[*index];
            let installed_version = self.installed.get(&package.name);
            let icon = self
                .icons
                .get(&package.name)
                .map(|icon| (icon.width, icon.height, icon.pixels.as_slice()));
            view::draw_row(
                &mut canvas,
                layout.row(row - scroll),
                &package.name,
                &package.version,
                &package.summary,
                installed_version.is_some(),
                icon,
                selected == Some(*row),
                hover_row == Some(row - scroll),
            );
        }

        if empty {
            view::draw_status(
                &mut canvas,
                layout.row(0),
                "Nothing to list yet — Update fetches the catalogue",
                false,
            );
        }

        view::draw_detail(
            &mut canvas,
            layout.detail,
            detail.as_ref(),
            detail_installed.as_deref(),
        );
        view::draw_button(
            &mut canvas,
            layout.refresh,
            "Update",
            !self.busy,
            hover_button == Some(Button::Refresh),
        );
        view::draw_button(
            &mut canvas,
            layout.install,
            "Install",
            can_install,
            hover_button == Some(Button::Install),
        );
        view::draw_button(
            &mut canvas,
            layout.remove,
            "Remove",
            can_remove,
            hover_button == Some(Button::Remove),
        );
        view::draw_status(&mut canvas, layout.status, &status, status_failed);

        self.widgets.draw_all(canvas.buf, width, height);
        self.window.swap_buffers();
    }
}

fn main() {
    let window = match Window::new(120, 60, WIN_W, WIN_H) {
        Ok(window) => window,
        Err(err) => {
            eprintln!("edos-grab: could not open a window: {err:?}");
            std::process::exit(1);
        }
    };

    let mut app = App::new(window);
    let _ = app.window.set_title("Packages");
    if app.window.show().is_err() {
        eprintln!("edos-grab: could not show the window");
        std::process::exit(1);
    }
    app.start(Job::Load);

    let mut events = [WindowEvent::default(); 16];
    loop {
        if let Ok(count) = app.window.poll_events(&mut events) {
            for event in &events[..count] {
                if !app.handle(event) {
                    return;
                }
            }
        }
        app.pump();
        app.draw();
        thread::sleep(Duration::from_millis(16));
    }
}
