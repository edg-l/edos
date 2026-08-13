//! Frame telemetry: what the compositor actually delivers, as numbers.
//!
//! "It feels slow" and "it is slow" are different claims, and only one can be
//! acted on. Turning the first into the second needs three things this module
//! provides and the counter it replaces did not.
//!
//! **Intervals, not just work.** Smoothness is the gap between one presented
//! frame and the next. A loop that composites in 1ms and then stalls 200ms
//! somewhere else looks perfect to a counter that times only the compositing,
//! and looks terrible to a human. The interval is therefore the headline
//! number, and the phases are recorded to say where a bad one went.
//!
//! **Percentiles, not a mean.** One 2-second startup frame in a 57-frame
//! window drags the mean to 37ms and hides that the other 56 were fine. A
//! median says what a typical frame did; a p99 says what the worst ones did;
//! the mean of the two populations together says nothing about either.
//!
//! **Always recording.** A counter that reports only once something is already
//! over budget cannot show that a change made things better, which is the whole
//! purpose of having it.

use std::{
    fmt::Write as _,
    fs,
    io::Write as _,
    time::{Duration, Instant},
};

/// How many frames of history each distribution keeps. At 74Hz this is a
/// little over three seconds, so a one-second report always has a full window
/// and the sort at report time costs nothing worth measuring.
const WINDOW: usize = 256;

/// Where the current numbers are left for anyone in the guest to read.
pub const STATS_PATH: &str = "/tmp/wm-frames";

/// Turns the per-second line to the kernel log on. Off by default: one line a
/// second would push everything else out of a ring buffer that other
/// subsystems also have to share.
pub const LOG_SETTING: &str = "/etc/wm-metrics";

/// A frame this far over budget is reported even when logging is off, since a
/// stall nobody asked to see is exactly the thing worth knowing about.
const STALL_MULTIPLE: u64 = 4;

/// A fixed-size ring of microsecond samples.
struct Samples {
    values: [u32; WINDOW],
    len: usize,
    next: usize,
}

impl Samples {
    fn new() -> Self {
        Samples {
            values: [0; WINDOW],
            len: 0,
            next: 0,
        }
    }

    fn push(&mut self, micros: u64) {
        self.values[self.next] = micros.min(u32::MAX as u64) as u32;
        self.next = (self.next + 1) % WINDOW;
        self.len = (self.len + 1).min(WINDOW);
    }

    /// Median, 95th, 99th and maximum, in one sort.
    fn summary(&self) -> Quantiles {
        if self.len == 0 {
            return Quantiles::default();
        }
        let mut sorted = self.values[..self.len].to_vec();
        sorted.sort_unstable();
        Quantiles {
            p50: sorted[self.len / 2],
            p95: sorted[self.len * 95 / 100],
            p99: sorted[self.len * 99 / 100],
            max: sorted[self.len - 1],
        }
    }
}

#[derive(Default, Clone, Copy)]
pub struct Quantiles {
    pub p50: u32,
    pub p95: u32,
    pub p99: u32,
    pub max: u32,
}

/// One frame's measurements.
pub struct Frame {
    /// Since the previous frame began: the cadence a viewer perceives.
    pub interval_us: u64,
    /// Reading input and the window list, before any drawing.
    pub input_us: u64,
    pub composite_us: u64,
    /// The transfer to the display, which is an ioctl and can block.
    pub flip_us: u64,
    /// Pixels handed to the display.
    pub sent_pixels: u64,
    pub full_screen: bool,
    /// Nothing changed, so the composite produced the image already on screen.
    pub idle_repaint: bool,
    /// The pointer moved or a drag was in progress, which is when smoothness
    /// is judged. Reported separately because an idle desktop and a window
    /// being dragged are different workloads and averaging them hides both.
    pub in_motion: bool,
}

pub struct FrameLog {
    interval: Samples,
    input: Samples,
    composite: Samples,
    flip: Samples,
    /// Intervals recorded only while something was moving.
    motion_interval: Samples,

    frames: u32,
    motion_frames: u32,
    idle_repaints: u32,
    full_screens: u32,
    stalls: u32,
    sent_pixels: u64,

    budget_us: u64,
    to_log: bool,
    last_report: Instant,
    started: Instant,
}

impl FrameLog {
    pub fn new(budget_ms: u64) -> Self {
        let to_log = edos_lib::config::read(LOG_SETTING)
            .map(|v| v == "on" || v == "1" || v == "true")
            .unwrap_or(false);

        FrameLog {
            interval: Samples::new(),
            input: Samples::new(),
            composite: Samples::new(),
            flip: Samples::new(),
            motion_interval: Samples::new(),
            frames: 0,
            motion_frames: 0,
            idle_repaints: 0,
            full_screens: 0,
            stalls: 0,
            sent_pixels: 0,
            budget_us: budget_ms * 1000,
            to_log,
            last_report: Instant::now(),
            started: Instant::now(),
        }
    }

    pub fn record(&mut self, frame: &Frame) {
        self.interval.push(frame.interval_us);
        self.input.push(frame.input_us);
        self.composite.push(frame.composite_us);
        self.flip.push(frame.flip_us);
        if frame.in_motion {
            self.motion_interval.push(frame.interval_us);
            self.motion_frames += 1;
        }

        self.frames += 1;
        self.sent_pixels += frame.sent_pixels;
        if frame.idle_repaint {
            self.idle_repaints += 1;
        }
        if frame.full_screen {
            self.full_screens += 1;
        }

        if frame.interval_us > self.budget_us * STALL_MULTIPLE {
            self.stalls += 1;
            // Named immediately rather than waited for, because the report is
            // an average of a second and a stall is a single event.
            klog(&format!(
                "wm: stall {}us (input {} composite {} flip {})",
                frame.interval_us, frame.input_us, frame.composite_us, frame.flip_us
            ));
        }
    }

    /// Emit the window's numbers if a second has passed. Cheap enough to call
    /// every frame.
    pub fn maybe_report(&mut self) {
        let now = Instant::now();
        let span = now.duration_since(self.last_report);
        if span < Duration::from_secs(1) {
            return;
        }
        self.last_report = now;

        let report = self.render(span);
        // memfs, rewritten in place, so this never grows and costs no disk.
        let _ = fs::write(STATS_PATH, &report);
        if self.to_log {
            klog(&self.one_line(span));
        }

        self.frames = 0;
        self.motion_frames = 0;
        self.idle_repaints = 0;
        self.full_screens = 0;
        self.stalls = 0;
        self.sent_pixels = 0;
    }

    /// The one line that goes to the kernel log, and so to the host's serial
    /// capture, where it can be read without touching the guest.
    fn one_line(&self, span: Duration) -> String {
        let interval = self.interval.summary();
        let motion = self.motion_interval.summary();
        let composite = self.composite.summary();
        let flip = self.flip.summary();
        let input = self.input.summary();

        format!(
            "wmfps fps={:.1} int_p50={} int_p95={} int_max={} motion_p50={} motion_p95={} \
             comp_p50={} comp_p95={} flip_p50={} flip_p95={} flip_max={} in_p50={} \
             idle_repaint={}/{} full={} stalls={} KiBs={}",
            self.fps(span),
            interval.p50,
            interval.p95,
            interval.max,
            motion.p50,
            motion.p95,
            composite.p50,
            composite.p95,
            flip.p50,
            flip.p95,
            flip.max,
            input.p50,
            self.idle_repaints,
            self.frames,
            self.full_screens,
            self.stalls,
            self.sent_pixels * 4 / 1024,
        )
    }

    /// The readable form, for `cat /tmp/wm-frames` in the guest.
    fn render(&self, span: Duration) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "uptime      {:.1}s",
            self.started.elapsed().as_secs_f64()
        );
        let _ = writeln!(
            out,
            "frames      {} in {:.2}s = {:.1} fps",
            self.frames,
            span.as_secs_f64(),
            self.fps(span)
        );
        let _ = writeln!(out, "budget      {}us per frame", self.budget_us);
        let _ = writeln!(out);
        let _ = writeln!(out, "                p50      p95      p99      max");
        row(&mut out, "interval", self.interval.summary());
        row(&mut out, " in motion", self.motion_interval.summary());
        row(&mut out, "input", self.input.summary());
        row(&mut out, "composite", self.composite.summary());
        row(&mut out, "flip", self.flip.summary());
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "motion      {} of {} frames",
            self.motion_frames, self.frames
        );
        let _ = writeln!(
            out,
            "idle repaint {} of {} frames composited with nothing changed",
            self.idle_repaints, self.frames
        );
        let _ = writeln!(out, "full screen {} frames", self.full_screens);
        let _ = writeln!(
            out,
            "stalls      {} frames over {}us",
            self.stalls,
            self.budget_us * STALL_MULTIPLE
        );
        let _ = writeln!(
            out,
            "to display  {} KiB/s",
            (self.sent_pixels * 4 / 1024) as f64 / span.as_secs_f64().max(0.001)
        );
        out
    }

    fn fps(&self, span: Duration) -> f64 {
        self.frames as f64 / span.as_secs_f64().max(0.001)
    }
}

fn row(out: &mut String, name: &str, q: Quantiles) {
    let _ = writeln!(
        out,
        "{:<12}{:>8} {:>8} {:>8} {:>8}",
        name, q.p50, q.p95, q.p99, q.max
    );
}

/// Leave a line where the serial log will carry it.
fn klog(message: &str) {
    if let Ok(mut klog) = fs::OpenOptions::new().write(true).open("/dev/klog") {
        let _ = klog.write_all(message.as_bytes());
    }
}
