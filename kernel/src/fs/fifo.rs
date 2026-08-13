//! Named pipes.
//!
//! A FIFO is the [`Pipe`] that already exists plus a name in the filesystem.
//! The filesystem stores only the name and its type; the buffer the two ends
//! meet in lives here, keyed by the inode the name resolves to, and exists for
//! as long as either end has it open.
//!
//! What the name buys is a channel between two programs with no common parent,
//! which is otherwise not expressible: an anonymous pipe can only be inherited
//! across `spawn`. It is also what makes `mkfifo f; prog > f & other < f` work
//! in a shell.
//!
//! # The rendezvous
//!
//! The interesting semantics are in `open`, not in the transfer. Opening for
//! reading blocks until a writer arrives and opening for writing blocks until a
//! reader does, so that neither end starts before there is anything on the
//! other side of it. `O_NONBLOCK` turns the reader's wait into an immediate
//! success and the writer's into `ENXIO`, as POSIX.1-2024 specifies.
//!
//! `O_NONBLOCK` is recorded on the descriptor the open returns, so it governs
//! the transfer as well: a `read` with an empty pipe still open at the other
//! end, or a `write` with no room, fails with `EAGAIN` rather than parking.
//! `F_SETFL` changes it afterwards. The open is the half this module decides;
//! the rest is in `sys_read` and `sys_write`.
//!
//! A waiter is waiting for its peer to *have arrived*, not to still be there: a
//! peer that opened and closed while the waiter was asleep has satisfied the
//! rendezvous, and restarting the wait on that would hang an open that a
//! complete transfer had already happened behind. `reader_seen` / `writer_seen`
//! are what say so.

use alloc::{collections::btree_map::BTreeMap, sync::Arc};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::{
    debug::lock_order::{RANK_FIFO_REGISTRY, RANK_PIPE},
    ranked_lock,
    syscalls::Errno,
    thread::{
        mutex::BlockingMutex,
        pipe::{FileDescriptor, OpenMode, Pipe},
        preempt::PreemptSpinlock,
        waitqueue::{WaitOutcome, WaitQueue},
    },
};

/// Which name a FIFO's buffer belongs to: the mount it lives on and its inode
/// number there. A path would be wrong, since a rename moves the name and not
/// the channel.
pub type FifoKey = (usize, u64);

/// One incarnation of a named pipe: the buffer, and the state that says whether
/// each end has ever been opened during it.
struct Fifo {
    pipe: Arc<BlockingMutex<Pipe>>,
    /// Wakes an `open` that is waiting for its peer to arrive.
    open_wq: Arc<WaitQueue>,
    reader_seen: AtomicBool,
    writer_seen: AtomicBool,
}

impl Fifo {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            pipe: Arc::new(BlockingMutex::new(Pipe::new_fifo())),
            open_wq: Arc::new(WaitQueue::new()),
            reader_seen: AtomicBool::new(false),
            writer_seen: AtomicBool::new(false),
        })
    }
}

/// Every named pipe with an end open, plus the ones whose last end has closed
/// but whose name has not been reused yet.
///
/// Entries are small and bounded by the number of FIFO inodes that have ever
/// been opened; `forget` drops one when its name goes away.
static FIFOS: PreemptSpinlock<BTreeMap<FifoKey, Arc<Fifo>>> = PreemptSpinlock::new(BTreeMap::new());

/// The FIFO for `key`, starting a fresh incarnation if the last one has no ends
/// left open.
///
/// A FIFO that everyone has closed keeps nothing: POSIX has the data go with
/// the last close, and a reader that arrives afterwards must not be handed the
/// end-of-file the previous incarnation ended on.
fn incarnation(key: FifoKey) -> Arc<Fifo> {
    let mut registry = ranked_lock!(RANK_FIFO_REGISTRY, "fifo::incarnation", FIFOS);
    if let Some(existing) = registry.get(&key) {
        let idle = {
            let pipe = ranked_lock!(RANK_PIPE, "fifo::incarnation", existing.pipe);
            pipe.readers == 0 && pipe.writers == 0
        };
        if !idle {
            return existing.clone();
        }
    }
    let fresh = Fifo::new();
    registry.insert(key, fresh.clone());
    fresh
}

/// Drop the buffer bound to `key`, because the name it belonged to is gone.
///
/// Descriptors already open keep working: they hold the `Arc<Pipe>` directly,
/// so unlinking a FIFO out from under a transfer does not interrupt it. This
/// only stops a later inode with the same number inheriting a stranger's
/// buffer.
pub fn forget(key: FifoKey) {
    ranked_lock!(RANK_FIFO_REGISTRY, "fifo::forget", FIFOS).remove(&key);
}

/// Register this end and tell anyone waiting for it.
fn attach(fifo: &Fifo, mode: OpenMode) {
    let notif = {
        let mut pipe = ranked_lock!(RANK_PIPE, "fifo::attach", fifo.pipe);
        if mode.readable() {
            pipe.readers += 1;
            fifo.reader_seen.store(true, Ordering::SeqCst);
        }
        if mode.writable() {
            pipe.writers += 1;
            // A writer arriving ends the end-of-file the previous one left
            // behind: a FIFO that is reopened for writing is readable again,
            // which is the whole of how `while true; do ...; done > fifo`
            // reads on the other side.
            pipe.closed = false;
            fifo.writer_seen.store(true, Ordering::SeqCst);
        }
        pipe.notify_ends()
    };
    notif.flush();
    fifo.open_wq.wake_all();
}

/// Undo an `attach` for an open that is not going to complete.
fn detach(fifo: &Fifo, mode: OpenMode) {
    let notif = {
        let mut pipe = ranked_lock!(RANK_PIPE, "fifo::detach", fifo.pipe);
        if mode.readable() {
            pipe.close_reader_silent();
        }
        if mode.writable() {
            pipe.close_writer_silent();
        }
        pipe.notify_ends()
    };
    notif.flush();
    fifo.open_wq.wake_all();
}

/// Open the named pipe at `key`, returning the descriptor for this end.
///
/// Blocks for the peer unless `nonblock` is set; see the module documentation
/// for what the wait is actually on.
pub fn open(key: FifoKey, mode: OpenMode, nonblock: bool) -> Result<FileDescriptor, Errno> {
    let fifo = incarnation(key);
    attach(&fifo, mode);

    // Read-write is not a rendezvous at all: the caller is both ends, so there
    // is nobody to wait for. POSIX leaves it undefined and every Unix makes it
    // return at once, which is what makes a FIFO usable as a control channel by
    // a program that must hold it open across writers coming and going.
    let peer_needed = match mode {
        OpenMode::ReadOnly => Some(&fifo.writer_seen),
        OpenMode::WriteOnly => Some(&fifo.reader_seen),
        OpenMode::ReadWrite => None,
    };

    if let Some(seen) = peer_needed
        && !seen.load(Ordering::SeqCst)
    {
        if nonblock {
            // A non-blocking read-only open succeeds with no writer; a
            // write-only one cannot, since there would be nowhere for its
            // bytes to go.
            if mode.writable() {
                detach(&fifo, mode);
                return Err(Errno::ENXIO);
            }
        } else {
            let ready = || seen.load(Ordering::SeqCst);
            if fifo.open_wq.wait_until_killable(ready) == WaitOutcome::Killed {
                detach(&fifo, mode);
                return Err(Errno::EINTR);
            }
        }
    }

    Ok(match mode {
        OpenMode::ReadOnly => FileDescriptor::PipeRead(fifo.pipe.clone()),
        OpenMode::WriteOnly => FileDescriptor::PipeWrite(fifo.pipe.clone()),
        OpenMode::ReadWrite => FileDescriptor::PipeReadWrite(fifo.pipe.clone()),
    })
}
