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
