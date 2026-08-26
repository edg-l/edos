use core::{
    arch::x86_64::_rdtsc,
    ops::Add,
    sync::atomic::{AtomicBool, AtomicI64, Ordering},
    time::Duration,
};

use raw_cpuid::CpuId;
use x86_64::instructions::port::Port;

use crate::{
    apic::get_lapic,
    drivers::hpet::{driver::get_hpet_timer, instant::HpetTimer},
    println,
};

// The HPET is the only wall clock. The PIT is read once, to calibrate the
// APIC timer against, and never again; a machine without an HPET reports a
// monotonic clock stuck at zero rather than falling back to it.

pub struct TimerCalibration {
    pub ticks_per_microsecond: u64,
}

impl TimerCalibration {
    /// Calibrate APIC timer frequency using PIT as reference
    pub fn calibrate_apic_timer() -> TimerCalibration {
        const PIT_FREQUENCY: u64 = 1193182; // Hz
        const CALIBRATION_MS: u64 = 35; // Calibrate for 35ms
        const PIT_TICKS_FOR_CALIBRATION: u16 = (PIT_FREQUENCY * CALIBRATION_MS / 1000) as u16;

        println!("Calibrating APIC timer frequency...");

        // 1. Setup PIT for one-shot mode
        unsafe {
            Self::setup_pit_oneshot(PIT_TICKS_FOR_CALIBRATION);
        }

        // 2. Start APIC timer with maximum count
        let apic_start_count = 0xFFFFFFFF;
        let tsc_start = unsafe { core::arch::x86_64::_rdtsc() };
        unsafe {
            let lapic = get_lapic();
            lapic.set_timer_mode(x2apic::lapic::TimerMode::OneShot);
            lapic.set_timer_divide(x2apic::lapic::TimerDivide::Div1); // No division for calibration
            lapic.set_timer_initial(apic_start_count);
        }

        // 3. Wait for PIT to finish (busy wait is fine for calibration)
        Self::wait_for_pit_completion();

        // 4. Read APIC timer current count
        let apic_end_count = unsafe { get_lapic().timer_current() };
        let tsc_end = unsafe { core::arch::x86_64::_rdtsc() };

        // 5. Calculate APIC frequency
        let apic_ticks_elapsed = apic_start_count - apic_end_count;
        let tsc_ticks_elapsed = tsc_end - tsc_start;
        let time_elapsed_us = CALIBRATION_MS * 1000; // microseconds

        // APIC frequency = ticks_elapsed / time_elapsed_seconds
        let apic_frequency_hz = (apic_ticks_elapsed as u64 * 1_000) / (time_elapsed_us / 1000);
        let tsc_frequency_hz = (tsc_ticks_elapsed * 1_000) / (time_elapsed_us / 1000);
        let ticks_per_microsecond = apic_ticks_elapsed as u64 / time_elapsed_us;

        println!("APIC Timer Calibration Results:");
        println!("  Calibration time: {}ms", CALIBRATION_MS);
        println!("  APIC ticks elapsed: {}", apic_ticks_elapsed);
        println!(
            "  APIC frequency: {} Hz ({}) MHz",
            apic_frequency_hz,
            apic_frequency_hz / 1_000_000
        );
        println!(
            "  TSC frequency: {} Hz ({:.2}) GHz",
            tsc_frequency_hz,
            tsc_frequency_hz as f64 / 1_000_000_000.0
        );
        println!("  Ticks per microsecond: {}", ticks_per_microsecond);

        TimerCalibration {
            ticks_per_microsecond,
        }
    }

    #[inline(always)]
    unsafe fn setup_pit_oneshot(ticks: u16) {
        unsafe {
            // PIT Command: Channel 0, Lo/Hi byte, Mode 0 (interrupt on terminal count), Binary
            Port::new(0x43).write(0b00110000u8); // Command register

            // Set count (LSB then MSB)
            Port::new(0x40).write((ticks & 0xFF) as u8); // LSB
            Port::new(0x40).write(((ticks >> 8) & 0xFF) as u8); // MSB
        }
    }

    #[inline(always)]
    fn wait_for_pit_completion() {
        // Poll PIT status until count reaches zero
        loop {
            unsafe {
                // Read-back command: Channel 0, latch count
                Port::new(0x43).write(0b11100010u8);

                // Read status
                let status: u8 = Port::new(0x40).read();

                // Bit 7 = output pin state (goes high when count reaches 0)
                if status & 0x80 != 0 {
                    break;
                }
            }

            core::hint::spin_loop();
        }
    }
}

// Global calibration result
static TIMER_CALIBRATION: spin::Once<TimerCalibration> = spin::Once::new();

pub fn get_timer_calibration() -> &'static TimerCalibration {
    TIMER_CALIBRATION.call_once(TimerCalibration::calibrate_apic_timer)
}

// ---------------------------------------------------------------------------
// Monotonic clock
// ---------------------------------------------------------------------------

/// Factors that turn a TSC reading into nanoseconds on the HPET's timeline.
///
/// `base_tsc` and `base_nanos` are read at the same moment, so the two
/// timelines meet there and an `Instant` taken before the switch stays
/// comparable with one taken after.
struct TscClock {
    base_tsc: u64,
    base_nanos: u64,
    /// `nanos = (ticks * mult) >> TSC_SHIFT`.
    mult: u64,
}

/// Fractional bits in `TscClock::mult`. The product is computed in 128 bits, so
/// a delta spanning the whole uptime cannot overflow, and 32 bits of fraction
/// is far finer than the calibration itself resolves.
const TSC_SHIFT: u32 = 32;

/// How far a CPU's TSC may sit from the HPET at bring-up before the TSC is
/// abandoned. Generous next to the microsecond a pair of HPET reads costs, and
/// far below the offset an unsynchronised counter shows.
const MAX_TSC_SKEW_NS: u64 = 1_000_000;

static TSC_CLOCK: spin::Once<TscClock> = spin::Once::new();

/// Whether [`Instant::now`] reads the TSC.
///
/// Cleared if a CPU brought up later disagrees with the HPET, which demotes
/// every CPU at once: a clock that is only monotonic on some cores is worse
/// than a slow one.
static TSC_ACTIVE: AtomicBool = AtomicBool::new(false);

impl TscClock {
    #[inline]
    fn now(&self) -> u64 {
        let delta = unsafe { _rdtsc() }.wrapping_sub(self.base_tsc);
        self.base_nanos
            .wrapping_add(((delta as u128 * self.mult as u128) >> TSC_SHIFT) as u64)
    }
}

/// Nanoseconds since the HPET started counting, or 0 before it exists.
fn hpet_nanos() -> u64 {
    match get_hpet_timer() {
        Some(hpet) => hpet.ticks_to_nanos(hpet.get_counter()),
        None => 0,
    }
}

#[inline]
fn monotonic_nanos() -> u64 {
    if TSC_ACTIVE.load(Ordering::Relaxed)
        && let Some(clock) = TSC_CLOCK.get()
    {
        return clock.now();
    }
    hpet_nanos()
}

/// Whether the processor guarantees the TSC counts at a fixed rate.
///
/// `CPUID.80000007H:EDX[8]` promises the TSC runs at a constant rate across
/// every ACPI P-, C- and T-state, which is what makes it a clock rather than a
/// cycle counter: without it the rate follows the core frequency, and turbo
/// alone would make every duration wrong.
fn invariant_tsc() -> bool {
    CpuId::new()
        .get_advanced_power_mgmt_info()
        .is_some_and(|apm| apm.has_invariant_tsc())
}

/// TSC frequency in Hz, measured against the HPET, or `None` if neither
/// counter advanced.
fn calibrate_tsc(hpet: &HpetTimer) -> Option<u64> {
    const WINDOW_NS: u64 = 10_000_000;

    let start_hpet = hpet.get_counter();
    let start_tsc = unsafe { _rdtsc() };
    loop {
        let elapsed = hpet.ticks_to_nanos(hpet.get_counter().wrapping_sub(start_hpet));
        if elapsed >= WINDOW_NS {
            break;
        }
        core::hint::spin_loop();
    }
    let end_tsc = unsafe { _rdtsc() };
    let elapsed_ns = hpet.ticks_to_nanos(hpet.get_counter().wrapping_sub(start_hpet));

    let ticks = end_tsc.wrapping_sub(start_tsc);
    if ticks == 0 || elapsed_ns == 0 {
        return None;
    }
    Some((ticks as u128 * 1_000_000_000 / elapsed_ns as u128) as u64)
}

/// Move the monotonic clock from the HPET to the TSC where the processor
/// allows it.
///
/// Reading the HPET main counter is an uncached MMIO access, and under an
/// emulated HPET it is a full exit to the hypervisor: measured at 6.4 us a
/// read, against about ten nanoseconds for `rdtsc`. Every timestamp the kernel
/// takes pays that, including one on each side of a context switch.
///
/// Runs after `drivers::hpet::driver::init` and before anything records an
/// `Instant`, and leaves the clock on the HPET when the TSC cannot be trusted.
pub fn init_monotonic_clock() {
    // Read straight out of the raw command line: this runs before the cmdline
    // is parsed, and parsing allocates.
    if crate::boot::boot_info()
        .cmdline
        .contains("clocksource=hpet")
    {
        println!("clocksource=hpet requested; monotonic clock stays on the HPET");
        return;
    }

    if !invariant_tsc() {
        println!("TSC is not invariant; monotonic clock stays on the HPET");
        return;
    }

    let Some(hpet) = get_hpet_timer() else {
        println!("No HPET to calibrate against; monotonic clock stays on the HPET");
        return;
    };

    let Some(freq) = calibrate_tsc(hpet) else {
        println!("TSC calibration measured no elapsed time; clock stays on the HPET");
        return;
    };

    let mult = ((1_000_000_000u128 << TSC_SHIFT) / freq as u128) as u64;
    TSC_CLOCK.call_once(|| TscClock {
        base_nanos: hpet_nanos(),
        base_tsc: unsafe { _rdtsc() },
        mult,
    });
    TSC_ACTIVE.store(true, Ordering::Release);

    println!(
        "Monotonic clock on the TSC at {}.{:03} GHz",
        freq / 1_000_000_000,
        (freq / 1_000_000) % 1_000
    );
}

/// Confirm the calling CPU's TSC agrees with the HPET, and demote the whole
/// system to the HPET if it does not.
///
/// Invariant TSC is a guarantee about one package's counting *rate*. Whether
/// two packages started counting together is firmware's job, and a CPU whose
/// TSC is offset would make the clock jump every time a thread migrated onto
/// it. This runs at bring-up, milliseconds after calibration, so the only
/// difference it can see is a genuine offset rather than accumulated drift.
pub fn verify_tsc_sync(cpu: u32) {
    if !TSC_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    let Some(clock) = TSC_CLOCK.get() else {
        return;
    };

    let skew = clock.now().abs_diff(hpet_nanos());
    if skew > MAX_TSC_SKEW_NS {
        TSC_ACTIVE.store(false, Ordering::Release);
        println!(
            "cpu {cpu}: TSC is {} us off the HPET; monotonic clock falls back to the HPET",
            skew / 1_000
        );
    }
}

/// A point on the monotonic clock, in nanoseconds since the counter started.
///
/// Nanoseconds rather than counter ticks because the source can change: the
/// clock starts on the HPET, moves to the TSC once the processor is known to
/// support it, and moves back if a CPU disagrees. A raw tick would mean
/// something different either side of those switches, and the values outlive
/// them in atomics like a sleep deadline or a thread's run start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant {
    nanos: u64,
}

impl Instant {
    pub fn now() -> Self {
        Self {
            nanos: monotonic_nanos(),
        }
    }

    pub const fn as_nanos(&self) -> u64 {
        self.nanos
    }

    pub const fn from_nanos(nanos: u64) -> Self {
        Self { nanos }
    }

    pub fn elapsed(&self) -> Duration {
        Self::now().duration_since(*self)
    }

    pub fn duration_since(&self, earlier: Instant) -> Duration {
        Duration::from_nanos(self.nanos.saturating_sub(earlier.nanos))
    }

    /// Time from `earlier` to `self`, or `None` when `self` is not later.
    ///
    /// [`Self::duration_since`] saturates to zero instead, which cannot tell a
    /// deadline that has just arrived from one that passed long ago. Anything
    /// computing the time left on a deadline needs that difference, since zero
    /// remaining is a timeout and not an unbounded wait.
    pub fn checked_duration_since(&self, earlier: Instant) -> Option<Duration> {
        self.nanos
            .checked_sub(earlier.nanos)
            .map(Duration::from_nanos)
    }
}

impl Add<Duration> for Instant {
    type Output = Self;
    fn add(self, rhs: Duration) -> Self::Output {
        Self {
            nanos: self.nanos.saturating_add(rhs.as_nanos() as u64),
        }
    }
}

/// Boot time reference point
static BOOT_TIME: spin::Once<Instant> = spin::Once::new();

/// Initialize the boot time reference
pub fn init_boot_time() {
    BOOT_TIME.call_once(Instant::now);
}

/// Get time elapsed since boot in microseconds
pub fn uptime_us() -> u64 {
    if let Some(boot_instant) = BOOT_TIME.get() {
        boot_instant.elapsed().as_micros() as u64
    } else {
        0
    }
}

/// The wall clock, pinned to the monotonic counter at a single point in time.
///
/// The RTC is a slow device — several port round-trips per read, each a VM exit
/// under KVM — and it has no sub-second resolution, so it is read exactly once
/// and every later answer is that reading plus HPET ticks.
struct WallClockRef {
    /// Nanoseconds since the Unix epoch at `at`.
    epoch_nanos: u64,
    at: Instant,
}

static WALL_CLOCK: spin::Once<WallClockRef> = spin::Once::new();

/// Correction added to the pinned RTC reading, in nanoseconds.
///
/// The RTC is sampled once and has one-second resolution, so the boot reading
/// is wrong by up to a second before the HPET's own error is counted. A time
/// client corrects it by storing the difference here rather than re-pinning,
/// which leaves the reference point and the monotonic counter alone.
static WALL_CLOCK_OFFSET_NS: AtomicI64 = AtomicI64::new(0);

/// Days from 1970-01-01 to a proleptic Gregorian date, for `m` in `[1, 12]`.
///
/// Howard Hinnant's `days_from_civil`:
/// <https://howardhinnant.github.io/date_algorithms.html#days_from_civil>
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Sample the RTC once and pin it to the monotonic counter.
///
/// Requires the HPET, so it runs after `drivers::hpet::driver::init`. The RTC is
/// read as UTC, which is what QEMU presents by default; a machine whose RTC
/// holds local time reports a wall clock offset by its timezone.
pub fn init_wall_clock() {
    WALL_CLOCK.call_once(|| {
        let rtc = crate::drivers::rtc::read_rtc();
        let days = days_from_civil(rtc.year as i64, rtc.month as i64, rtc.day as i64);
        let secs =
            days * 86_400 + rtc.hour as i64 * 3_600 + rtc.minute as i64 * 60 + rtc.second as i64;
        WallClockRef {
            epoch_nanos: secs.max(0) as u64 * 1_000_000_000,
            at: Instant::now(),
        }
    });
}

/// Nanoseconds since the Unix epoch, or `None` before the RTC has been sampled.
pub fn wall_clock_nanos() -> Option<u64> {
    let reference = WALL_CLOCK.get()?;
    let elapsed = Instant::now().duration_since(reference.at).as_nanos() as u64;
    let pinned = reference.epoch_nanos.saturating_add(elapsed);
    Some(pinned.saturating_add_signed(WALL_CLOCK_OFFSET_NS.load(Ordering::Relaxed)))
}

/// Step the wall clock so that it reads `epoch_nanos` now.
///
/// Returns `false` before the RTC has been sampled, since there is no reference
/// to correct. The step is not slewed: a reader either side of it sees the jump,
/// which is why the monotonic counter and not this is what durations are measured
/// against.
pub fn set_wall_clock_nanos(epoch_nanos: u64) -> bool {
    let Some(reference) = WALL_CLOCK.get() else {
        return false;
    };
    let elapsed = Instant::now().duration_since(reference.at).as_nanos() as u64;
    let pinned = reference.epoch_nanos.saturating_add(elapsed);
    WALL_CLOCK_OFFSET_NS.store(epoch_nanos as i64 - pinned as i64, Ordering::Relaxed);
    true
}
