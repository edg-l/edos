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
    #[allow(dead_code)]
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
        StealSkip {
            cpu: u32,
            tid: u64,
        },
        Enqueue {
            cpu: u32,
            tid: u64,
        },
        StateChange {
            tid: u64,
            old: u8,
            new: u8,
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
                TraceEvent::StealSkip { cpu, tid } => {
                    write!(f, "StealSkip cpu={cpu} tid={tid}")
                }
                TraceEvent::Enqueue { cpu, tid } => write!(f, "Enqueue cpu={cpu} tid={tid}"),
                TraceEvent::StateChange { tid, old, new } => {
                    write!(f, "State tid={tid} {old}->{new}")
                }
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

    /// Dump all trace buffers via raw serial port (bypasses SERIAL_DBG lock).
    /// Called from panic handler -- must not acquire any locks.
    pub fn dump_all_cpus() {
        static DUMPING: AtomicBool = AtomicBool::new(false);
        if DUMPING.swap(true, Ordering::Acquire) {
            return;
        }

        let mut port = unsafe { uart_16550::SerialPort::new(0x3F8) };

        use core::fmt::Write;
        let _ = writeln!(port, "\n=== TRACE DUMP ===");

        for cpu in 0..MAX_CPUS {
            let buf = &TRACE_BUFFERS[cpu];
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

            let _ = writeln!(port, "--- CPU {cpu} ({count} events, {total} total) ---");
            for i in 0..count {
                let idx = (start + i) % RING_SIZE;
                let event = unsafe { buf.slots[idx].assume_init_read() };
                let _ = writeln!(port, "  [{i:>3}] {event}");
            }
        }
        let _ = writeln!(port, "=== END TRACE ===\n");
    }
}

#[cfg(feature = "trace")]
pub use inner::*;
