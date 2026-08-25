//! Lightweight per-CPU event tracing for scheduler debugging.
//!
//! Lockless ring buffer that records scheduler events (context switches,
//! steals, saves, etc.). Gated behind `#[cfg(feature = "trace")]` so it
//! compiles to nothing in normal builds.
//!
//! # Safety
//!
//! Each CPU writes only to its own `TraceBuffer` (indexed by LAPIC ID).
//! No locks are needed: the `AtomicUsize` write head serializes writes
//! from the same CPU, and no cross-CPU writes occur.

/// Convenience macro for recording trace events. Compiles to nothing
/// without `--features trace`.
#[macro_export]
macro_rules! trace_event {
    ($variant:ident { $($field:ident : $val:expr),* $(,)? }) => {
        #[cfg(feature = "trace")]
        {
            $crate::util::trace::record(
                $crate::util::per_cpu::get_percpu_data().lapic_id.get(),
                $crate::util::trace::TraceEvent::$variant { $($field: $val),* },
            );
        }
    };
}

#[cfg(feature = "trace")]
mod inner {
    use core::mem::MaybeUninit;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    pub const MAX_CPUS: usize = 128;
    pub const RING_SIZE: usize = 256;

    #[derive(Clone, Copy)]
    pub enum TraceEvent {
        Switch {
            cpu: u32,
            from_tid: u64,
            to_tid: u64,
            to_rip: u64,
        },
        Save {
            cpu: u32,
            tid: u64,
            rip: u64,
        },
        Steal {
            thief_cpu: u32,
            victim_cpu: u32,
            tid: u64,
        },
        Rebalance {
            thief_cpu: u32,
            victim_cpu: u32,
            tid: u64,
        },
        Enqueue {
            cpu: u32,
            tid: u64,
        },
    }

    impl core::fmt::Display for TraceEvent {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                TraceEvent::Switch {
                    cpu,
                    from_tid,
                    to_tid,
                    to_rip,
                } => write!(f, "Switch cpu={cpu} {from_tid}->{to_tid} rip=0x{to_rip:x}"),
                TraceEvent::Save { cpu, tid, rip } => {
                    write!(f, "Save cpu={cpu} tid={tid} rip=0x{rip:x}")
                }
                TraceEvent::Steal {
                    thief_cpu,
                    victim_cpu,
                    tid,
                } => write!(f, "Steal {victim_cpu}->{thief_cpu} tid={tid}"),
                TraceEvent::Rebalance {
                    thief_cpu,
                    victim_cpu,
                    tid,
                } => write!(f, "Rebalance {victim_cpu}->{thief_cpu} tid={tid}"),
                TraceEvent::Enqueue { cpu, tid } => write!(f, "Enqueue cpu={cpu} tid={tid}"),
            }
        }
    }

    struct TraceBuffer {
        head: AtomicUsize,
        written: AtomicUsize,
        slots: [MaybeUninit<TraceEvent>; RING_SIZE],
    }

    // SAFETY: Each buffer is only written by its owning CPU. The dump function
    // reads during panic when other CPUs are halted.
    unsafe impl Sync for TraceBuffer {}

    impl TraceBuffer {
        const fn new() -> Self {
            Self {
                head: AtomicUsize::new(0),
                written: AtomicUsize::new(0),
                slots: [const { MaybeUninit::uninit() }; RING_SIZE],
            }
        }

        fn record(&self, event: TraceEvent) {
            let idx = self.head.fetch_add(1, Ordering::Relaxed) % RING_SIZE;
            // SAFETY: Single writer per buffer (the owning CPU).
            unsafe {
                let ptr = &self.slots[idx] as *const MaybeUninit<TraceEvent> as *mut TraceEvent;
                ptr.write(event);
            }
            self.written.fetch_add(1, Ordering::Relaxed);
        }
    }

    static TRACE_BUFFERS: [TraceBuffer; MAX_CPUS] = [const { TraceBuffer::new() }; MAX_CPUS];

    /// Record a trace event for the given CPU.
    #[inline]
    pub fn record(cpu_id: u32, event: TraceEvent) {
        let idx = (cpu_id as usize) % MAX_CPUS;
        TRACE_BUFFERS[idx].record(event);
    }

    /// Dump all trace buffers through `emergency_println!`, which writes the
    /// UART directly rather than through the `SERIAL_DBG` lock. Called from the
    /// panic handler -- must not acquire any locks.
    pub fn dump_all_cpus() {
        static DUMPING: AtomicBool = AtomicBool::new(false);
        if DUMPING.swap(true, Ordering::Acquire) {
            return;
        }

        use crate::emergency_println;

        emergency_println!("\n=== TRACE DUMP ===");

        for (cpu, buf) in TRACE_BUFFERS.iter().enumerate().take(MAX_CPUS) {
            let total = buf.written.load(Ordering::Relaxed);
            if total == 0 {
                continue;
            }

            let count = total.min(RING_SIZE);
            let start = if total >= RING_SIZE {
                buf.head.load(Ordering::Relaxed) % RING_SIZE
            } else {
                0
            };

            emergency_println!("--- CPU {cpu} ({count} events, {total} total) ---");
            for i in 0..count {
                let idx = (start + i) % RING_SIZE;
                // SAFETY: `written` counts recorded events, so the first
                // `count` slots reachable from `start` are initialised.
                let event = unsafe { buf.slots[idx].assume_init_read() };
                emergency_println!("  [{i:>3}] {event}");
            }
        }
        emergency_println!("=== END TRACE ===\n");
    }
}

#[cfg(feature = "trace")]
pub use inner::*;
