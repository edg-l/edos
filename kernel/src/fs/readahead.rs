use core::sync::atomic::{AtomicU64, Ordering};

/// Initial readahead window size in pages (16 KiB).
pub const RA_INIT_PAGES: u64 = 4;
/// Maximum readahead window size in pages (512 KiB).
pub const RA_MAX_PAGES: u64 = 128;
/// Sentinel value for `prev_last_page` meaning "no prior read on this fd".
pub const RA_NO_PREV: u64 = u64::MAX;

/// Whole-file prefetch threshold in pages (2 MiB). On the first sequential
/// read of a file this small, fill the entire file in one bulk pass. Common
/// EDOS binaries and configs are well under this; readahead's ramp-up
/// otherwise takes ~7 calls to reach max window, during which each 64 KiB
/// user read still triggers one AHCI command.
pub const RA_WHOLE_FILE_MAX_PAGES: u64 = 512;

/// Per-fd readahead window state.
///
/// Tracks how far ahead of the user's requested range to pre-fetch pages, and
/// whether the access pattern looks sequential. Window doubles on actual I/O
/// (uncached fills), capped at `RA_MAX_PAGES`. Random reads reset to zero so
/// the next sequential run starts fresh at `RA_INIT_PAGES`.
#[derive(Debug, Clone, Copy)]
pub struct ReadaheadState {
    /// Last page index returned to the user on the previous read on this fd.
    /// `RA_NO_PREV` means "no prior read".
    pub prev_last_page: u64,
    /// Number of *extra* pages to fetch past the user's requested end page.
    /// `0` means "no active window — use RA_INIT_PAGES on the next sequential read".
    pub window_size: u64,
}

impl Default for ReadaheadState {
    fn default() -> Self {
        Self {
            prev_last_page: RA_NO_PREV,
            window_size: 0,
        }
    }
}

impl ReadaheadState {
    /// Reset the ramp after a non-sequential read.
    ///
    /// Only `window_size` is touched; `prev_last_page` is left as-is because
    /// the read path unconditionally overwrites it with the current read's
    /// `end_page` right after the fill. Writing `RA_NO_PREV` here would be a
    /// dead store masking the actual position and could mis-detect the next
    /// read as "first read on fd" if the overwrite ever became conditional.
    pub fn reset(&mut self) {
        self.window_size = 0;
    }
}

// ---------------------------------------------------------------------------
// Branch counters
// ---------------------------------------------------------------------------

// A readahead window past the caller's requested range takes exactly one of
// three paths in `vfs::page_cache_read_core`, and throughput, stall counts and
// `ncq_inflight` cannot tell them apart: only the first is asynchronous, and
// the other two are a bulk fill billed to the reader inside its own `read`.
// Exposed as `/proc/readahead_stats` and reported by `fsbench ra`.

/// Windows the driver accepted for asynchronous prefetch.
pub static RA_ASYNC_WINDOWS: AtomicU64 = AtomicU64::new(0);
/// Pages in those windows.
pub static RA_ASYNC_PAGES: AtomicU64 = AtomicU64::new(0);
/// Of those, windows whose `PageFillHandle` could not be installed because a
/// page in the range was already in flight. The block I/O is submitted before
/// the install is attempted, so such a window still reads from the device and
/// then discards the result: the pages never reach the cache, and the next read
/// finds the same range uncached and submits it again.
pub static RA_ASYNC_DROPPED_WINDOWS: AtomicU64 = AtomicU64::new(0);
/// Pages in those windows — device reads whose result nothing keeps.
pub static RA_ASYNC_DROPPED_PAGES: AtomicU64 = AtomicU64::new(0);
/// Windows the driver declined (no single extent covers the range, inline data,
/// or a run too long for one command), which fall back to a synchronous fill.
pub static RA_SYNC_WINDOWS: AtomicU64 = AtomicU64::new(0);
/// Pages in those windows.
pub static RA_SYNC_PAGES: AtomicU64 = AtomicU64::new(0);
/// Windows whose prefetch submit failed outright, taking the same fallback.
pub static RA_ERR_WINDOWS: AtomicU64 = AtomicU64::new(0);
/// Pages in those windows.
pub static RA_ERR_PAGES: AtomicU64 = AtomicU64::new(0);

/// Record a window the driver accepted for asynchronous prefetch, and whether
/// its fill handle was installed or the submitted read will be discarded.
#[inline]
pub fn count_async_window(pages: u64, installed: bool) {
    RA_ASYNC_WINDOWS.fetch_add(1, Ordering::Relaxed);
    RA_ASYNC_PAGES.fetch_add(pages, Ordering::Relaxed);
    if !installed {
        RA_ASYNC_DROPPED_WINDOWS.fetch_add(1, Ordering::Relaxed);
        RA_ASYNC_DROPPED_PAGES.fetch_add(pages, Ordering::Relaxed);
    }
}

/// Record a window that fell back to a synchronous fill, either because the
/// driver declined it (`accepted` false path) or because the submit failed.
#[inline]
pub fn count_sync_window(pages: u64, submit_failed: bool) {
    if submit_failed {
        RA_ERR_WINDOWS.fetch_add(1, Ordering::Relaxed);
        RA_ERR_PAGES.fetch_add(pages, Ordering::Relaxed);
    } else {
        RA_SYNC_WINDOWS.fetch_add(1, Ordering::Relaxed);
        RA_SYNC_PAGES.fetch_add(pages, Ordering::Relaxed);
    }
}
