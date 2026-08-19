//! snake - the terminal game, on a timer.
//!
//! The interesting part is not the game, it is that a game is the shape of
//! program this system had never run: it has to redraw on a clock the user
//! does not drive, and read the keyboard without waiting for it. So every
//! frame is one `poll` on stdin with whatever is left of the tick as its
//! timeout, which is the only way to be both responsive to a key and honest
//! about the interval; sleeping the tick and then reading would drop keys, and
//! reading without a timeout would stop the clock.
//!
//! A cell is two terminal columns wide, because a character cell is about
//! twice as tall as it is wide and a snake drawn one column per cell moves
//! visibly faster sideways than up.

use std::collections::VecDeque;
use std::io::{Write, stdout};
use std::process::exit;
use std::time::{Duration, Instant};

use edos_lib::io::{get_winsize, isatty, poll_stdin, pty_set_canonical, pty_set_raw, sys_read};
use edos_lib::time::clock_gettime_nanos;

/// Milliseconds per tick before the snake has eaten anything.
const DEFAULT_SPEED_MS: u64 = 130;
/// Milliseconds shaved off the tick per food eaten. The floor is half the
/// starting speed, so `-s` is the one dial and the ramp is derived from it.
const SPEEDUP_MS: u64 = 4;
/// Terminal columns one board cell occupies.
const CELL_W: usize = 2;
/// The smallest board worth playing on; below this the terminal is too small.
const MIN_W: usize = 12;
const MIN_H: usize = 6;
/// Rows the chrome takes: the score line, both borders, and the key legend.
const CHROME_ROWS: usize = 4;
/// Snake length at the start, including the head.
const START_LEN: usize = 4;
/// Longest a wait on the keyboard runs before the loop looks at the clock
/// again. A paused or finished game has no tick to wait for, and an unbounded
/// timeout would be a poll the game could never be woken out of.
const IDLE_WAIT_MS: u64 = 200;

const EMPTY: &str = "\x1b[0m  ";
const WALL: &str = "\x1b[100m  \x1b[0m";
const BODY: &str = "\x1b[42m  \x1b[0m";
const HEAD: &str = "\x1b[102m  \x1b[0m";
const FOOD: &str = "\x1b[41m  \x1b[0m";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    fn delta(self) -> (i32, i32) {
        match self {
            Dir::Up => (0, -1),
            Dir::Down => (0, 1),
            Dir::Left => (-1, 0),
            Dir::Right => (1, 0),
        }
    }

    fn is_opposite(self, other: Dir) -> bool {
        matches!(
            (self, other),
            (Dir::Up, Dir::Down)
                | (Dir::Down, Dir::Up)
                | (Dir::Left, Dir::Right)
                | (Dir::Right, Dir::Left)
        )
    }
}

enum Key {
    Dir(Dir),
    Char(u8),
}

/// stdin, decoded into keys. Escape sequences arrive a byte at a time as far
/// as a reader is concerned, so an incomplete one stays in the buffer until
/// the rest of it turns up rather than being reported as a bare Escape.
#[derive(Default)]
struct Input {
    buf: VecDeque<u8>,
}

impl Input {
    /// Wait up to `timeout_ms` for bytes and take everything available.
    /// Returns false when stdin is closed, which ends the game.
    fn fill(&mut self, timeout_ms: u64) -> bool {
        if !poll_stdin(timeout_ms) {
            return true;
        }
        let mut chunk = [0u8; 32];
        let n = sys_read(0, &mut chunk);
        if n <= 0 {
            return false;
        }
        self.buf.extend(&chunk[..n as usize]);
        true
    }

    fn next_key(&mut self) -> Option<Key> {
        let first = *self.buf.front()?;
        if first != 0x1b {
            self.buf.pop_front();
            return Some(Key::Char(first));
        }
        // CSI A/B/C/D are the arrow keys. Anything else starting with Escape
        // is not a control this game has, so it is consumed as a plain byte.
        match self.buf.get(1) {
            None => None,
            Some(b'[') => match self.buf.get(2) {
                None => None,
                Some(&code) => {
                    self.buf.drain(..3);
                    match code {
                        b'A' => Some(Key::Dir(Dir::Up)),
                        b'B' => Some(Key::Dir(Dir::Down)),
                        b'C' => Some(Key::Dir(Dir::Right)),
                        b'D' => Some(Key::Dir(Dir::Left)),
                        _ => Some(Key::Char(0)),
                    }
                }
            },
            Some(_) => {
                self.buf.pop_front();
                Some(Key::Char(0x1b))
            }
        }
    }
}

/// xorshift64*, seeded from the clock. Food placement is the only thing that
/// needs randomness and it needs no more than this.
struct Rng(u64);

impl Rng {
    fn new() -> Self {
        Rng(clock_gettime_nanos().unwrap_or(0x2545F4914F6CDD1D) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Playing,
    Paused,
    Lost,
    Won,
}

struct Game {
    w: usize,
    h: usize,
    /// Head first. The tail end is what a move drops when nothing was eaten.
    snake: VecDeque<(usize, usize)>,
    /// One flag per cell, so a self-collision costs a lookup rather than a
    /// walk of the whole snake every tick.
    occupied: Vec<bool>,
    dir: Dir,
    /// The next direction accepted from the keyboard. Kept apart from `dir`
    /// because two keys pressed inside one tick must not turn the snake back
    /// into itself: only `dir`, the direction actually travelled, decides
    /// what counts as a reversal.
    pending: Dir,
    food: (usize, usize),
    score: u32,
    state: State,
    wrap: bool,
    rng: Rng,
}

impl Game {
    fn new(w: usize, h: usize, wrap: bool, rng: Rng) -> Self {
        let mut game = Game {
            w,
            h,
            snake: VecDeque::new(),
            occupied: vec![false; w * h],
            dir: Dir::Right,
            pending: Dir::Right,
            food: (0, 0),
            score: 0,
            state: State::Playing,
            wrap,
            rng,
        };
        game.reset();
        game
    }

    fn reset(&mut self) {
        self.snake.clear();
        self.occupied.iter_mut().for_each(|c| *c = false);
        self.dir = Dir::Right;
        self.pending = Dir::Right;
        self.score = 0;
        self.state = State::Playing;
        let y = self.h / 2;
        for i in 0..START_LEN.min(self.w) {
            let x = self.w / 4 + START_LEN - 1 - i;
            self.snake.push_back((x, y));
            self.occupied[y * self.w + x] = true;
        }
        self.place_food();
    }

    /// Put food on a uniformly chosen free cell. Picking an index among the
    /// free cells rather than retrying random cells keeps the cost bounded
    /// when the board is nearly full, which is exactly when a retry loop
    /// would spin longest.
    fn place_food(&mut self) {
        let free = self.occupied.iter().filter(|c| !**c).count();
        if free == 0 {
            self.state = State::Won;
            return;
        }
        let mut nth = self.rng.below(free);
        for (i, taken) in self.occupied.iter().enumerate() {
            if *taken {
                continue;
            }
            if nth == 0 {
                self.food = (i % self.w, i / self.w);
                return;
            }
            nth -= 1;
        }
    }

    fn turn(&mut self, dir: Dir) {
        if !dir.is_opposite(self.dir) {
            self.pending = dir;
        }
    }

    fn tick(&mut self) {
        if self.state != State::Playing {
            return;
        }
        self.dir = self.pending;
        let (dx, dy) = self.dir.delta();
        let (hx, hy) = *self.snake.front().expect("snake is never empty");
        let (nx, ny) = (hx as i32 + dx, hy as i32 + dy);

        let head = if self.wrap {
            (
                nx.rem_euclid(self.w as i32) as usize,
                ny.rem_euclid(self.h as i32) as usize,
            )
        } else if nx < 0 || ny < 0 || nx >= self.w as i32 || ny >= self.h as i32 {
            self.state = State::Lost;
            return;
        } else {
            (nx as usize, ny as usize)
        };

        // The tail cell is vacated this tick, so moving into it is legal.
        // Freeing it before the collision test is what makes that true.
        let ate = head == self.food;
        if !ate
            && let Some((tx, ty)) = self.snake.pop_back() {
                self.occupied[ty * self.w + tx] = false;
            }

        if self.occupied[head.1 * self.w + head.0] {
            self.state = State::Lost;
            return;
        }

        self.snake.push_front(head);
        self.occupied[head.1 * self.w + head.0] = true;

        if ate {
            self.score += 1;
            self.place_food();
        }
    }

    fn interval_ms(&self, base: u64) -> u64 {
        base.saturating_sub(self.score as u64 * SPEEDUP_MS)
            .max(base.div_ceil(2))
    }
}

fn clip(line: &str, cols: usize) -> String {
    line.chars().take(cols).collect()
}

fn draw(game: &Game, best: u32) {
    let cols = get_winsize(1).map(|(c, _)| c as usize).unwrap_or(80);
    let mut frame = String::with_capacity((game.w + 2) * (game.h + 2) * 8);
    frame.push_str("\x1b[H");

    let status = match game.state {
        State::Playing => format!("snake   score {}   best {}", game.score, best),
        State::Paused => format!("PAUSED   score {}   best {}", game.score, best),
        State::Lost => format!("GAME OVER   score {}   best {}", game.score, best),
        State::Won => format!("YOU WIN   score {}   best {}", game.score, best),
    };
    frame.push_str(&clip(&status, cols.saturating_sub(1)));
    frame.push_str("\x1b[K\r\n");

    let border: String = WALL.repeat(game.w + 2);
    frame.push_str(&border);
    frame.push_str("\x1b[K\r\n");

    let head = *game.snake.front().expect("snake is never empty");
    for y in 0..game.h {
        frame.push_str(WALL);
        for x in 0..game.w {
            let cell = if (x, y) == head {
                HEAD
            } else if game.occupied[y * game.w + x] {
                BODY
            } else if (x, y) == game.food && game.state != State::Won {
                FOOD
            } else {
                EMPTY
            };
            frame.push_str(cell);
        }
        frame.push_str(WALL);
        frame.push_str("\x1b[K\r\n");
    }

    frame.push_str(&border);
    frame.push_str("\x1b[K\r\n");

    let legend = match game.state {
        State::Playing | State::Paused => "arrows/wasd move   p pause   q quit",
        _ => "r play again   q quit",
    };
    frame.push_str(&clip(legend, cols.saturating_sub(1)));
    frame.push_str("\x1b[K");

    let out = stdout();
    let mut w = out.lock();
    let _ = w.write_all(frame.as_bytes());
    let _ = w.flush();
}

/// Give the terminal back: canonical mode, cursor on, off the last line.
fn restore() {
    pty_set_canonical(0);
    let out = stdout();
    let mut w = out.lock();
    let _ = write!(w, "\x1b[0m\x1b[?25h\r\n");
    let _ = w.flush();
}

fn usage() -> ! {
    eprintln!("usage: snake [-s MILLISECONDS] [-w]");
    eprintln!("  -s  milliseconds per move before the snake grows (default 130)");
    eprintln!("  -w  wrap around the walls instead of dying on them");
    exit(2)
}

struct Options {
    speed_ms: u64,
    wrap: bool,
}

fn parse_args() -> Options {
    let mut options = Options {
        speed_ms: DEFAULT_SPEED_MS,
        wrap: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            a if a.starts_with("-s") => {
                // `-s80` and `-s 80` are both what people type.
                let value = match a.strip_prefix("-s") {
                    Some("") | None => args.next().unwrap_or_else(|| usage()),
                    Some(rest) => rest.to_string(),
                };
                let ms: u64 = value.parse().unwrap_or_else(|_| usage());
                if ms < 10 {
                    usage();
                }
                options.speed_ms = ms;
            }
            "-w" | "--wrap" => options.wrap = true,
            _ => usage(),
        }
    }
    options
}

/// Act on one key. Returns false to quit.
fn handle_key(key: Key, game: &mut Game) -> bool {
    let dir = match key {
        Key::Dir(dir) => Some(dir),
        Key::Char(b'w') | Key::Char(b'k') => Some(Dir::Up),
        Key::Char(b's') | Key::Char(b'j') => Some(Dir::Down),
        Key::Char(b'a') | Key::Char(b'h') => Some(Dir::Left),
        Key::Char(b'd') | Key::Char(b'l') => Some(Dir::Right),
        Key::Char(b'q') | Key::Char(b'Q') | Key::Char(0x03) => return false,
        Key::Char(b'p') | Key::Char(b' ') => {
            game.state = match game.state {
                State::Playing => State::Paused,
                State::Paused => State::Playing,
                other => other,
            };
            None
        }
        Key::Char(b'r') => {
            if game.state != State::Playing {
                game.reset();
            }
            None
        }
        Key::Char(_) => None,
    };
    if let Some(dir) = dir {
        // A direction key is also how a paused game resumes; nothing else
        // would explain why the snake ignored it.
        if game.state == State::Paused {
            game.state = State::Playing;
        }
        game.turn(dir);
    }
    true
}

fn main() {
    let options = parse_args();

    // The game is a clock and a keyboard; neither exists without a terminal,
    // and cursor addressing into a pipe is noise.
    if !isatty(0) || !isatty(1) {
        eprintln!("snake: needs a terminal");
        exit(1);
    }

    let (cols, rows) = get_winsize(1).unwrap_or((80, 24));
    let w = (cols as usize).saturating_sub(1) / CELL_W;
    let w = w.saturating_sub(2);
    let h = (rows as usize).saturating_sub(CHROME_ROWS);
    if w < MIN_W || h < MIN_H {
        eprintln!(
            "snake: terminal too small, need at least {}x{}",
            (MIN_W + 2) * CELL_W + 1,
            MIN_H + CHROME_ROWS
        );
        exit(1);
    }

    let mut game = Game::new(w, h, options.wrap, Rng::new());
    let mut input = Input::default();
    let mut best = 0;

    pty_set_raw(0);
    let out = stdout();
    {
        let mut w = out.lock();
        let _ = write!(w, "\x1b[?25l\x1b[2J");
        let _ = w.flush();
    }

    let mut next_tick = Instant::now() + Duration::from_millis(options.speed_ms);
    loop {
        best = best.max(game.score);
        draw(&game, best);

        // Wait for whichever comes first, the tick or a key. The deadline
        // lives outside this loop on purpose: a key redraws the frame but must
        // not move the snake, and must not push the next move back either, or
        // holding a direction down would stall the game.
        loop {
            let now = Instant::now();
            let playing = game.state == State::Playing;
            if playing && now >= next_tick {
                break;
            }
            let wait = if playing {
                (next_tick.saturating_duration_since(now).as_millis() as u64).clamp(1, IDLE_WAIT_MS)
            } else {
                IDLE_WAIT_MS
            };
            if !input.fill(wait) {
                restore();
                return;
            }
            let mut acted = false;
            while let Some(key) = input.next_key() {
                if !handle_key(key, &mut game) {
                    restore();
                    return;
                }
                acted = true;
            }
            if acted {
                break;
            }
        }

        if game.state == State::Playing && Instant::now() >= next_tick {
            game.tick();
        }
        // A game that is not running has no clock, so its next move is always
        // one full interval after it starts running again.
        if game.state != State::Playing || Instant::now() >= next_tick {
            next_tick = Instant::now() + Duration::from_millis(game.interval_ms(options.speed_ms));
        }
    }
}
