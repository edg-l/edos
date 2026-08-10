//! Processes: a live view of the kernel's thread table.
//!
//! The table is re-read on a timer, the selected row's detail comes from
//! `/proc/<tid>/{status,cmdline}`, and the selection is kept by pid so a
//! refresh that reorders or drops rows does not move it under the reader.

mod procinfo;

use std::time::{Duration, Instant};

use edos_lib::keymap::keycode;
use edos_lib::process::sys_kill;
use edos_render::metrics::{TEXT_CELL_HEIGHT, space};
use edos_render::text::{self, Style};
use edos_render::theme::Theme;
use edos_render::widgets::{
    draw_rect, draw_rect_outline, draw_text, draw_text_styled, text_height, text_width,
};
use edos_render::window::{Window, WindowEvent, WindowEventType, property, window_set};

use procinfo::{Details, Memory, Process, Table};

const WIN_W: u32 = 680;
const WIN_H: u32 = 440;

/// Margin from the window edge to the content.
const MARGIN: u32 = space(3);
/// Height of one table row: a text cell with a step of air above and below.
const ROW_H: u32 = TEXT_CELL_HEIGHT + space(2);
/// Gap between two columns.
const COL_GAP: u32 = space(3);
/// Gap between the bands of the window: summary, table, detail, hints.
const BAND_GAP: u32 = space(2);
/// Width of the scroll indicator beside the rows, and of the bar marking the
/// selected row.
const MARKER_W: u32 = space(1);
/// Inner padding of the kill confirmation.
const DIALOG_PAD: u32 = space(4);
/// Rows one wheel notch moves the view.
const WHEEL_ROWS: usize = 3;
/// How often the table is re-read.
const REFRESH: Duration = Duration::from_millis(1000);
/// How long the loop sleeps between polls, which is also how soon a keystroke
/// is answered.
const TICK: Duration = Duration::from_millis(16);
/// The signal the kill action sends.
const SIGTERM: u32 = 15;

/// The table is a grid, so its cells are set in the monospaced face and a
/// column of digits lines up. The chrome around it stays in the proportional
/// face the rest of the shell uses.
const CELL: Style = Style::mono(0);
/// The proportional face, for everything that is not a table cell.
const CHROME: Style = Style::new(0);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Right,
}

struct Column {
    title: &'static str,
    align: Align,
}

/// The columns, in the order `/proc/processes` prints them. The name goes last
/// because it is the only one without a bound, so it is the one to clip.
const COLUMNS: [Column; NCOLS] = [
    Column {
        title: "PID",
        align: Align::Right,
    },
    Column {
        title: "PPID",
        align: Align::Right,
    },
    Column {
        title: "TYPE",
        align: Align::Left,
    },
    Column {
        title: "STATE",
        align: Align::Left,
    },
    Column {
        title: "CPU",
        align: Align::Right,
    },
    Column {
        title: "TIME ms",
        align: Align::Right,
    },
    Column {
        title: "RSS KiB",
        align: Align::Right,
    },
    Column {
        title: "NAME",
        align: Align::Left,
    },
];

const NCOLS: usize = 8;
/// The state column, which is the one coloured by what it says.
const STATE_COL: usize = 3;
/// The column that absorbs the width the others leave.
const NAME_COL: usize = NCOLS - 1;

/// One drawable row: the cells, plus what their colours are chosen from.
struct Row {
    pid: u64,
    name: String,
    kernel: bool,
    running: bool,
    cells: [String; NCOLS],
}

impl Row {
    fn new(process: &Process) -> Self {
        Self {
            pid: process.pid,
            name: process.name.clone(),
            kernel: process.is_kernel(),
            running: process.is_running(),
            cells: [
                process.pid.to_string(),
                process.ppid.to_string(),
                process.kind.clone(),
                process.state.clone(),
                process.cpu.to_string(),
                process.cpu_ms.to_string(),
                // A kernel thread runs in whatever address space it was
                // scheduled over, so it has no resident size to report.
                process
                    .rss_kib
                    .map(|kib| kib.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                process.name.clone(),
            ],
        }
    }
}

/// Width of `text` set in `style`, in pixels. Both faces are measured through
/// the same call: a character count times a cell width is wrong for the
/// proportional face, and hard-coding one for the monospaced face would drift
/// the moment the type scale moves.
fn measure(text: &str, style: Style) -> u32 {
    text::width(text, style)
}

/// Height of one line of table text.
fn cell_height() -> u32 {
    text::line_height(CELL)
}

/// `text`, shortened with an ellipsis until it fits `max_w` pixels.
fn fit(text: &str, max_w: u32, style: Style) -> String {
    const ELLIPSIS: &str = "...";
    if measure(text, style) <= max_w {
        return text.to_string();
    }
    let mut kept = String::new();
    for ch in text.chars() {
        let mut candidate = kept.clone();
        candidate.push(ch);
        if measure(&format!("{candidate}{ELLIPSIS}"), style) > max_w {
            break;
        }
        kept = candidate;
    }
    kept.push_str(ELLIPSIS);
    kept
}

/// Top of one line of text centred in a band of `height` starting at `y`.
fn centred_text_y(y: i32, height: u32, line: u32) -> i32 {
    y + (height as i32 - line as i32) / 2
}

/// Where each band of the window sits, for the current window size.
struct Bands {
    summary_y: i32,
    header_y: i32,
    rows_y: i32,
    rows_h: u32,
    detail_y: i32,
    hints_y: i32,
    content_w: u32,
}

impl Bands {
    fn new(width: u32, height: u32) -> Self {
        let line = text_height();
        let summary_y = MARGIN as i32;
        let header_y = summary_y + line as i32 + BAND_GAP as i32;
        let rows_y = header_y + ROW_H as i32;
        let hints_y = height as i32 - MARGIN as i32 - line as i32;
        let detail_y = hints_y - BAND_GAP as i32 - line as i32;
        let rows_h = (detail_y - BAND_GAP as i32 - rows_y).max(0) as u32;
        Self {
            summary_y,
            header_y,
            rows_y,
            rows_h,
            detail_y,
            hints_y,
            content_w: width.saturating_sub(MARGIN * 2),
        }
    }

    /// How many whole rows the table band holds.
    fn visible_rows(&self) -> usize {
        (self.rows_h / ROW_H) as usize
    }

    /// Width the columns share. The scroll indicator is reserved whether or
    /// not it is drawn, so the rows do not reflow when it appears.
    fn table_w(&self) -> u32 {
        self.content_w.saturating_sub(MARKER_W + COL_GAP)
    }

    /// Index of the row a client-area `y` falls in, counting from the top of
    /// the view rather than from the top of the list.
    fn row_at(&self, y: i32) -> Option<usize> {
        if y < self.rows_y {
            return None;
        }
        let offset = (y - self.rows_y) as usize / ROW_H as usize;
        (offset < self.visible_rows()).then_some(offset)
    }
}

/// Column widths and origins for one frame.
struct Grid {
    x0: i32,
    widths: [u32; NCOLS],
}

impl Grid {
    /// Size every column to the widest thing in it, then give the name column
    /// whatever is left over.
    fn new(x0: i32, table_w: u32, rows: &[Row]) -> Self {
        let mut widths = [0u32; NCOLS];
        for (index, column) in COLUMNS.iter().enumerate() {
            widths[index] = measure(column.title, CELL);
        }
        for row in rows {
            for (index, cell) in row.cells.iter().enumerate() {
                widths[index] = widths[index].max(measure(cell, CELL));
            }
        }
        let fixed: u32 = widths[..NAME_COL].iter().sum::<u32>() + COL_GAP * NAME_COL as u32;
        widths[NAME_COL] = table_w.saturating_sub(fixed);
        Self { x0, widths }
    }

    /// Left edge of column `index`.
    fn x(&self, index: usize) -> i32 {
        self.x0 + (self.widths[..index].iter().sum::<u32>() + COL_GAP * index as u32) as i32
    }

    /// Left edge of a `text_w` wide string in column `index`, by its alignment.
    fn text_x(&self, index: usize, text_w: u32) -> i32 {
        match COLUMNS[index].align {
            Align::Left => self.x(index),
            Align::Right => self.x(index) + self.widths[index] as i32 - text_w as i32,
        }
    }
}

/// Draw one row of cells, asking `ink` for each cell's colour.
fn draw_cells(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    grid: &Grid,
    y: i32,
    cells: &[String; NCOLS],
    ink: impl Fn(usize) -> u32,
) {
    let text_y = centred_text_y(y, ROW_H, cell_height());
    for (index, cell) in cells.iter().enumerate() {
        let text = fit(cell, grid.widths[index], CELL);
        let x = grid.text_x(index, measure(&text, CELL));
        draw_text_styled(
            buffer,
            width,
            height,
            x,
            text_y,
            &text,
            Style::mono(ink(index)),
        );
    }
}

/// A kill waiting to be confirmed.
struct Confirm {
    pid: u64,
    name: String,
}

struct App {
    rows: Vec<Row>,
    memory: Option<Memory>,
    details: Option<Details>,
    /// Trailer of the process table: exit records nobody has collected, and
    /// the pid of the process the kernel started.
    pending_exits: u64,
    init_pid: u64,
    /// Shown in place of the summary until the next refresh, for a failed read
    /// or a refused action.
    notice: Option<String>,
    /// The selection is a pid, not an index: rows come and go every refresh.
    selected: Option<u64>,
    scroll: usize,
    confirm: Option<Confirm>,
    last_refresh: Instant,
    /// Set when something on screen has changed. The table moves once a
    /// second, so painting every tick would spend the compositor's frame
    /// budget redrawing an identical window.
    dirty: bool,
}

impl App {
    fn new() -> Self {
        let mut app = Self {
            rows: Vec::new(),
            memory: None,
            details: None,
            pending_exits: 0,
            init_pid: 0,
            notice: None,
            selected: None,
            scroll: 0,
            confirm: None,
            last_refresh: Instant::now(),
            dirty: true,
        };
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        self.dirty = true;
        let previous = self.selected_index();
        match procinfo::read_table() {
            Ok(Table {
                processes,
                pending_exits,
                init_pid,
            }) => {
                self.rows = processes.iter().map(Row::new).collect();
                self.pending_exits = pending_exits;
                self.init_pid = init_pid;
                self.notice = None;
            }
            Err(e) => {
                self.rows.clear();
                self.notice = Some(format!("cannot read /proc/processes: {e}"));
            }
        }
        self.memory = procinfo::read_memory().ok();

        if self.selected_index().is_none() {
            // The selected thread has exited. Take whatever moved into its
            // place, rather than throwing the reader back to the top of a list
            // they had scrolled through.
            let index = previous.unwrap_or(0).min(self.rows.len().saturating_sub(1));
            self.selected = self.rows.get(index).map(|row| row.pid);
        }
        self.read_details();
    }

    fn read_details(&mut self) {
        self.details = self
            .selected
            .and_then(|pid| procinfo::read_details(pid).ok());
    }

    fn selected_index(&self) -> Option<usize> {
        let pid = self.selected?;
        self.rows.iter().position(|row| row.pid == pid)
    }

    /// Move the selection by `delta` rows and follow it with the view.
    fn move_selection(&mut self, delta: isize, visible: usize) {
        if self.rows.is_empty() {
            return;
        }
        let current = self.selected_index().unwrap_or(0) as isize;
        let last = self.rows.len() as isize - 1;
        let next = current.saturating_add(delta).clamp(0, last) as usize;
        self.select_at(next, visible);
    }

    fn select_at(&mut self, index: usize, visible: usize) {
        let Some(row) = self.rows.get(index) else {
            return;
        };
        self.selected = Some(row.pid);
        self.dirty = true;
        self.read_details();
        self.clamp_scroll(visible);
    }

    fn scroll_by(&mut self, delta: isize, visible: usize) {
        let max_scroll = self.rows.len().saturating_sub(visible) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max_scroll) as usize;
        self.dirty = true;
    }

    /// Keep the view inside the list, and the selection inside the view.
    fn clamp_scroll(&mut self, visible: usize) {
        let max_scroll = self.rows.len().saturating_sub(visible);
        self.scroll = self.scroll.min(max_scroll);
        let Some(index) = self.selected_index() else {
            return;
        };
        if index < self.scroll {
            self.scroll = index;
        } else if visible > 0 && index >= self.scroll + visible {
            self.scroll = index + 1 - visible;
        }
    }

    /// Ask before killing: the selection moves with the arrow keys, so what is
    /// highlighted is not always what the reader thinks they are pointing at.
    fn request_kill(&mut self) {
        let Some(index) = self.selected_index() else {
            return;
        };
        let row = &self.rows[index];
        if row.kernel {
            self.notice = Some(format!(
                "{} is a kernel thread, and is not killable",
                row.name
            ));
            return;
        }
        self.confirm = Some(Confirm {
            pid: row.pid,
            name: row.name.clone(),
        });
    }

    fn confirm_kill(&mut self) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        let result = sys_kill(confirm.pid, SIGTERM);
        // The thread leaves the table on its own once it dies, so refresh
        // rather than leave a row the reader can act on again.
        self.refresh();
        if result != 0 {
            self.notice = Some(format!(
                "cannot signal pid {}: kill returned {result}",
                confirm.pid
            ));
        }
    }

    fn handle_key(&mut self, code: u32, bands: &Bands) {
        let visible = bands.visible_rows();
        let page = visible.max(1) as isize;
        let whole_list = self.rows.len() as isize;
        self.dirty = true;

        if self.confirm.is_some() {
            match code {
                keycode::Y | keycode::RETURN | keycode::NUMPAD_ENTER => self.confirm_kill(),
                keycode::N | keycode::ESCAPE => self.confirm = None,
                _ => {}
            }
            return;
        }

        match code {
            keycode::ARROW_UP => self.move_selection(-1, visible),
            keycode::ARROW_DOWN => self.move_selection(1, visible),
            keycode::PAGE_UP => self.move_selection(-page, visible),
            keycode::PAGE_DOWN => self.move_selection(page, visible),
            keycode::HOME => self.move_selection(-whole_list, visible),
            keycode::END => self.move_selection(whole_list, visible),
            keycode::DELETE => self.request_kill(),
            _ => {}
        }
    }

    fn draw(&self, buffer: &mut [u32], width: u32, height: u32, bands: &Bands) {
        let theme = &Theme::DEFAULT;
        let visible = bands.visible_rows();
        let grid = Grid::new(MARGIN as i32, bands.table_w(), &self.rows);

        draw_text(
            buffer,
            width,
            height,
            MARGIN as i32,
            bands.summary_y,
            &fit(&self.summary(), bands.content_w, CHROME),
            if self.notice.is_some() {
                theme.focus_ring.raw()
            } else {
                theme.label_text.raw()
            },
        );

        draw_rect(
            buffer,
            width,
            height,
            MARGIN as i32,
            bands.header_y,
            bands.content_w,
            ROW_H,
            theme.button_normal.raw(),
        );
        let header: [String; NCOLS] = std::array::from_fn(|i| COLUMNS[i].title.to_string());
        draw_cells(
            buffer,
            width,
            height,
            &grid,
            bands.header_y,
            &header,
            |_| theme.label_text.raw(),
        );

        for (offset, row) in self.rows.iter().skip(self.scroll).take(visible).enumerate() {
            let y = bands.rows_y + (offset as u32 * ROW_H) as i32;
            if self.selected == Some(row.pid) {
                draw_rect(
                    buffer,
                    width,
                    height,
                    MARGIN as i32,
                    y,
                    bands.content_w,
                    ROW_H,
                    theme.button_hover.raw(),
                );
                draw_rect(
                    buffer,
                    width,
                    height,
                    MARGIN as i32,
                    y,
                    MARKER_W,
                    ROW_H,
                    theme.focus_ring.raw(),
                );
            }
            // A kernel thread is not something the reader can act on, so it
            // recedes; a running one is what they are looking for.
            let base = if row.kernel {
                theme.text_placeholder.raw()
            } else {
                theme.text_primary.raw()
            };
            let state_ink = if row.running {
                theme.checkbox_check.raw()
            } else {
                base
            };
            draw_cells(buffer, width, height, &grid, y, &row.cells, |index| {
                if index == STATE_COL { state_ink } else { base }
            });
        }

        self.draw_scrollbar(buffer, width, height, bands, visible);

        draw_text(
            buffer,
            width,
            height,
            MARGIN as i32,
            bands.detail_y,
            &fit(&self.detail(), bands.content_w, CHROME),
            theme.text_primary.raw(),
        );
        draw_rect(
            buffer,
            width,
            height,
            MARGIN as i32,
            bands.hints_y - BAND_GAP as i32,
            bands.content_w,
            1,
            theme.input_border.raw(),
        );
        draw_text(
            buffer,
            width,
            height,
            MARGIN as i32,
            bands.hints_y,
            &fit(
                "Up/Down select   PgUp/PgDn page   Home/End ends   Del kill",
                bands.content_w,
                CHROME,
            ),
            theme.text_placeholder.raw(),
        );

        if let Some(confirm) = &self.confirm {
            draw_confirm(buffer, width, height, confirm);
        }
    }

    fn draw_scrollbar(
        &self,
        buffer: &mut [u32],
        width: u32,
        height: u32,
        bands: &Bands,
        visible: usize,
    ) {
        if visible == 0 || self.rows.len() <= visible {
            return;
        }
        let theme = &Theme::DEFAULT;
        let x = (MARGIN + bands.content_w - MARKER_W) as i32;
        draw_rect(
            buffer,
            width,
            height,
            x,
            bands.rows_y,
            MARKER_W,
            bands.rows_h,
            theme.slider_track.raw(),
        );
        let span = bands.rows_h as usize;
        let thumb_h = (span * visible / self.rows.len()).max(ROW_H as usize);
        let travel = span.saturating_sub(thumb_h);
        let max_scroll = self.rows.len() - visible;
        let thumb_y = bands.rows_y + (travel * self.scroll / max_scroll) as i32;
        draw_rect(
            buffer,
            width,
            height,
            x,
            thumb_y,
            MARKER_W,
            thumb_h as u32,
            theme.slider_thumb.raw(),
        );
    }

    fn summary(&self) -> String {
        if let Some(notice) = &self.notice {
            return notice.clone();
        }
        let memory = match &self.memory {
            Some(memory) => format!(
                "memory {} of {} MiB   ",
                memory.used_kib / 1024,
                memory.total_kib / 1024
            ),
            None => String::new(),
        };
        format!(
            "{} threads   {}init pid {}   pending exit statuses {}",
            self.rows.len(),
            memory,
            self.init_pid,
            self.pending_exits
        )
    }

    fn detail(&self) -> String {
        match (&self.details, self.selected) {
            (Some(details), _) => format!(
                "{}   priority {}   affinity {}   vmas {}   vm size {}   cwd {}",
                details.cmdline,
                details.priority,
                details.affinity,
                details.vmas,
                details.vm_size,
                details.cwd
            ),
            (None, Some(pid)) => format!("pid {pid} has no detail to read"),
            (None, None) => "no thread selected".to_string(),
        }
    }
}

/// Draw the kill confirmation over the middle of the window.
fn draw_confirm(buffer: &mut [u32], width: u32, height: u32, confirm: &Confirm) {
    let theme = &Theme::DEFAULT;
    let line = text_height();
    let question = format!("Send SIGTERM to {} (pid {})?", confirm.name, confirm.pid);
    let answer = "Y: kill      N: cancel";

    let box_w = (text_width(&question).max(text_width(answer)) + DIALOG_PAD * 2).min(width);
    let box_h = line * 2 + BAND_GAP + DIALOG_PAD * 2;
    let box_x = (width as i32 - box_w as i32) / 2;
    let box_y = (height as i32 - box_h as i32) / 2;

    draw_rect(
        buffer,
        width,
        height,
        box_x,
        box_y,
        box_w,
        box_h,
        theme.input_bg.raw(),
    );
    draw_rect_outline(
        buffer,
        width,
        height,
        box_x,
        box_y,
        box_w,
        box_h,
        theme.focus_ring.raw(),
    );

    let inner_w = box_w.saturating_sub(DIALOG_PAD * 2);
    let text_x = box_x + DIALOG_PAD as i32;
    draw_text(
        buffer,
        width,
        height,
        text_x,
        box_y + DIALOG_PAD as i32,
        &fit(&question, inner_w, CHROME),
        theme.text_primary.raw(),
    );
    draw_text(
        buffer,
        width,
        height,
        text_x,
        box_y + (DIALOG_PAD + line + BAND_GAP) as i32,
        answer,
        theme.text_placeholder.raw(),
    );
}

fn main() {
    let mut window = match Window::new(120, 90, WIN_W, WIN_H) {
        Ok(window) => window,
        Err(e) => {
            eprintln!("procview: could not create a window: {e:?}");
            return;
        }
    };
    let _ = window.set_title("Processes");

    let mut app = App::new();
    let mut events = [WindowEvent::default(); 32];

    // Painted before it is mapped, so the first frame the compositor picks up
    // is the table rather than an empty buffer.
    paint(&mut window, &mut app);
    if let Err(e) = window_set(window.id, property::VISIBLE, 1) {
        eprintln!("procview: could not show the window: {e:?}");
        return;
    }

    loop {
        let mut bands = Bands::new(window.width, window.height);

        if let Ok(count) = window.poll_events(&mut events) {
            for event in &events[..count] {
                match event.event_type() {
                    Some(WindowEventType::CloseRequested) => return,
                    Some(WindowEventType::Resize) => {
                        if window.resize(event.x as u32, event.y as u32).is_err() {
                            eprintln!("procview: could not resize the window");
                        }
                        bands = Bands::new(window.width, window.height);
                        app.clamp_scroll(bands.visible_rows());
                        app.dirty = true;
                    }
                    Some(WindowEventType::KeyPress) => app.handle_key(event.code, &bands),
                    Some(WindowEventType::MouseScroll) => {
                        // The wheel delta is an i8 carried in `data`, positive
                        // away from the reader.
                        let delta = event.data as u8 as i8 as isize;
                        app.scroll_by(-delta * WHEEL_ROWS as isize, bands.visible_rows());
                    }
                    Some(WindowEventType::MouseButton) if event.data != 0 && event.code == 0 => {
                        if app.confirm.is_none()
                            && let Some(offset) = bands.row_at(event.y)
                        {
                            app.select_at(app.scroll + offset, bands.visible_rows());
                        }
                    }
                    _ => {}
                }
            }
        }

        if app.last_refresh.elapsed() >= REFRESH {
            app.refresh();
            app.clamp_scroll(bands.visible_rows());
        }

        if app.dirty {
            paint(&mut window, &mut app);
        }
        std::thread::sleep(TICK);
    }
}

/// Repaint the whole window. Both buffers are drawn from scratch, so a frame
/// skipped while nothing changed leaves nothing stale behind.
fn paint(window: &mut Window, app: &mut App) {
    let bands = Bands::new(window.width, window.height);
    app.dirty = false;
    window.fill(Theme::DEFAULT.background.raw());
    let (width, height) = (window.width, window.height);
    if let Some(buffer) = window.buffer_mut() {
        app.draw(buffer, width, height, &bands);
    }
    window.swap_buffers();
}
