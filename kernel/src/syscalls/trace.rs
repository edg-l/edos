//! Syscall tracing: what a process actually asked the kernel for.
//!
//! A traced thread writes an [`TRACE_ENTER`] record at the dispatch choke
//! point in [`super::syscall_handler`] and a [`TRACE_EXIT`] record when the
//! call returns; the tracer drains them through `trace_read`. There is no
//! stop-the-target machinery: the target never blocks on the tracer, so a
//! tracer that falls behind loses records and is told how many rather than
//! changing what the traced program does.
//!
//! Marks are generation-stamped. `Thread::traced` holds the generation it was
//! marked under and a thread is traced only while that matches [`TRACE_GEN`],
//! so releasing a trace session invalidates every outstanding mark with one
//! store instead of walking the thread table.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use edos_trace_abi::{
    ArgKind, TRACE_DIED, TRACE_ENTER, TRACE_EXIT, TRACE_STR_CAP, TRACE_STR_FAULT,
    TRACE_STR_TRUNCATED, TraceRecord, ctl,
};

use crate::{
    debug::lock_order::RANK_TRACE_RING,
    memory::vma::USER_VA_END,
    ranked_lock,
    syscalls::{Errno, SyscallContext, table},
    thread::{
        preempt::PreemptSpinlock,
        scheduler::{current_thread_id, current_thread_info},
        thread::{Thread, ThreadId, get_thread_by_id},
        waitqueue::WaitQueue,
    },
    util::uaccess::{
        UAccessError, try_copy_from_user, try_copy_string_from_user, try_copy_to_user,
    },
};

/// Records the ring holds. At 248 bytes each this is ~250 KiB, allocated when
/// a tracer claims the ring and freed when it lets go.
const RING_CAP: usize = 1024;

/// Records one `trace_read` may move in a single call, bounding both the
/// bounce buffer and how long the ring stays locked.
const MAX_READ_BATCH: usize = 256;

/// Longest a `trace_read` with no records waiting parks before returning 0.
const MAX_WAIT_MS: u64 = 1000;

struct Ring {
    buf: Vec<TraceRecord>,
    /// Index of the oldest record.
    tail: usize,
    len: usize,
}

impl Ring {
    /// Append `rec`, returning whether an older record had to be evicted to
    /// make room.
    ///
    /// A full ring normally refuses the new record, which keeps the trace a
    /// contiguous prefix of what the program did. `TRACE_DIED` is the one
    /// exception: it is what tells the tracer a thread is gone, and a tracer
    /// that never learns that waits for it forever. A terminator is not
    /// optional, so it evicts the oldest record rather than being dropped.
    fn push(&mut self, rec: TraceRecord) -> bool {
        if self.len == RING_CAP {
            if rec.kind != TRACE_DIED {
                return false;
            }
            self.tail = (self.tail + 1) % RING_CAP;
            self.len -= 1;
        }
        let head = (self.tail + self.len) % RING_CAP;
        self.buf[head] = rec;
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<TraceRecord> {
        if self.len == 0 {
            return None;
        }
        let rec = self.buf[self.tail];
        self.tail = (self.tail + 1) % RING_CAP;
        self.len -= 1;
        Some(rec)
    }
}

/// The ring, present only while a tracer holds the session.
static RING: PreemptSpinlock<Option<Ring>> = PreemptSpinlock::new(None);

/// Generation a mark must carry to count. Starts at 1 so 0 means "not marked",
/// and is bumped on both claim and release.
static TRACE_GEN: AtomicU64 = AtomicU64::new(1);

/// Thread holding the session, or 0.
static TRACER_TID: AtomicU64 = AtomicU64::new(0);

/// Records in the ring, published outside the lock so a parking reader can
/// test its wake condition without taking one.
static AVAILABLE: AtomicUsize = AtomicUsize::new(0);

/// Records dropped since the session was claimed because the ring was full.
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Set while the tracer is parked, so a writer only pays for a wake when
/// somebody is actually waiting on it.
static READER_WAITING: AtomicUsize = AtomicUsize::new(0);

static WAITQ: WaitQueue = WaitQueue::new();

/// The session recording `thread`'s syscalls, or `None`.
///
/// On the untraced path — every thread, almost always — this is one relaxed
/// load and a compare against a second one. The caller keeps the returned
/// generation and hands it back to [`emit`], which is what stops a record
/// authorised by one session from landing in the next one's ring.
pub fn traced_session(thread: &Thread) -> Option<u64> {
    let marked = thread.traced.load(Ordering::Relaxed);
    (marked != 0 && marked == TRACE_GEN.load(Ordering::Relaxed)).then_some(marked)
}

/// Whether `thread`'s syscalls are being recorded.
pub fn is_traced(thread: &Thread) -> bool {
    traced_session(thread).is_some()
}

/// The mark a thread created by `parent` starts with.
///
/// Tracing follows children by default: a process spawning a helper is exactly
/// the case where the trace would otherwise stop at the interesting point.
pub fn inherit_from(parent: &Thread) -> u64 {
    if is_traced(parent) {
        parent.traced.load(Ordering::Relaxed)
    } else {
        0
    }
}

fn now_ns() -> u64 {
    crate::timer::uptime_us().saturating_mul(1_000)
}

/// An argument register as a pointer into user memory, or `None`.
///
/// `try_copy_from_user` faults on a bad address but does not itself refuse a
/// kernel one, and the tracer copies from an address the caller chose into a
/// record the caller reads back. One compare closes that.
fn user_ptr(value: u64) -> Option<*const u8> {
    (value != 0 && value < USER_VA_END).then_some(value as *const u8)
}

/// Append a record authorised by session `generation` and wake the tracer if it
/// is waiting for one.
///
/// The generation is re-checked here, under the same lock that guards the ring,
/// because the decision to trace was taken at syscall entry and a blocking call
/// can outlive the session that authorised it. Without this a `read` that
/// started under one tracer delivers its return record to the next one, which
/// then holds a tid it never marked and waits forever for a death record that
/// will never come.
fn emit(generation: u64, rec: TraceRecord) {
    let mut guard = ranked_lock!(RANK_TRACE_RING, "trace::ring", RING);
    if TRACE_GEN.load(Ordering::Relaxed) != generation {
        return;
    }
    let Some(ring) = guard.as_mut() else {
        return;
    };
    if !ring.push(rec) {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // Published under the guard: a store after the lock is released can land
    // behind a drain that has already zeroed it.
    AVAILABLE.store(ring.len, Ordering::Release);
    drop(guard);

    if READER_WAITING.load(Ordering::Acquire) > 0 {
        WAITQ.wake_one();
    }
}

/// Copy the string arguments `info` declares into `rec`'s inline storage.
///
/// Both slots are filled in argument order, and a slot is spent even when the
/// copy faults, so the reader can pair the n-th captured string with the n-th
/// string argument without the kernel having to say which is which.
fn capture_strings(rec: &mut TraceRecord, info: &table::SyscallInfo, args: &[u64; 6]) {
    let mut slot = 0;
    let mut offset = 0;

    for (i, kind) in info.args.iter().enumerate() {
        if slot == rec.str_lens.len() {
            break;
        }
        if !kind.is_captured() {
            continue;
        }

        let cap = TRACE_STR_CAP - offset;
        if cap == 0 {
            // An earlier argument filled the record. Distinct from a fault:
            // the address was fine and there was simply nowhere to put it, and
            // a reader that showed a bare pointer here would imply otherwise.
            rec.str_lens[slot] = TRACE_STR_TRUNCATED;
            slot += 1;
            continue;
        }

        // SAFETY: `offset` is the running total of what earlier arguments wrote
        // and `cap` is `TRACE_STR_CAP - offset`, checked non-zero just above, so
        // the offset lands inside `rec.strs`.
        let dst = unsafe { rec.strs.as_mut_ptr().add(offset) };
        let Some(ptr) = user_ptr(args[i]) else {
            rec.str_lens[slot] = TRACE_STR_FAULT;
            slot += 1;
            continue;
        };

        let len = if *kind == ArgKind::StrLen {
            // The declared length is the argument after the pointer.
            let declared = args.get(i + 1).copied().unwrap_or(0) as usize;
            let want = declared.min(cap);
            if want == 0 {
                Ok(0)
            // SAFETY: `want` is clamped to `cap`, the space left in `rec.strs`
            // behind `dst`; `ptr` came from `user_ptr` and is checked again inside.
            } else if unsafe { try_copy_from_user(dst, ptr, want) } {
                Ok(want)
            } else {
                Err(UAccessError::Fault)
            }
        } else {
            // SAFETY: `cap` is exactly the space left behind `dst`, and that is the
            // most `try_copy_string_from_user` will write.
            match unsafe { try_copy_string_from_user(dst, ptr, cap) } {
                // No terminator within the space left: keep the prefix, which
                // `try_copy_string_from_user` has already written.
                Err(UAccessError::TooLong) => Ok(cap),
                other => other,
            }
        };

        match len {
            Ok(n) => {
                rec.str_lens[slot] = n as u16;
                offset += n;
            }
            Err(_) => rec.str_lens[slot] = TRACE_STR_FAULT,
        }
        slot += 1;
    }
}

/// What a traced syscall carries from its entry to its return.
///
/// The arguments are copied here rather than read back off `ctx` on the way
/// out, because a dispatch arm is not obliged to leave them alone: `sys_execve`
/// replaces the whole `SyscallContext` with the new image's, so on that path
/// the registers name an address space that no longer exists.
#[derive(Clone, Copy)]
pub struct TracedCall {
    pub tid: u64,
    pub generation: u64,
    pub nr: u64,
    pub args: [u64; 6],
}

impl TracedCall {
    pub fn new(tid: u64, generation: u64, ctx: &SyscallContext) -> Self {
        Self {
            tid,
            generation,
            nr: ctx.rax,
            args: [ctx.rdi, ctx.rsi, ctx.rdx, ctx.r10, ctx.r8, ctx.r9],
        }
    }
}

/// Record a syscall entry, including any string arguments it names.
pub fn record_enter(call: &TracedCall) {
    let mut rec = TraceRecord {
        tid: call.tid,
        time_ns: now_ns(),
        args: call.args,
        ret: 0,
        nr: call.nr as u32,
        kind: TRACE_ENTER,
        errno: 0,
        str_lens: [0; 2],
        strs: [0; TRACE_STR_CAP],
    };

    if let Some(info) = table::lookup(call.nr) {
        capture_strings(&mut rec, info, &call.args);
    }

    emit(call.generation, rec);
}

/// Record a syscall return, including whatever the call wrote into an output
/// buffer.
pub fn record_exit(call: &TracedCall, ret: u64) {
    let errno = current_thread_info().lock().errno as u32;
    let mut rec = TraceRecord {
        tid: call.tid,
        time_ns: now_ns(),
        ret,
        nr: call.nr as u32,
        kind: TRACE_EXIT,
        errno,
        ..TraceRecord::zeroed()
    };

    // A filled length only exists on a call that returned one; an error return
    // says nothing about how much of the buffer is initialised.
    if (ret as i64) > 0
        && let Some(info) = table::lookup(call.nr)
        && let Some(index) = info.args.iter().position(|kind| kind.is_out())
    {
        let want = (ret as usize).min(TRACE_STR_CAP);
        if let Some(ptr) = user_ptr(call.args[index]) {
            // SAFETY: `want` is clamped to `TRACE_STR_CAP`, the whole of
            // `rec.strs`, and nothing has written into it on this path.
            rec.str_lens[0] = if unsafe { try_copy_from_user(rec.strs.as_mut_ptr(), ptr, want) } {
                want as u16
            } else {
                TRACE_STR_FAULT
            };
        }
    }

    emit(call.generation, rec);
}

/// Called from `thread_exit` for every thread, traced or not.
///
/// Two jobs: a traced thread's death is itself an event the tracer wants, and
/// a tracer that dies without releasing would otherwise leave every target
/// writing into a ring nobody drains.
pub fn on_thread_exit(thread: &Thread, code: i32) {
    if let Some(generation) = traced_session(thread) {
        emit(
            generation,
            TraceRecord {
                tid: thread.id.0,
                time_ns: now_ns(),
                ret: code as i64 as u64,
                kind: TRACE_DIED,
                ..TraceRecord::zeroed()
            },
        );
    }

    if TRACER_TID.load(Ordering::Acquire) == thread.id.0 {
        end_session();
    }
}

/// Drop the ring and invalidate every outstanding mark.
///
/// Ownership and the generation both move under the ring lock, which is what
/// makes this mutually exclusive with `ctl::CLAIM`. Doing it outside would let
/// the next tracer install its ring in the window between clearing `TRACER_TID`
/// and taking the old one — and then this would take the *new* ring, leaving a
/// registered tracer that can never read and a session nobody can claim.
fn end_session() {
    let old = {
        let mut guard = ranked_lock!(RANK_TRACE_RING, "trace::end_session", RING);
        TRACER_TID.store(0, Ordering::Release);
        TRACE_GEN.fetch_add(1, Ordering::AcqRel);
        AVAILABLE.store(0, Ordering::Release);
        guard.take()
    };
    // Freed outside the lock: dropping ~250 KiB reaches the allocator, and the
    // ring lock is never held across another lock.
    drop(old);
    WAITQ.wake_all();
}

/// Mark or unmark one thread. `tid` of 0 means the caller.
fn set_mark(tid: u64, generation: u64) -> Result<u64, Errno> {
    let tid = if tid == 0 {
        current_thread_id().ok_or(Errno::EINVAL)?.0
    } else {
        tid
    };

    let Some(thread) = get_thread_by_id(ThreadId(tid)) else {
        return Err(Errno::ENOENT);
    };
    thread.traced.store(generation, Ordering::Relaxed);
    Ok(0)
}

pub fn sys_trace_ctl(op: u64, arg: u64) -> Result<u64, Errno> {
    let Some(caller) = current_thread_id() else {
        return Err(Errno::EINVAL);
    };

    match op {
        ctl::CLAIM => {
            // Allocated before the guard: it is ~250 KiB and the ring lock is
            // never held across the allocator.
            let mut buf = Vec::new();
            if buf.try_reserve_exact(RING_CAP).is_err() {
                return Err(Errno::ENOMEM);
            }
            buf.resize(RING_CAP, TraceRecord::zeroed());

            let mut guard = ranked_lock!(RANK_TRACE_RING, "trace::claim", RING);
            // A tracer that died released the session in `on_thread_exit`, so
            // a non-zero owner here is a live one. Taking ownership under the
            // ring lock is what makes this exclusive against `end_session`.
            if TRACER_TID
                .compare_exchange(0, caller.0, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                drop(guard);
                return Err(Errno::EBUSY);
            }

            DROPPED.store(0, Ordering::Relaxed);
            AVAILABLE.store(0, Ordering::Release);
            **guard = Some(Ring {
                buf,
                tail: 0,
                len: 0,
            });
            // Only now can marks from this session count: bumping the
            // generation with the ring already in place means no traced thread
            // ever finds a mark valid and no ring to write to.
            TRACE_GEN.fetch_add(1, Ordering::AcqRel);
            Ok(0)
        }
        ctl::RELEASE => {
            if TRACER_TID.load(Ordering::Acquire) != caller.0 {
                return Err(Errno::EPERM);
            }
            end_session();
            Ok(0)
        }
        ctl::MARK => {
            // Deliberately not restricted to the tracer: the useful case is a
            // freshly forked child marking itself before `execve`, which is
            // the only way to trace a program from its first instruction
            // without racing the tracer's attach.
            let owner = TRACER_TID.load(Ordering::Acquire);
            if owner == 0 {
                return Err(Errno::EPERM);
            }
            // A tracer that traces itself records two calls per `trace_read`,
            // so the ring never drains to empty and the trace is all tracer.
            if arg == owner || (arg == 0 && caller.0 == owner) {
                return Err(Errno::EINVAL);
            }
            set_mark(arg, TRACE_GEN.load(Ordering::Relaxed))
        }
        // Unmarking is the tracer's alone. Anything else would let an unrelated
        // process blind a live trace one thread at a time.
        ctl::UNMARK => {
            if TRACER_TID.load(Ordering::Acquire) != caller.0 {
                return Err(Errno::EPERM);
            }
            set_mark(arg, 0)
        }
        ctl::DROPPED => Ok(DROPPED.load(Ordering::Relaxed)),
        _ => Err(Errno::EINVAL),
    }
}

fn is_tracer_alive() -> bool {
    TRACER_TID.load(Ordering::Acquire) != 0
}

/// Drain up to `max` records into the caller's buffer, parking for up to
/// `timeout_ms` if none are waiting.
///
/// Returns the number of records written, which is 0 on timeout.
pub fn sys_trace_read(dst: *mut TraceRecord, max: u64, timeout_ms: u64) -> Result<u64, Errno> {
    if dst.is_null() || max == 0 {
        return Err(Errno::EINVAL);
    }
    let max = (max as usize).min(MAX_READ_BATCH);

    // The tracer's alone, for two reasons. Anyone else draining the ring
    // steals records the real tracer then never sees and `DROPPED` never
    // counts; and anyone else parking below enqueues on `WAITQ`, whose
    // `push_back` panics the kernel past `WAITQUEUE_CAP` waiters. With this
    // check the queue depth is one.
    let Some(caller) = current_thread_id() else {
        return Err(Errno::EINVAL);
    };
    if TRACER_TID.load(Ordering::Acquire) != caller.0 {
        return Err(Errno::EPERM);
    }

    if AVAILABLE.load(Ordering::Acquire) == 0 && timeout_ms > 0 {
        let timeout = core::time::Duration::from_millis(timeout_ms.min(MAX_WAIT_MS));
        READER_WAITING.fetch_add(1, Ordering::AcqRel);
        WAITQ.wait_until_timeout(
            || AVAILABLE.load(Ordering::Acquire) > 0 || !is_tracer_alive(),
            Some(timeout),
        );
        READER_WAITING.fetch_sub(1, Ordering::AcqRel);
    }

    let mut batch: Vec<TraceRecord> = Vec::new();
    if batch.try_reserve_exact(max).is_err() {
        return Err(Errno::ENOMEM);
    }

    {
        let mut guard = ranked_lock!(RANK_TRACE_RING, "trace::read", RING);
        let Some(ring) = guard.as_mut() else {
            return Err(Errno::EPERM);
        };
        while batch.len() < max {
            match ring.pop() {
                Some(rec) => batch.push(rec),
                None => break,
            }
        }
        AVAILABLE.store(ring.len, Ordering::Release);
    }

    if batch.is_empty() {
        return Ok(0);
    }

    let bytes = core::mem::size_of_val(batch.as_slice());
    // SAFETY: `bytes` is `size_of_val` of the batch's own slice, so the
    // source is valid for the length named.
    if !unsafe { try_copy_to_user(dst as *mut u8, batch.as_ptr() as *const u8, bytes) } {
        // The records are already out of the ring; a tracer that hands the
        // kernel an unwritable buffer loses them, which is its own doing.
        return Err(Errno::EFAULT);
    }

    Ok(batch.len() as u64)
}

/// `/proc/syscalls`: everything `strace` needs to turn a record back into a
/// call, so the formatter never carries its own copy of the syscall numbers,
/// the argument shapes, or the error names.
///
/// Two tagged record types: `call <nr> <name> <argkinds>` and
/// `errno <value> <name>`.
pub fn render_syscall_table() -> alloc::string::String {
    use core::fmt::Write;

    let mut out = alloc::string::String::new();
    for info in table::SYSCALLS {
        let _ = write!(out, "call {} {} ", info.nr, info.name);
        if info.args.is_empty() {
            out.push('-');
        } else {
            for kind in info.args {
                out.push(kind.as_char());
            }
        }
        out.push('\n');
    }
    for errno in crate::syscalls::ALL_ERRNOS {
        let _ = writeln!(out, "errno {} {}", *errno as u64, errno.name());
    }
    out
}
