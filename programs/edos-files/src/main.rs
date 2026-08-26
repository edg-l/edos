//! Files: a window onto what is actually on the disks.
//!
//! Three panels over one status strip. The rail is the kernel's mount table
//! rather than a list of favourites, so the places you can go are the places
//! that exist; the listing is the hero, and its size column carries a bar
//! measured against the largest file in view; the details pane says what the
//! selected row is in the system's own terms.

mod model;
mod view;

use std::fs;
use std::time::{Duration, Instant};

use edos_lib::keymap::{Modifiers, keycode, update_modifiers};
use edos_lib::mounts::Mount;
use edos_lib::process::spawn;
use edos_render::image::{Image, decode_raster};
use edos_render::metrics::space;
use edos_render::widgets::{Rect, TextInput, WidgetContainer, WidgetEvent, WidgetId};
use edos_render::window::{Window, WindowEvent, WindowEventType};

use edos_render::surface::Surface;
use model::{Entry, Kind, Place, Volume};
use view::{Columns, Layout};

/// Opening size. Wide enough for the details pane, short enough to leave the
/// desktop visible behind it.
const WIN_W: u32 = 900;
const WIN_H: u32 = 580;

/// The picture viewer, which is what an image opens in.
const IMAGE_VIEWER: &str = "/bin/imgview";

/// The editor, which is what text opens in.
const EDITOR: &str = "/bin/edos-edit";

/// Two clicks closer together than this on the same row mean "open it".
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Rows moved per notch of the wheel.
const SCROLL_STEP: usize = 3;

/// Tallest a preview may be drawn in the details pane.
const THUMBNAIL_H: u32 = 132;

/// A file larger than this is not read for a preview. A thumbnail is not worth
/// stalling the window for.
const PREVIEW_LIMIT: u64 = 8 * 1024 * 1024;

/// What the pointer is over.
#[derive(Default, PartialEq, Eq, Clone, Copy)]
struct Hover {
    button: Option<usize>,
    crumb: Option<usize>,
    place: Option<usize>,
    row: Option<usize>,
}

/// The inline rename field, which lives only while a name is being changed.
struct Rename {
    widgets: WidgetContainer,
    field: WidgetId,
    original: String,
}

/// A decoded picture, kept so a redraw at 60Hz does not decode it again.
struct Preview {
    path: String,
    pixels: Vec<u32>,
    width: u32,
    height: u32,
}

/// What the status strip says, when it is not just describing the directory.
enum Note {
    /// Nothing to add: the strip counts what is in view.
    None,
    Message(String),
    Warning(String),
    /// Waiting for the user to confirm destroying a name.
    Confirm {
        name: String,
        is_dir: bool,
    },
}

struct App {
    window: Window,
    layout: Layout,
    cwd: String,
    entries: Vec<Entry>,
    selected: usize,
    scroll: usize,
    /// Every directory visited, and where in it we are. Back and forward walk
    /// this rather than the filesystem.
    history: Vec<String>,
    history_index: usize,
    mounts: Vec<Mount>,
    places: Vec<Place>,
    volume: Option<Volume>,
    note: Note,
    hover: Hover,
    rename: Option<Rename>,
    preview: Option<Preview>,
    last_click: Option<(usize, Instant)>,
    dragging_scroll: bool,
    mods: Modifiers,
}

impl App {
    fn new(window: Window, start: String) -> Self {
        let mounts = edos_lib::mounts::list();
        let places = model::places(&mounts);
        let layout = Layout::new(window.width, window.height);
        let mut app = Self {
            window,
            layout,
            cwd: start.clone(),
            entries: Vec::new(),
            selected: 0,
            scroll: 0,
            history: vec![start],
            history_index: 0,
            mounts,
            places,
            volume: None,
            note: Note::None,
            hover: Hover::default(),
            rename: None,
            preview: None,
            last_click: None,
            dragging_scroll: false,
            mods: Modifiers::default(),
        };
        app.reload();
        app
    }

    // --- Directory state ----------------------------------------------------

    /// Read the current directory in, keeping nothing from the last one.
    fn reload(&mut self) {
        match model::read_dir(&self.cwd) {
            Ok(entries) => {
                self.entries = entries;
                if !matches!(self.note, Note::Message(_)) {
                    self.note = Note::None;
                }
            }
            Err(message) => {
                self.entries = Vec::new();
                self.note = Note::Warning(message);
            }
        }
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        // Landing on the way out describes the folder you just left, so the
        // selection starts on the first thing that is actually in here.
        if self
            .entries
            .get(self.selected)
            .is_some_and(|e| e.kind == Kind::Parent)
            && self.entries.len() > 1
        {
            self.selected = 1;
        }
        self.scroll = 0;
        self.volume = model::volume_of(&self.mounts, &self.cwd);
        let _ = self.window.set_title(&format!("Files: {}", self.cwd));
        self.refresh_preview();
    }

    /// Go to `path`, recording it so back can undo it.
    fn navigate(&mut self, path: String) {
        if path == self.cwd {
            return;
        }
        self.cancel_rename();
        self.note = Note::None;
        self.history.truncate(self.history_index + 1);
        self.history.push(path.clone());
        self.history_index = self.history.len() - 1;
        self.cwd = path;
        self.selected = 0;
        self.reload();
    }

    /// Go somewhere already in the history, without recording the move.
    fn jump(&mut self, index: usize) {
        self.cancel_rename();
        self.history_index = index;
        self.cwd = self.history[index].clone();
        self.selected = 0;
        self.note = Note::None;
        self.reload();
    }

    fn can_go_back(&self) -> bool {
        self.history_index > 0
    }

    fn can_go_forward(&self) -> bool {
        self.history_index + 1 < self.history.len()
    }

    fn can_go_up(&self) -> bool {
        self.cwd != "/"
    }

    fn go_up(&mut self) {
        if self.can_go_up() {
            let leaving = self.cwd.clone();
            self.navigate(model::parent(&self.cwd));
            // Coming out of a directory, the directory you came out of is what
            // you were last looking at, so that is what stays selected.
            self.select_named(leaving.rsplit('/').next().unwrap_or_default());
        }
    }

    /// Put the selection on a name, if the listing holds it.
    fn select_named(&mut self, name: &str) {
        if let Some(index) = self.entries.iter().position(|e| e.name == name) {
            self.selected = index;
            self.scroll_into_view();
            self.refresh_preview();
        }
    }

    // --- Selection ----------------------------------------------------------

    fn selected_entry(&self) -> Option<&Entry> {
        self.entries.get(self.selected)
    }

    fn move_selection(&mut self, delta: i32) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() as i32 - 1;
        self.selected = (self.selected as i32 + delta).clamp(0, last) as usize;
        self.scroll_into_view();
        self.refresh_preview();
    }

    fn scroll_into_view(&mut self) {
        let visible = self.layout.rows_visible.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible {
            self.scroll = self.selected + 1 - visible;
        }
    }

    fn clamp_scroll(&mut self) {
        let visible = self.layout.rows_visible.max(1);
        let max = self.entries.len().saturating_sub(visible);
        self.scroll = self.scroll.min(max);
    }

    /// Open what is selected: enter a directory, hand a picture to the viewer,
    /// and say so when nothing here can open it.
    fn open_selected(&mut self) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        if entry.kind == Kind::Parent {
            self.go_up();
            return;
        }
        let path = model::join(&self.cwd, &entry.name);
        if entry.opens_directory {
            self.navigate(path);
            return;
        }
        if entry.opens_in_viewer() {
            if spawn(IMAGE_VIEWER, &[&path], 0, 1, 2).is_err() {
                self.note = Note::Warning(format!("Couldn't start {IMAGE_VIEWER}."));
            }
            return;
        }
        if entry.opens_in_editor() {
            if spawn(EDITOR, &[&path], 0, 1, 2).is_err() {
                self.note = Note::Warning(format!("Couldn't start {EDITOR}."));
            }
            return;
        }
        self.note = Note::Message(match model::extension(&entry.name) {
            Some(ext) => format!("Nothing opens .{} files yet.", ext.to_lowercase()),
            None => "Nothing opens this kind of file yet.".to_string(),
        });
    }

    /// Decode the selected picture, or drop the one held for a row that is no
    /// longer selected.
    fn refresh_preview(&mut self) {
        let wanted = self
            .selected_entry()
            .filter(|entry| entry.has_thumbnail())
            .filter(|entry| entry.size.is_some_and(|size| size <= PREVIEW_LIMIT))
            .map(|entry| model::join(&self.cwd, &entry.name));

        let Some(path) = wanted else {
            self.preview = None;
            return;
        };
        if self.preview.as_ref().is_some_and(|held| held.path == path) {
            return;
        }

        self.preview = fs::read(&path)
            .ok()
            .and_then(|bytes| decode_raster(&bytes).ok())
            .map(|image| scale_preview(&image, path));
    }

    // --- Acting on names ----------------------------------------------------

    /// Open the selected name for editing.
    ///
    /// `prefill` decides whether the field starts holding the name or merely
    /// showing it: a name somebody chose is edited, and a provisional one this
    /// program invented is replaced, since typing over it is the only reason
    /// the field is open.
    fn begin_rename(&mut self, prefill: bool) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        if entry.kind == Kind::Parent {
            return;
        }
        let original = entry.name.clone();
        let columns = Columns::new(self.layout.list);
        let row = view::row_rect(self.layout.list, self.selected, self.scroll);

        let mut widgets = WidgetContainer::new();
        let mut field = TextInput::new(columns.name_x - space(1) as i32, row.y, columns.name_w);
        if prefill {
            field.set_text(&original);
        } else {
            field.set_placeholder(&original);
        }
        let field = widgets.add(field);
        widgets.set_focus(field);

        self.note = Note::Message(if prefill {
            "Enter renames, Esc keeps the old name.".to_string()
        } else {
            format!("Type a name. Enter accepts it, Esc keeps {original}.")
        });
        self.rename = Some(Rename {
            widgets,
            field,
            original,
        });
    }

    fn cancel_rename(&mut self) {
        if self.rename.take().is_some() {
            self.note = Note::None;
        }
    }

    fn commit_rename(&mut self, name: String) {
        let Some(rename) = self.rename.take() else {
            return;
        };
        let name = name.trim().to_string();
        if name.is_empty() || name == rename.original {
            self.note = Note::None;
            return;
        }
        if name.contains('/') {
            self.note = Note::Warning("A name can't contain a slash.".to_string());
            return;
        }

        let from = model::join(&self.cwd, &rename.original);
        let to = model::join(&self.cwd, &name);
        match fs::rename(&from, &to) {
            Ok(()) => {
                self.note = Note::Message(format!("Renamed to {name}."));
                self.reload();
                self.select_named(&name);
            }
            Err(err) => {
                self.note = Note::Warning(format!("Couldn't rename {}: {err}", rename.original))
            }
        }
    }

    /// Make a folder and open its name for editing, since a folder called
    /// "New folder" is not what anybody wanted.
    fn new_folder(&mut self) {
        let name = self.free_name("New folder");
        match fs::create_dir(model::join(&self.cwd, &name)) {
            Ok(()) => {
                self.reload();
                self.select_named(&name);
                self.begin_rename(false);
            }
            Err(err) => self.note = Note::Warning(format!("Couldn't make a folder: {err}")),
        }
    }

    /// `stem`, or the first numbered variant of it that nothing here is called.
    fn free_name(&self, stem: &str) -> String {
        let taken = |name: &str| self.entries.iter().any(|entry| entry.name == name);
        if !taken(stem) {
            return stem.to_string();
        }
        (2..)
            .map(|n| format!("{stem} {n}"))
            .find(|name| !taken(name))
            .unwrap_or_else(|| stem.to_string())
    }

    fn ask_to_delete(&mut self) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        if entry.kind == Kind::Parent {
            return;
        }
        self.note = Note::Confirm {
            name: entry.name.clone(),
            is_dir: entry.kind == Kind::Dir,
        };
    }

    fn delete_confirmed(&mut self) {
        let Note::Confirm { name, is_dir } = &self.note else {
            return;
        };
        let (name, is_dir) = (name.clone(), *is_dir);
        let path = model::join(&self.cwd, &name);
        let outcome = if is_dir {
            fs::remove_dir(&path)
        } else {
            fs::remove_file(&path)
        };
        self.note = match outcome {
            Ok(()) => Note::Message(format!("Deleted {name}.")),
            Err(err) if is_dir => Note::Warning(format!(
                "Couldn't delete {name}: {err}. A folder has to be empty first."
            )),
            Err(err) => Note::Warning(format!("Couldn't delete {name}: {err}")),
        };
        self.reload();
        self.refresh_preview();
    }

    // --- Events -------------------------------------------------------------

    fn handle(&mut self, event: &WindowEvent) -> bool {
        // Whether a name was already being edited when this event arrived. A
        // key that opens the field must not also be typed into it, and the key
        // that opens it is a key like any other until it has been handled.
        let was_renaming = self.rename.is_some();

        match event.event_type() {
            Some(WindowEventType::CloseRequested) => return false,
            Some(WindowEventType::Resize) => {
                let (width, height) = (event.x as u32, event.y as u32);
                if self.window.resize(width, height).is_ok() {
                    self.layout = Layout::new(width, height);
                    self.cancel_rename();
                    self.clamp_scroll();
                    self.scroll_into_view();
                }
            }
            Some(WindowEventType::MouseMove) => self.on_mouse_move(event.x, event.y),
            Some(WindowEventType::MouseButton) => self.on_mouse_button(event),
            Some(WindowEventType::MouseScroll) => {
                let delta = event.data as i32;
                if delta > 0 {
                    self.scroll = self.scroll.saturating_sub(SCROLL_STEP);
                } else if delta < 0 {
                    self.scroll += SCROLL_STEP;
                    self.clamp_scroll();
                }
            }
            Some(WindowEventType::KeyPress) => {
                if !self.on_key(event) {
                    return false;
                }
            }
            // A modifier changes state and produces nothing of its own, but
            // the release still has to reach the rename field below, whose
            // own tracking would otherwise see every press and no release.
            Some(WindowEventType::KeyRelease) => {
                update_modifiers(&mut self.mods, event.code, false);
            }
            _ => {}
        }

        // A rename field is the only thing that wants raw keys and characters,
        // and it wants them after the shortcuts above have had their say.
        if was_renaming
            && self.rename.is_some()
            && !matches!(event.event_type(), Some(WindowEventType::MouseMove))
        {
            self.feed_rename(event);
        }
        true
    }

    fn feed_rename(&mut self, event: &WindowEvent) {
        let Some(rename) = self.rename.as_mut() else {
            return;
        };
        let mut submitted = None;
        for (id, widget_event) in rename.widgets.handle_event(event) {
            if id == rename.field
                && let WidgetEvent::Submit(text) = widget_event
            {
                submitted = Some(text);
            }
        }
        if let Some(text) = submitted {
            self.commit_rename(text);
        }
    }

    fn on_mouse_move(&mut self, x: i32, y: i32) {
        if self.dragging_scroll {
            self.scroll_to_pointer(y);
            return;
        }

        let crumbs = model::breadcrumbs(&self.cwd);
        let crumb_rects = view::crumb_rects(self.layout.toolbar, &crumbs);
        self.hover = Hover {
            button: view::nav_buttons(self.layout.toolbar)
                .iter()
                .position(|rect| rect.contains(x, y)),
            crumb: crumb_rects
                .iter()
                .position(|rect| rect.width > 0 && rect.contains(x, y)),
            place: view::place_at(self.layout.rail, self.places.len(), x, y),
            row: view::row_at(self.layout.list, self.scroll, y)
                .filter(|index| *index < self.entries.len() && self.layout.list.contains(x, y)),
        };
    }

    fn on_mouse_button(&mut self, event: &WindowEvent) {
        let pressed = event.data != 0;
        let (x, y) = (event.x, event.y);
        if !pressed {
            self.dragging_scroll = false;
            return;
        }
        // Only the left button acts; the others have nothing bound to them yet.
        if event.code != 0 {
            return;
        }

        if let Some(track) = view::scrollbar(
            self.layout.list,
            self.entries.len(),
            self.layout.rows_visible,
        ) && track.contains(x, y)
        {
            self.dragging_scroll = true;
            self.scroll_to_pointer(y);
            return;
        }

        if let Some(index) = view::nav_buttons(self.layout.toolbar)
            .iter()
            .position(|rect| rect.contains(x, y))
        {
            match index {
                0 if self.can_go_back() => self.jump(self.history_index - 1),
                1 if self.can_go_forward() => self.jump(self.history_index + 1),
                2 => self.go_up(),
                _ => {}
            }
            return;
        }

        let crumbs = model::breadcrumbs(&self.cwd);
        if let Some(index) = view::crumb_rects(self.layout.toolbar, &crumbs)
            .iter()
            .position(|rect| rect.width > 0 && rect.contains(x, y))
        {
            self.navigate(crumbs[index].1.clone());
            return;
        }

        if let Some(index) = view::place_at(self.layout.rail, self.places.len(), x, y) {
            self.navigate(self.places[index].path.clone());
            return;
        }

        if self.layout.list.contains(x, y)
            && let Some(index) = view::row_at(self.layout.list, self.scroll, y)
            && index < self.entries.len()
        {
            let now = Instant::now();
            let repeat = self
                .last_click
                .is_some_and(|(row, at)| row == index && now.duration_since(at) < DOUBLE_CLICK);
            self.last_click = Some((index, now));

            if self.selected != index {
                self.cancel_rename();
            }
            self.selected = index;
            self.refresh_preview();
            if repeat {
                self.open_selected();
            }
        }
    }

    /// Put the scroll where the pointer is on the track.
    fn scroll_to_pointer(&mut self, y: i32) {
        let list = self.layout.list;
        let fraction = ((y - list.y) as f32 / list.height.max(1) as f32).clamp(0.0, 1.0);
        self.scroll = (self.entries.len() as f32 * fraction) as usize;
        self.clamp_scroll();
    }

    /// Returns false when the window should close.
    fn on_key(&mut self, event: &WindowEvent) -> bool {
        let code = event.code;

        if update_modifiers(&mut self.mods, code, true) {
            return true;
        }

        // Alt marks a chord as the window manager's. The manager claims the
        // ones it acts on, and those never arrive here, but an unclaimed one
        // still does, and it is not a binding this program made: Alt+N is not
        // a request for a new folder.
        if self.mods.alt {
            return true;
        }

        if code == keycode::ESCAPE {
            if self.rename.is_some() {
                self.cancel_rename();
            } else if matches!(self.note, Note::Confirm { .. }) {
                self.note = Note::Message("Kept.".to_string());
            }
            return true;
        }

        if let Note::Confirm { .. } = self.note
            && matches!(code, keycode::RETURN | keycode::NUMPAD_ENTER)
        {
            self.delete_confirmed();
            return true;
        }

        // While a name is being edited every key belongs to the field.
        if self.rename.is_some() {
            return true;
        }

        match code {
            keycode::ARROW_DOWN => self.move_selection(1),
            keycode::ARROW_UP => self.move_selection(-1),
            keycode::PAGE_DOWN => self.move_selection(self.layout.rows_visible as i32),
            keycode::PAGE_UP => self.move_selection(-(self.layout.rows_visible as i32)),
            keycode::HOME => self.move_selection(i32::MIN / 2),
            keycode::END => self.move_selection(i32::MAX / 2),
            keycode::RETURN | keycode::NUMPAD_ENTER => self.open_selected(),
            keycode::BACKSPACE => self.go_up(),
            keycode::F2 => self.begin_rename(true),
            keycode::DELETE => self.ask_to_delete(),
            keycode::N => self.new_folder(),
            keycode::F5 => {
                self.mounts = edos_lib::mounts::list();
                self.places = model::places(&self.mounts);
                let name = self.selected_entry().map(|entry| entry.name.clone());
                self.reload();
                if let Some(name) = name {
                    self.select_named(&name);
                }
            }
            _ => {}
        }
        true
    }

    // --- Drawing ------------------------------------------------------------

    fn draw(&mut self) {
        let crumbs = model::breadcrumbs(&self.cwd);
        let active_place = self
            .places
            .iter()
            .enumerate()
            .filter(|(_, place)| self.cwd == place.path)
            .map(|(index, _)| index)
            .next();
        let span = view::Span::of(&self.entries);
        let details = self.details_content();

        let (width, height) = (self.window.width, self.window.height);
        let layout = Layout::new(width, height);
        let renaming = self.rename.is_some();
        let hover = self.hover;
        let selected = self.selected;
        let scroll = self.scroll;
        let (message, warning) = self.status_line();
        let volume_note = self.volume_note();
        let can_back = self.can_go_back();
        let can_forward = self.can_go_forward();
        let can_up = self.can_go_up();

        let Some(buf) = self.window.buffer_mut() else {
            return;
        };
        let mut canvas = Surface::new(buf, width, height);
        canvas.fill(
            Rect::new(0, 0, width, height),
            edos_render::theme::Theme::DEFAULT.background.raw(),
        );

        view::draw_toolbar(
            &mut canvas,
            &layout,
            &crumbs,
            &view::Nav {
                can_go_back: can_back,
                can_go_forward: can_forward,
                can_go_up: can_up,
                hovered_button: hover.button,
                hovered_crumb: hover.crumb,
            },
        );
        view::draw_rail(
            &mut canvas,
            &layout,
            &self.places,
            active_place,
            hover.place,
            self.volume.as_ref(),
        );
        view::draw_header(&mut canvas, &layout);
        view::draw_list(
            &mut canvas,
            &layout,
            &self.entries,
            &view::ListState {
                selected,
                scroll,
                hovered: hover.row,
                span,
                renaming,
            },
        );
        if let Some(pane) = layout.details {
            let thumbnail = self
                .preview
                .as_ref()
                .map(|preview| (preview.pixels.as_slice(), preview.width, preview.height));
            view::draw_details(
                &mut canvas,
                pane,
                &details.title,
                &details.subtitle,
                &details.facts,
                thumbnail,
            );
        }
        view::draw_status(&mut canvas, &layout, &message, warning, &volume_note);

        if let Some(rename) = &self.rename {
            rename.widgets.draw_all(&mut canvas);
        }

        self.window.swap_buffers();
    }

    /// What the status strip says on the left, and whether it says it in the
    /// colour of something going wrong.
    fn status_line(&self) -> (String, bool) {
        match &self.note {
            Note::Warning(message) => (message.clone(), true),
            Note::Message(message) => (message.clone(), false),
            Note::Confirm { name, .. } => (
                format!("Delete {name}? Enter deletes it, Esc keeps it."),
                true,
            ),
            Note::None => {
                let folders = self
                    .entries
                    .iter()
                    .filter(|entry| entry.kind == Kind::Dir)
                    .count();
                let items = self
                    .entries
                    .iter()
                    .filter(|entry| entry.kind != Kind::Parent)
                    .count();
                let plural = |n: usize, word: &str| {
                    if n == 1 {
                        format!("1 {word}")
                    } else {
                        format!("{n} {word}s")
                    }
                };
                (
                    format!("{}, {}", plural(items, "item"), plural(folders, "folder")),
                    false,
                )
            }
        }
    }

    /// What the strip says on the right: the volume under the current
    /// directory, and either how much room is left or why there is no such
    /// number.
    fn volume_note(&self) -> String {
        let Some(volume) = &self.volume else {
            return String::new();
        };
        match volume.used_fraction() {
            Some(_) => format!(
                "{} on {}, {} free",
                volume.filesystem.name(),
                volume.mount_point,
                model::format_size(volume.free_bytes)
            ),
            None => format!(
                "{} on {}, {}",
                volume.filesystem.name(),
                volume.mount_point,
                volume.filesystem.nature()
            ),
        }
    }

    fn details_content(&self) -> Details {
        let volume = self
            .volume
            .as_ref()
            .map(|volume| format!("{} on {}", volume.filesystem.name(), volume.mount_point));

        let Some(entry) = self.selected_entry() else {
            let mut facts = vec![("Holds", "nothing".to_string())];
            if let Some(volume) = volume {
                facts.push(("Volume", volume));
            }
            facts.push(("Where", self.cwd.clone()));
            return Details {
                title: directory_name(&self.cwd),
                subtitle: "folder".to_string(),
                facts,
            };
        };

        let mut facts = Vec::new();
        if let Some(size) = entry.size {
            facts.push(("Size", model::format_exact(size)));
        }
        if let Some(target) = &entry.target {
            facts.push(("Points to", target.clone()));
        }
        if let Some(modified) = entry.modified {
            facts.push(("Modified", model::format_time(modified)));
        }
        if let Some(volume) = volume {
            facts.push(("Volume", volume));
        }
        facts.push(("Where", self.cwd.clone()));

        Details {
            title: if entry.kind == Kind::Parent {
                directory_name(&model::parent(&self.cwd))
            } else {
                entry.name.clone()
            },
            subtitle: match entry.kind {
                Kind::Parent => "the folder above".to_string(),
                Kind::Link if entry.opens_directory => "link to a folder".to_string(),
                Kind::Link => "link".to_string(),
                Kind::Dir => "folder".to_string(),
                Kind::Special => "device".to_string(),
                // A name with an extension is named by it; one without is
                // just a file, and "file file" is neither.
                Kind::File => match entry.kind_label() {
                    label if label == Kind::File.label() => label,
                    label => format!("{label} file"),
                },
            },
            facts,
        }
    }
}

/// What the details pane shows.
struct Details {
    title: String,
    subtitle: String,
    facts: Vec<(&'static str, String)>,
}

/// The name of a directory, with the root called by its own separator.
fn directory_name(path: &str) -> String {
    match path.rsplit('/').next() {
        Some("") | None => "/".to_string(),
        Some(name) => name.to_string(),
    }
}

/// Scale a decoded picture down to what the details pane can show.
fn scale_preview(image: &Image, path: String) -> Preview {
    let room_w = view::DETAILS_W - view::PAD * 2;
    let (width, height) = image.fit_size(room_w, THUMBNAIL_H);
    Preview {
        path,
        pixels: image.scaled_to_fit(room_w, THUMBNAIL_H),
        width,
        height,
    }
}

/// Where to open. An argument wins, then `HOME`, then the root, and a path that
/// is not a directory falls back the same way rather than opening an empty
/// window.
fn starting_directory() -> String {
    let candidates = std::env::args().nth(1).into_iter().chain([
        std::env::var("HOME").unwrap_or_else(|_| "/root".to_string()),
        "/".to_string(),
    ]);
    for candidate in candidates {
        if fs::metadata(&candidate).is_ok_and(|meta| meta.is_dir()) {
            return candidate;
        }
    }
    "/".to_string()
}

fn main() {
    let window = match Window::new(120, 90, WIN_W, WIN_H) {
        Ok(window) => window,
        Err(err) => {
            eprintln!("files: could not open a window: {err:?}");
            std::process::exit(1);
        }
    };

    let mut app = App::new(window, starting_directory());
    if app.window.show().is_err() {
        eprintln!("files: could not show the window");
        std::process::exit(1);
    }

    let mut events = [WindowEvent::default(); 16];
    loop {
        if let Ok(count) = app.window.poll_events(&mut events) {
            for event in events.iter().take(count) {
                if !app.handle(event) {
                    return;
                }
            }
        }
        app.draw();
        std::thread::sleep(Duration::from_millis(16));
    }
}
