use crate::thread::preempt::PreemptSpinlock;
use alloc::{
    boxed::Box,
    collections::btree_map::BTreeMap,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::{
    debug::lock_order::{
        RANK_DIRTY_INODES, RANK_INODE, RANK_MAPPERS, RANK_PAGES, RANK_USER_MM, RANK_VFS, RANK_VMAS,
    },
    ranked_lock, ranked_read, ranked_write,
};

use super::{
    Error, File, FileAttrs, FileKind, FileSystem, LinkEscape, LinkMode, MmapRegion, MountInfo,
    StatFs, dentry, handle::Pollable, icache, inode::VfsInode, page_fill, path::Path,
    readahead::ReadaheadState,
};
use x86_64::{
    VirtAddr,
    structures::paging::{Mapper, Page, Size4KiB},
};

use crate::thread::preempt::PreemptRwLock;
use crate::{
    fs::gpt::FilesystemType,
    memory::{frame_allocator::frame_allocator, mapper::MemoryManager, vma::VmaBacking},
    thread::irqlock::IrqSpinlock,
};

static NEXT_MOUNT_ID: AtomicUsize = AtomicUsize::new(1);

/// Global registry of inodes that have MAP_SHARED dirty pages.
/// Entries are Weak so that inode drop removes them naturally (via tombstoning).
/// The writeback kthread iterates this list to flush dirty shared-mapping pages.
// IrqSpinlock (not plain Mutex) because `register_dirty_inode` is called from
// `fault_in_page`, which in turn runs inside the page-fault handler context.
// With a plain spin::Mutex, the writeback kthread could hold DIRTY_INODES
// while getting preempted, then the same CPU takes a page fault that calls
// `register_dirty_inode` -- deadlock. IrqSpinlock disables interrupts while
// held, preventing the recursive acquire.
static DIRTY_INODES: IrqSpinlock<Vec<Arc<VfsInode>>> = IrqSpinlock::new(Vec::new());

pub struct MountEntry {
    pub fs: Arc<dyn FileSystem + Send + Sync>,
    pub mount_id: usize,
    pub device_id: usize,
    pub partition_index: usize,
    pub filesystem: FilesystemType,
}

static VFS: PreemptRwLock<BTreeMap<Path, MountEntry>> = PreemptRwLock::new(BTreeMap::new());

/// Result of a VFS lookup: the filesystem, mount-relative path, and mount ID.
/// Internal type used by resolve functions.
struct VfsLookup {
    fs: Arc<dyn FileSystem + Send + Sync>,
    relative: Path,
    mount_id: usize,
    mount_path: Path,
}

/// Stable filesystem mount info (fs, relative path, mount_id) cached from
/// open-time resolution.  The filesystem backing an open fd never changes,
/// so this eliminates redundant mount-registry scans on read/write.
#[derive(Clone)]
pub struct VfsFsInfo {
    pub fs: Arc<dyn FileSystem + Send + Sync>,
    pub relative: Path,
    pub mount_id: usize,
}

/// A resolved filesystem operation handle.
pub struct VfsOp {
    pub fs: Arc<dyn FileSystem + Send + Sync>,
    pub relative: Path,
    pub inode: Option<Arc<VfsInode>>,
    pub mount_id: usize,
    /// Where `fs` is mounted, so a symbolic link that escaped the mount can be
    /// put back into the namespace it was written in. See [`link_escape`].
    mount_path: Path,
}

impl VfsOp {
    /// Rebuild an op from what an open file descriptor cached at open time.
    ///
    /// Open resolved its path in full, symbolic links included, so the
    /// descriptor names a file directly. `mount_path` exists only to put a
    /// link that escaped its mount back into the namespace, which cannot
    /// happen from here, so it stays at the root.
    pub fn from_open_file(
        fs: Arc<dyn FileSystem + Send + Sync>,
        relative: Path,
        inode: Option<Arc<VfsInode>>,
        mount_id: usize,
    ) -> Self {
        Self {
            fs,
            relative,
            inode,
            mount_id,
            mount_path: Path::from_components(Vec::new()),
        }
    }

    /// Return just the stable filesystem info (fs, relative path, mount_id),
    /// excluding the inode.  Useful for caching in FsFile.
    pub fn fs_info(&self) -> VfsFsInfo {
        VfsFsInfo {
            fs: self.fs.clone(),
            relative: self.relative.clone(),
            mount_id: self.mount_id,
        }
    }
}

/// Look up the filesystem for a given path.
/// Uses longest-prefix matching to find the deepest mount point.
fn lookup(path: &Path) -> Option<VfsLookup> {
    let registry = ranked_read!(RANK_VFS, "VFS", VFS);
    let mut best_mount: Option<(&Path, &MountEntry)> = None;
    for (mount_path, entry) in registry.iter() {
        if path == mount_path || path.starts_with(mount_path) {
            match best_mount {
                None => best_mount = Some((mount_path, entry)),
                Some((prev, _)) if mount_path.component_count() > prev.component_count() => {
                    best_mount = Some((mount_path, entry));
                }
                _ => {}
            }
        }
    }
    let (mount_path, entry) = best_mount?;
    let relative = path.strip_prefix(mount_path).normalize();
    Some(VfsLookup {
        fs: entry.fs.clone(),
        relative,
        mount_id: entry.mount_id,
        mount_path: mount_path.clone(),
    })
}

/// Look up for FileInfo specifically - handles the case where the path itself is a mount point
/// by resolving through the parent filesystem.
fn lookup_for_info(path: &Path) -> Option<VfsLookup> {
    if ranked_read!(RANK_VFS, "VFS", VFS).contains_key(path) {
        if let Some(parent) = path.parent() {
            return lookup(&parent).map(|lk| {
                let name = path.last_component().unwrap_or("");
                let relative = lk.relative.join(name).normalize();
                VfsLookup {
                    fs: lk.fs,
                    relative,
                    mount_id: lk.mount_id,
                    mount_path: lk.mount_path,
                }
            });
        }
    }
    lookup(path)
}

/// Resolve a path to its VfsInode, using the dentry cache.
/// On cache miss, calls the filesystem's resolve_inode + file_info to populate.
fn resolve_inode_for(
    mount_id: usize,
    fs: &Arc<dyn FileSystem + Send + Sync>,
    relative: &Path,
) -> Option<Arc<VfsInode>> {
    let dc = dentry::dentry_cache();

    if let Some(inode) = dc.lookup(mount_id, relative) {
        return Some(inode);
    }

    let ino = fs.resolve_inode(relative).ok()?;
    let info = fs.file_info(relative).ok()?;
    // Through the inode cache, not `VfsInode::new`: a dentry miss on a path
    // whose inode is still live (invalidated by truncate/rename, or evicted by
    // the dentry LRU) must return that same inode, or the file ends up with two
    // page caches. See fs/icache.rs.
    let inode = icache::get_or_insert(mount_id, ino, info.kind);
    dc.insert(mount_id, relative.clone(), inode.clone());
    Some(inode)
}

/// Resolve a path to a VfsOp with inode from dentry cache.
pub fn resolve(path: &Path) -> Option<VfsOp> {
    let lk = lookup(path)?;
    let inode = resolve_inode_for(lk.mount_id, &lk.fs, &lk.relative);
    Some(VfsOp {
        fs: lk.fs,
        relative: lk.relative,
        inode,
        mount_id: lk.mount_id,
        mount_path: lk.mount_path,
    })
}

/// Resolve for file_info - handles mount point parent resolution.
pub fn resolve_for_info(path: &Path) -> Option<VfsOp> {
    let lk = lookup_for_info(path)?;
    let inode = resolve_inode_for(lk.mount_id, &lk.fs, &lk.relative);
    Some(VfsOp {
        fs: lk.fs,
        relative: lk.relative,
        inode,
        mount_id: lk.mount_id,
        mount_path: lk.mount_path,
    })
}

/// Resolve a path to a VfsOp without inode resolution.
/// Used for create/mkdir where the target doesn't exist yet.
pub fn resolve_mount(path: &Path) -> Option<VfsOp> {
    let lk = lookup(path)?;
    Some(VfsOp {
        fs: lk.fs,
        relative: lk.relative,
        inode: None,
        mount_id: lk.mount_id,
        mount_path: lk.mount_path,
    })
}

/// Resolve using a pre-cached inode (from FsFile). Skips dentry lookup.
pub fn resolve_with_inode(path: &Path, inode: Option<Arc<VfsInode>>) -> Option<VfsOp> {
    let lk = lookup(path)?;
    Some(VfsOp {
        fs: lk.fs,
        relative: lk.relative,
        inode,
        mount_id: lk.mount_id,
        mount_path: lk.mount_path,
    })
}

/// Put a symbolic link's target back into the namespace it was written in.
///
/// A filesystem walks paths from its own root and cannot see the mount table,
/// so it stops at every link it is asked to follow and reports
/// `Error::LinkEscape`. This turns that report into the absolute path the link
/// named, which the caller resolves again from the VFS root, possibly landing
/// on another mount.
pub fn link_escape(op: &VfsOp, mode: LinkMode) -> Result<Path, Error> {
    let components = match op.fs.link_escape(&op.relative, mode)? {
        LinkEscape::Absolute(components) => components,
        LinkEscape::AboveMount { up, components } => {
            // `..` past the VFS root stays at the root, as POSIX has it.
            let mount = op.mount_path.components();
            let keep = mount.len().saturating_sub(up);
            let mut out = mount[..keep].to_vec();
            out.extend(components);
            out
        }
    };
    Ok(Path::from_components(components))
}

// --- Readahead debug logging ---
// Flip the body to enable debug logs locally; keep disabled by default.
// To enable: replace `{}` with `{ crate::serial_println!($($arg)*); }`
macro_rules! ra_log {
    ($($arg:tt)*) => {};
}

// --- Read-path operations ---

pub fn read(
    op: &VfsOp,
    ra: &mut ReadaheadState,
    offset: usize,
    count: usize,
) -> Result<Vec<u8>, Error> {
    // Hold the read lock only for the file_size query. Released before
    // page_cache_read so the lock isn't held across disk I/O / WaitQueue
    // parks during page fills.
    //
    // TOCTOU: after the drop, a concurrent truncate may shrink the file.
    // We will then read up to the *old* file_size; the page cache handles
    // missing pages via fill_page (which zero-fills past EOF), so the
    // returned bytes are well-defined but may include zero bytes from the
    // logically-truncated region. This is the standard POSIX read/truncate
    // race; do NOT "fix" by reacquiring the lock — that reintroduces
    // park-under-lock and serializes all readers against any writer.
    let file_size_opt =
        {
            let _guard = op
                .inode
                .as_ref()
                .map(|i| i.lock.read_ranked(RANK_INODE, "inode.lock"));
            if let Some(inode) = op.inode.as_ref().filter(|i| i.ino != 0) {
                if let Some(_pc_ops) = op.fs.as_page_cache_ops() {
                    Some(op.fs.file_size_ino(inode.ino).unwrap_or_else(|_| {
                        op.fs.file_info(&op.relative).map(|f| f.size).unwrap_or(0)
                    }) as usize)
                } else {
                    // No page-cache ops; try read_bytes_ino fallback (still under lock).
                    match op.fs.read_bytes_ino(inode.ino, offset, count) {
                        Err(Error::Unsupported) => None,
                        result => return result,
                    }
                }
            } else {
                None
            }
        };

    let file_size = match file_size_opt {
        Some(s) => s,
        None => {
            // No page-cache ops, or ino == 0, or read_bytes_ino unsupported.
            // Path-based read doesn't use the lock guard (already dropped).
            return op.fs.read_bytes(&op.relative, offset, count);
        }
    };

    let inode = op
        .inode
        .as_ref()
        .expect("vfs::read: missing inode after page_cache_ops check");
    let pc_ops = op
        .fs
        .as_page_cache_ops()
        .expect("vfs::read: missing pc_ops after page_cache_ops check");

    if offset >= file_size {
        return Ok(Vec::new());
    }
    let clamped = count.min(file_size - offset);
    page_cache_read(inode, pc_ops, ra, file_size, offset, clamped)
}

fn page_cache_read(
    inode: &Arc<VfsInode>,
    pc_ops: &dyn super::page_cache::PageCacheOps,
    ra: &mut ReadaheadState,
    file_size: usize,
    offset: usize,
    count: usize,
) -> Result<Vec<u8>, Error> {
    let mut result = Vec::with_capacity(count);
    page_cache_read_core(
        inode,
        pc_ops,
        ra,
        file_size,
        offset,
        count,
        &mut |_page_idx, slice, copy_start, copy_end| {
            result.extend_from_slice(&slice[copy_start..copy_end]);
            Ok(())
        },
    )?;
    Ok(result)
}

/// Same as page_cache_read but copies output directly to userspace.
pub fn page_cache_read_to_user(
    inode: &Arc<VfsInode>,
    pc_ops: &dyn super::page_cache::PageCacheOps,
    ra: &mut ReadaheadState,
    file_size: usize,
    offset: usize,
    count: usize,
    user_ptr: *mut u8,
) -> Result<usize, Error> {
    let mut pos: usize = 0;
    page_cache_read_core(
        inode,
        pc_ops,
        ra,
        file_size,
        offset,
        count,
        &mut |_page_idx, slice, copy_start, copy_end| {
            let len = copy_end - copy_start;
            if !unsafe {
                crate::util::uaccess::try_copy_to_user(
                    user_ptr.wrapping_add(pos),
                    slice[copy_start..].as_ptr(),
                    len,
                )
            } {
                return Err(Error::IoError);
            }
            pos += len;
            Ok(())
        },
    )?;
    Ok(pos)
}

fn page_cache_read_core(
    inode: &Arc<VfsInode>,
    pc_ops: &dyn super::page_cache::PageCacheOps,
    ra: &mut ReadaheadState,
    file_size: usize,
    offset: usize,
    count: usize,
    output: &mut dyn FnMut(usize, &[u8], usize, usize) -> Result<(), Error>,
) -> Result<(), Error> {
    if count == 0 {
        return Ok(());
    }

    use super::readahead::{
        RA_INIT_PAGES, RA_MAX_PAGES, RA_NO_PREV, RA_WHOLE_FILE_MAX_PAGES, count_async_window,
        count_skipped_window, count_sync_window, count_trimmed_pages,
    };

    let start_page = offset / 4096;
    let end_page = (offset + count - 1) / 4096;
    let ino = inode.ino;

    let file_size_pages: u64 = (file_size.saturating_add(4095) / 4096) as u64;

    let sequential =
        ra.prev_last_page == RA_NO_PREV || start_page as u64 == ra.prev_last_page.wrapping_add(1);

    let read_end_page: usize = if sequential {
        if ra.prev_last_page == RA_NO_PREV
            && file_size_pages > 0
            && file_size_pages <= RA_WHOLE_FILE_MAX_PAGES
        {
            (file_size_pages - 1) as usize
        } else {
            let ra_pages = if ra.window_size == 0 {
                RA_INIT_PAGES
            } else {
                ra.window_size
            };
            let target = end_page as u64 + 1 + ra_pages;
            let clipped = target.min(file_size_pages);
            (clipped.saturating_sub(1) as usize).max(end_page)
        }
    } else {
        ra.reset();
        end_page
    };

    ra_log!(
        "ra: ino={} req=[{}..{}] read_end={} seq={} win={} prev={}",
        ino,
        start_page,
        end_page,
        read_end_page,
        sequential,
        ra.window_size,
        ra.prev_last_page,
    );

    let mut uncached_start: Option<usize> = None;
    let mut uncached_ranges: Vec<(usize, usize)> = Vec::new();

    for page_idx in start_page..=read_end_page {
        let is_cached = {
            let map = ranked_lock!(RANK_PAGES, "vfs::page_cache_read", inode.pages.pages);
            map.contains_key(&(page_idx as u64))
        };
        if !is_cached {
            if uncached_start.is_none() {
                uncached_start = Some(page_idx);
            }
        } else if let Some(start) = uncached_start.take() {
            uncached_ranges.push((start, page_idx - 1));
        }
    }
    if let Some(start) = uncached_start {
        uncached_ranges.push((start, read_end_page));
    }

    // Split each uncached run at `end_page`: the request portion
    // `[range_start..=min(range_end, end_page)]` must complete before we
    // emit, and is filled synchronously. Anything past `end_page` is
    // pure readahead and can be fired off without parking via
    // `submit_prefetch_pages`, with finalization deferred to the first
    // joiner. On any prefetch-submit failure we fall back to the sync
    // bulk fill — never block the read path on a readahead error.
    //
    // The readahead window is submitted **before** the request portion is
    // filled, so the device works on the next window while the reader is
    // parked on its own pages. Filling first leaves the queue empty for the
    // whole of that park and starts the window only once the reader no
    // longer needs the overlap: the prefetch then trails the reader by a
    // full round trip instead of pulling ahead of it.
    let mut deferred_sync_windows: Vec<(usize, u64, bool)> = Vec::new();
    for &(_, range_end) in &uncached_ranges {
        if range_end <= end_page {
            continue;
        }
        let window_start = end_page + 1;
        let pf_end = range_end;
        // `uncached_ranges` is built from the page map alone, and a page an
        // earlier window is still filling is in neither the map nor the
        // window's way — so most of this window is typically already in
        // flight. Trim to the free tail before submitting anything: the
        // block I/O goes to the device before the handle install can refuse
        // a colliding range, so a window submitted whole is read and thrown
        // away.
        let Some(pf_start) =
            page_fill::narrow_prefetch_window(inode, window_start as u64, pf_end as u64)
        else {
            count_skipped_window((pf_end - end_page) as u64);
            continue;
        };
        let pf_start = pf_start as usize;
        count_trimmed_pages((pf_start - window_start) as u64);
        let pf_offset = pf_start * 4096;
        let pf_count = (pf_end - pf_start + 1) * 4096;
        let pf_pages = (pf_end - pf_start + 1) as u64;
        let submitted = pc_ops.submit_prefetch_pages(ino, pf_offset, pf_count);
        let submit_failed = submitted.is_err();
        match submitted {
            Ok(Some((block_handle, buffer))) => {
                let installed = page_fill::issue_prefetch_bulk(
                    inode,
                    pf_start as u64,
                    pf_pages,
                    block_handle,
                    buffer,
                );
                count_async_window(pf_pages, installed);
            }
            Ok(None) | Err(_) => {
                // No prefetch path available (e.g. cross-extent or
                // unsupported by driver) — fall back to a sync bulk fill of
                // the readahead window so this read still benefits from a
                // populated cache, but the user pays for it. Deferred past
                // the request portion: it is the reader's own pages that
                // must not queue behind a whole window of readahead.
                deferred_sync_windows.push((pf_start, pf_pages, submit_failed));
            }
        }
    }

    for &(range_start, range_end) in &uncached_ranges {
        // Sync portion overlapping the user's requested range.
        let sync_end = range_end.min(end_page);
        if range_start <= sync_end {
            let byte_offset = range_start * 4096;
            let byte_count = (sync_end - range_start + 1) * 4096;
            let page_count = (sync_end - range_start + 1) as u64;
            let bulk_result = page_fill::get_or_fill_bulk_async_sync(
                inode,
                range_start as u64,
                page_count,
                || pc_ops.fill_pages_bulk(ino, byte_offset, byte_count),
            );
            if bulk_result.is_err() {
                for page_idx in range_start..=sync_end {
                    page_fill::get_or_fill_async_sync(inode, page_idx as u64, |buf| {
                        let valid = pc_ops.fill_page(ino, page_idx as u64, buf)?;
                        if valid < 4096 {
                            buf[valid..].fill(0);
                        }
                        Ok(())
                    })?;
                }
            }
        }
    }

    for &(pf_start, pf_pages, submit_failed) in &deferred_sync_windows {
        count_sync_window(pf_pages, submit_failed);
        let byte_offset = pf_start * 4096;
        let byte_count = pf_pages as usize * 4096;
        let _ = page_fill::get_or_fill_bulk_async_sync(inode, pf_start as u64, pf_pages, || {
            pc_ops.fill_pages_bulk(ino, byte_offset, byte_count)
        });
    }

    let did_io = !uncached_ranges.is_empty();
    if sequential && did_io {
        let next = if ra.window_size == 0 {
            RA_INIT_PAGES
        } else {
            ra.window_size.saturating_mul(2)
        };
        ra.window_size = next.min(RA_MAX_PAGES);
    }
    ra.prev_last_page = end_page as u64;

    ra_log!(
        "ra: ino={} after: win={} prev={} did_io={}",
        ino,
        ra.window_size,
        ra.prev_last_page,
        did_io,
    );

    for page_idx in start_page..=end_page {
        let guard = page_fill::get_or_fill_async_sync(inode, page_idx as u64, |buf| {
            let valid = pc_ops.fill_page(ino, page_idx as u64, buf)?;
            if valid < 4096 {
                buf[valid..].fill(0);
            }
            Ok(())
        })?;

        let page_start_in_file = page_idx * 4096;
        let copy_start = if page_idx == start_page {
            offset - page_start_in_file
        } else {
            0
        };
        let copy_end = if page_idx == end_page {
            offset + count - page_start_in_file
        } else {
            4096
        };
        let copy_end = copy_end.min(4096);

        let slice = unsafe { guard.as_slice() };
        output(page_idx, slice, copy_start, copy_end)?;
    }

    Ok(())
}

pub fn file_info(op: &VfsOp) -> Result<File, Error> {
    let _guard = op
        .inode
        .as_ref()
        .map(|i| i.lock.read_ranked(RANK_INODE, "inode.lock"));
    op.fs.file_info(&op.relative)
}

/// `file_info` describing a final symbolic link rather than its target.
pub fn file_info_nofollow(op: &VfsOp) -> Result<File, Error> {
    let _guard = op
        .inode
        .as_ref()
        .map(|i| i.lock.read_ranked(RANK_INODE, "inode.lock"));
    op.fs.file_info_nofollow(&op.relative)
}

pub fn list_files(op: &VfsOp, full_path: &Path) -> Result<Vec<File>, Error> {
    // Release inode.lock (rank 30) before calling child_mount_points, which
    // acquires VFS (rank 10). The inode guard only needs to protect the
    // driver's list_files call; the mount-registry query is fs-global.
    let mut files = {
        let _guard = op
            .inode
            .as_ref()
            .map(|i| i.lock.read_ranked(RANK_INODE, "inode.lock"));
        op.fs.list_files(&op.relative)?
    };

    // Append synthetic directory entries for child mount points.
    for (name, _mount_path) in child_mount_points(full_path) {
        if !files.iter().any(|f| f.name == name) {
            files.push(File {
                name,
                kind: FileKind::Directory,
                size: 0,
                attrs: FileAttrs {
                    readonly: false,
                    hidden: false,
                    system: false,
                    archive: false,
                },
                created: None,
                accessed: None,
                modified: None,
            });
        }
    }

    Ok(files)
}

// --- Write-path operations ---

/// Write with optional O_APPEND support, copying data directly from userspace.
/// Avoids the intermediate kernel heap buffer in the fd-based write path.
pub fn write_from_user(
    op: &VfsOp,
    offset: usize,
    user_ptr: *const u8,
    count: usize,
    append: bool,
) -> Result<u64, Error> {
    // Hold the write lock only for the size query in the append path; release
    // before page_cache_write_from_user so we don't park on disk I/O while
    // holding it. Page-cache writes are internally consistent (per-page dirty
    // marking is atomic), so concurrent readers/writers stay correct.
    //
    // O_APPEND atomicity (matching read's TOCTOU note): we observe the size
    // once under the lock; a concurrent appending write may interleave and
    // produce a "later" append landing at an earlier offset. POSIX guarantees
    // O_APPEND atomicity only for single write() calls; we keep that contract
    // for non-page-cache drivers (still under lock below) but accept the
    // interleave for the page-cache path to keep disk I/O lock-free.
    let actual_offset = {
        let _guard = op
            .inode
            .as_ref()
            .map(|i| i.lock.write_ranked(RANK_INODE, "inode.lock"));
        if append {
            if let Some(ino) = op.inode.as_ref().map(|i| i.ino).filter(|&i| i != 0) {
                match op.fs.file_size_ino(ino) {
                    Ok(size) => size as usize,
                    Err(Error::Unsupported) => op
                        .fs
                        .file_info(&op.relative)
                        .map(|f| f.size as usize)
                        .unwrap_or(offset),
                    Err(e) => return Err(e),
                }
            } else {
                op.fs
                    .file_info(&op.relative)
                    .map(|f| f.size as usize)
                    .unwrap_or(offset)
            }
        } else {
            offset
        }
    };

    if let Some(inode) = op.inode.as_ref().filter(|i| i.ino != 0) {
        if let Some(pc_ops) = op.fs.as_page_cache_ops() {
            return page_cache_write_from_user(inode, pc_ops, actual_offset, count, user_ptr);
        }
        // Fill the buffer before taking the lock: a user copy can demand fault
        // and park, and a thread killed while parked never runs the guard's
        // Drop, which would leave the inode write-locked for good.
        let mut buffer = alloc::vec![0u8; count];
        if !unsafe {
            crate::util::uaccess::try_copy_from_user(buffer.as_mut_ptr(), user_ptr, count)
        } {
            return Err(Error::IoError);
        }
        // No page-cache ops; reacquire the write lock for the synchronous
        // driver call (no parking expected on these paths).
        let _guard = op
            .inode
            .as_ref()
            .map(|i| i.lock.write_ranked(RANK_INODE, "inode.lock"));
        match op.fs.write_bytes_ino(inode.ino, actual_offset, &buffer) {
            Err(Error::Unsupported) => {}
            result => return result,
        }
    }
    let mut buffer = alloc::vec![0u8; count];
    if !unsafe { crate::util::uaccess::try_copy_from_user(buffer.as_mut_ptr(), user_ptr, count) } {
        return Err(Error::IoError);
    }
    op.fs.write_bytes(&op.relative, actual_offset, &buffer)
}

/// Write with optional O_APPEND support. The append size query happens
/// inside a brief write-lock scope; the lock is released before the page-cache
/// write, mirroring `write_from_user`. See its comment for O_APPEND semantics.
pub fn write(op: &VfsOp, offset: usize, data: &[u8], append: bool) -> Result<u64, Error> {
    let actual_offset = {
        let _guard = op
            .inode
            .as_ref()
            .map(|i| i.lock.write_ranked(RANK_INODE, "inode.lock"));
        if append {
            if let Some(ino) = op.inode.as_ref().map(|i| i.ino).filter(|&i| i != 0) {
                match op.fs.file_size_ino(ino) {
                    Ok(size) => size as usize,
                    Err(Error::Unsupported) => op
                        .fs
                        .file_info(&op.relative)
                        .map(|f| f.size as usize)
                        .unwrap_or(offset),
                    Err(e) => return Err(e),
                }
            } else {
                op.fs
                    .file_info(&op.relative)
                    .map(|f| f.size as usize)
                    .unwrap_or(offset)
            }
        } else {
            offset
        }
    };

    if let Some(inode) = op.inode.as_ref().filter(|i| i.ino != 0) {
        if let Some(pc_ops) = op.fs.as_page_cache_ops() {
            return page_cache_write(inode, pc_ops, actual_offset, data);
        }
        let _guard = op
            .inode
            .as_ref()
            .map(|i| i.lock.write_ranked(RANK_INODE, "inode.lock"));
        match op.fs.write_bytes_ino(inode.ino, actual_offset, data) {
            Err(Error::Unsupported) => {}
            result => return result,
        }
    }
    op.fs.write_bytes(&op.relative, actual_offset, data)
}

/// Read with readahead, copying output directly to userspace.
///
/// Same TOCTOU caveat as [`read`]: file_size is captured under the inode read
/// lock, then released before disk I/O. A concurrent truncate may shrink the
/// file before page_cache_read_to_user runs; the page cache zero-fills past
/// EOF so the result is well-defined.
pub fn read_to_user(
    op: &VfsOp,
    ra: &mut ReadaheadState,
    offset: usize,
    count: usize,
    user_ptr: *mut u8,
) -> Result<usize, Error> {
    // Bytes produced by the inode-based fallback, copied out after the guard is
    // released. A user copy can demand fault and park, and a thread killed while
    // parked never runs the guard's Drop, so the reader count would never be
    // decremented.
    let mut fallback: Option<Vec<u8>> = None;

    let file_size_opt =
        {
            let _guard = op
                .inode
                .as_ref()
                .map(|i| i.lock.read_ranked(RANK_INODE, "inode.lock"));
            if let Some(inode) = op.inode.as_ref().filter(|i| i.ino != 0) {
                if let Some(_pc_ops) = op.fs.as_page_cache_ops() {
                    Some(op.fs.file_size_ino(inode.ino).unwrap_or_else(|_| {
                        op.fs.file_info(&op.relative).map(|f| f.size).unwrap_or(0)
                    }) as usize)
                } else {
                    match op.fs.read_bytes_ino(inode.ino, offset, count) {
                        Err(Error::Unsupported) => None,
                        Ok(data) => {
                            fallback = Some(data);
                            None
                        }
                        Err(e) => return Err(e),
                    }
                }
            } else {
                None
            }
        };

    if let Some(data) = fallback {
        let n = data.len();
        if !unsafe { crate::util::uaccess::try_copy_to_user(user_ptr, data.as_ptr(), n) } {
            return Err(Error::IoError);
        }
        return Ok(n);
    }

    let file_size = match file_size_opt {
        Some(s) => s,
        None => {
            let data = op.fs.read_bytes(&op.relative, offset, count)?;
            let n = data.len();
            if !unsafe { crate::util::uaccess::try_copy_to_user(user_ptr, data.as_ptr(), n) } {
                return Err(Error::IoError);
            }
            return Ok(n);
        }
    };

    let inode = op.inode.as_ref().expect("vfs::read_to_user: missing inode");
    let pc_ops = op
        .fs
        .as_page_cache_ops()
        .expect("vfs::read_to_user: missing pc_ops");

    if offset >= file_size {
        return Ok(0);
    }
    let clamped = count.min(file_size - offset);
    page_cache_read_to_user(inode, pc_ops, ra, file_size, offset, clamped, user_ptr)
}

fn page_cache_write(
    inode: &Arc<VfsInode>,
    pc_ops: &dyn super::page_cache::PageCacheOps,
    offset: usize,
    data: &[u8],
) -> Result<u64, Error> {
    page_cache_write_core(inode, pc_ops, offset, data.len(), &mut |start, slice| {
        slice.copy_from_slice(&data[start..start + slice.len()]);
        Ok(())
    })
}

/// Same as page_cache_write but copies data directly from userspace.
fn page_cache_write_from_user(
    inode: &Arc<VfsInode>,
    pc_ops: &dyn super::page_cache::PageCacheOps,
    offset: usize,
    count: usize,
    user_ptr: *const u8,
) -> Result<u64, Error> {
    page_cache_write_core(inode, pc_ops, offset, count, &mut |start, slice| {
        if !unsafe {
            crate::util::uaccess::try_copy_from_user(
                slice.as_mut_ptr(),
                user_ptr.wrapping_add(start),
                slice.len(),
            )
        } {
            return Err(Error::IoError);
        }
        Ok(())
    })
}

fn page_cache_write_core(
    inode: &Arc<VfsInode>,
    pc_ops: &dyn super::page_cache::PageCacheOps,
    offset: usize,
    count: usize,
    source: &mut dyn FnMut(usize, &mut [u8]) -> Result<(), Error>,
) -> Result<u64, Error> {
    if count == 0 {
        return Ok(0);
    }

    let end_offset = offset.checked_add(count).ok_or(Error::IoError)?;

    let start_page = offset / 4096;
    let end_page = (end_offset - 1) / 4096;
    let ino = inode.ino;
    let mut data_pos = 0usize;

    for page_idx in start_page..=end_page {
        let page_start_in_file = page_idx * 4096;
        let write_start = if page_idx == start_page {
            offset - page_start_in_file
        } else {
            0
        };
        let write_end = if page_idx == end_page {
            offset + count - page_start_in_file
        } else {
            4096
        };
        let write_len = write_end - write_start;

        let is_full_page = write_start == 0 && write_end == 4096;

        let guard = if is_full_page {
            page_fill::get_or_fill_async_sync(inode, page_idx as u64, |buf| {
                buf.fill(0);
                Ok(())
            })?
        } else {
            page_fill::get_or_fill_async_sync(inode, page_idx as u64, |buf| {
                let valid = pc_ops.fill_page(ino, page_idx as u64, buf)?;
                if valid < 4096 {
                    buf[valid..].fill(0);
                }
                Ok(())
            })?
        };

        let slice = unsafe { guard.as_slice_mut() };
        source(data_pos, &mut slice[write_start..write_end])?;
        data_pos += write_len;
    }

    // Publish the size before the pages become flushable. A writeback pass
    // that sees a dirty page past the recorded end of file treats it as
    // nothing to write and clears the dirty flag, so marking pages first
    // loses whatever the pass caught in that window -- reliably the first
    // page of a new file, which is written before any size has been stamped.
    let new_size = end_offset as u64;
    pc_ops.update_size(ino, new_size)?;

    for page_idx in start_page..=end_page {
        inode.pages.mark_dirty(page_idx as u64);
    }
    register_dirty_inode(inode);

    Ok(count as u64)
}

pub fn truncate(op: &VfsOp, size: u64) -> Result<(), Error> {
    let _guard = op
        .inode
        .as_ref()
        .map(|i| i.lock.write_ranked(RANK_INODE, "inode.lock"));
    let result = op.fs.truncate(&op.relative, size);
    // An escape is the caller being told to try elsewhere, not a truncate that
    // half happened: nothing was touched, and `op.relative` names a path this
    // filesystem never resolved, so invalidating for it is at best pointless.
    if matches!(result, Err(Error::LinkEscape)) {
        return result;
    }
    // Otherwise always run invalidators, even on FS failure. The FS may have partially
    // applied the truncate (cluster chain trimmed but dirent size update
    // failed, or similar). Leaving stale pages in the cache would let reads
    // return data whose on-disk backing no longer exists. Dropping the
    // dentry cache also forces a fresh resolve on the next open, which
    // picks up whatever state the FS landed in.
    dentry::dentry_cache().invalidate(op.mount_id, &op.relative);
    if let Some(inode) = op.inode.as_ref().filter(|i| i.ino != 0) {
        let from_page = (size as usize + 4095) / 4096;
        // D.2: Unmap PTEs past new_size from every process that has a
        // FileBacked VMA on this inode, before freeing the cache frames.
        // Lock ordering: inode.lock (held above) > mappers.lock > vmas.lock
        // > memory_manager.lock.  We release vmas/mm locks before shootdown.
        // See doc/invariants/lock-order.md for the full rank table.
        invalidate_mappings_above(inode, size);
        inode.pages.invalidate_from(from_page as u64);
        inode.pages.zero_tail(size);
    }
    result
}

pub fn set_times(op: &VfsOp, atime: Option<u64>, mtime: Option<u64>) -> Result<(), Error> {
    let _guard = op
        .inode
        .as_ref()
        .map(|i| i.lock.write_ranked(RANK_INODE, "inode.lock"));
    op.fs.set_times(&op.relative, atime, mtime)
}

pub fn ioctl(op: &VfsOp, request: u64, arg: u64) -> Result<u64, Error> {
    let _guard = op
        .inode
        .as_ref()
        .map(|i| i.lock.write_ranked(RANK_INODE, "inode.lock"));
    op.fs.ioctl(&op.relative, request, arg)
}

// --- Directory mutation operations ---

fn resolve_parent_inode(op: &VfsOp) -> Option<Arc<VfsInode>> {
    let parent = op.relative.parent()?;
    resolve_inode_for(op.mount_id, &op.fs, &parent)
}

/// Whether the name is already taken, for the benefit of the operations that
/// POSIX requires to fail with `EEXIST` rather than replace what is there.
/// A dangling symbolic link takes the name too, so `file_info` alone (which
/// follows links) is not enough.
fn name_taken(op: &VfsOp) -> bool {
    op.fs.file_info(&op.relative).is_ok() || op.fs.read_link(&op.relative).is_ok()
}

pub fn create_file(op: &VfsOp) -> Result<(), Error> {
    let parent_inode = resolve_parent_inode(op);
    let _guard = parent_inode
        .as_ref()
        .map(|i| i.lock.write_ranked(RANK_INODE, "inode.lock"));
    if name_taken(op) {
        return Err(Error::AlreadyExists);
    }
    let result = op.fs.create_file(&op.relative);
    if result.is_ok() {
        dentry::dentry_cache().invalidate(op.mount_id, &op.relative);
    }
    result
}

pub fn symlink(op: &VfsOp, target: &str) -> Result<(), Error> {
    let parent_inode = resolve_parent_inode(op);
    let _guard = parent_inode
        .as_ref()
        .map(|i| i.lock.write_ranked(RANK_INODE, "inode.lock"));
    if name_taken(op) {
        return Err(Error::AlreadyExists);
    }
    let result = op.fs.symlink(target, &op.relative);
    if result.is_ok() {
        dentry::dentry_cache().invalidate(op.mount_id, &op.relative);
    }
    result
}

pub fn read_link(op: &VfsOp) -> Result<String, Error> {
    op.fs.read_link(&op.relative)
}

pub fn create_dir(op: &VfsOp) -> Result<(), Error> {
    let parent_inode = resolve_parent_inode(op);
    let _guard = parent_inode
        .as_ref()
        .map(|i| i.lock.write_ranked(RANK_INODE, "inode.lock"));
    if name_taken(op) {
        return Err(Error::AlreadyExists);
    }
    let result = op.fs.create_dir(&op.relative);
    if result.is_ok() {
        dentry::dentry_cache().invalidate(op.mount_id, &op.relative);
    }
    result
}

/// Remove a regular file. Implements Linux-style orphan-inode semantics
/// via `Arc<VfsInode>` refcounting:
///
/// 1. The FS driver's `remove_file` detaches the dentry from the directory
///    tree only; it MUST NOT free data blocks or the inode itself.
/// 2. The dentry cache entry for this path is invalidated so new opens
///    fail ENOENT.
/// 3. If any `Arc<VfsInode>` refs remain (open fds, FileBacked VMAs), the
///    inode is marked orphan. Existing mappings continue to read and write
///    through live PTEs — their `Arc<CachedPage>` keeps faulted frames alive
///    and the file's blocks stay allocated.
/// 4. `VfsInode::drop` fires when the last ref is released and calls
///    `FileSystem::evict_inode(ino)` to free the on-disk allocation. This
///    is the equivalent of Linux's `evict_inode`.
///
/// The page cache is NOT invalidated: dirty pages are still valid file data
/// going to still-allocated blocks. Writeback keeps working on the orphan
/// until the final Arc drop, at which point InodePages drops too.
pub fn remove_file(op: &VfsOp) -> Result<(), Error> {
    let parent_inode = resolve_parent_inode(op);
    let _guard = parent_inode
        .as_ref()
        .map(|i| i.lock.write_ranked(RANK_INODE, "inode.lock"));
    // `op.inode` was resolved by following symbolic links, so on a link it is
    // the target's inode. Unlinking the link must not orphan the target.
    let is_symlink = op.fs.read_link(&op.relative).is_ok();
    let result = op.fs.remove_file(&op.relative);
    if result.is_ok() {
        dentry::dentry_cache().invalidate(op.mount_id, &op.relative);
        if let Some(inode) = op.inode.as_ref().filter(|i| i.ino != 0 && !is_symlink) {
            inode.mark_orphan();
        }
    }
    result
}

pub fn remove_dir(op: &VfsOp) -> Result<(), Error> {
    let parent_inode = resolve_parent_inode(op);
    let _guard = parent_inode
        .as_ref()
        .map(|i| i.lock.write_ranked(RANK_INODE, "inode.lock"));
    let result = op.fs.remove_dir(&op.relative);
    if result.is_ok() {
        let dc = dentry::dentry_cache();
        dc.invalidate(op.mount_id, &op.relative);
        dc.invalidate_children(op.mount_id, &op.relative);
    }
    result
}

pub fn rename(old_op: &VfsOp, new_op: &VfsOp) -> Result<(), Error> {
    if !Arc::ptr_eq(&old_op.fs, &new_op.fs) {
        return Err(Error::Unsupported);
    }

    let old_parent_inode = resolve_parent_inode(old_op);
    let new_parent_inode = resolve_parent_inode(new_op);

    // Acquire locks in inode number order to prevent deadlocks.
    // The first acquisition is ranked_write (strict); the second is
    // write_ranked_same (same class, different instance). Caller is
    // responsible for the key ordering (ino order above).
    let (_g1, _g2) = match (&old_parent_inode, &new_parent_inode) {
        (Some(a), Some(b)) if Arc::ptr_eq(a, b) => {
            (Some(a.lock.write_ranked(RANK_INODE, "inode.lock")), None)
        }
        (Some(a), Some(b)) if a.ino <= b.ino => {
            let g1 = a.lock.write_ranked(RANK_INODE, "inode.lock");
            let g2 = b.lock.write_ranked_same(RANK_INODE, "inode.lock");
            (Some(g1), Some(g2))
        }
        (Some(a), Some(b)) => {
            let g1 = b.lock.write_ranked(RANK_INODE, "inode.lock");
            let g2 = a.lock.write_ranked_same(RANK_INODE, "inode.lock");
            (Some(g1), Some(g2))
        }
        (Some(a), None) => (Some(a.lock.write_ranked(RANK_INODE, "inode.lock")), None),
        (None, Some(b)) => (Some(b.lock.write_ranked(RANK_INODE, "inode.lock")), None),
        (None, None) => (None, None),
    };

    let result = old_op.fs.rename(&old_op.relative, &new_op.relative);
    if result.is_ok() {
        let dc = dentry::dentry_cache();
        dc.invalidate_children(old_op.mount_id, &old_op.relative);
        dc.invalidate(old_op.mount_id, &old_op.relative);
        dc.invalidate(new_op.mount_id, &new_op.relative);
    }
    result
}

// --- Passthrough operations (no locking needed) ---

pub fn flush(op: &VfsOp) -> Result<(), Error> {
    op.fs.flush()
}

pub fn flush_file(op: &VfsOp) -> Result<(), Error> {
    if let Some(inode) = op.inode.as_ref().filter(|i| i.ino != 0) {
        // Flush per-inode dirty page cache pages via the bulk path.
        //
        // We pass None for new_size_hint: `page_cache_write` already stamped
        // the inode size synchronously via `pc_ops.update_size` at write time
        // (see the doc-comment at vfs.rs:449-457).  fsync's job is data
        // durability, not size re-stamping.
        //
        // Ordering: `flush_pages_bulk` merges all metadata enrollments into
        // the active journal tx; the `flush_inode` call below then seals that
        // tx via `force_commit_and_wait`, guaranteeing the batch is durable
        // before we return.
        let t0 = crate::timer::Instant::now();
        if let Some(pc_ops) = op.fs.as_page_cache_ops() {
            let ino = inode.ino;
            inode
                .pages
                .flush_dirty_bulk(|pages| pc_ops.flush_pages_bulk(ino, pages, None))?;
        }
        let elapsed = t0.elapsed();
        if elapsed.as_millis() >= 1_000 {
            crate::log!(
                "vfs flush_file: slow: {} ms flushing the inode page cache",
                elapsed.as_millis()
            );
        }
        match op.fs.flush_inode(inode.ino) {
            Err(Error::Unsupported) => {}
            result => return result,
        }
    }
    op.fs.flush()
}

pub fn poll(op: &VfsOp) -> Result<Box<dyn Pollable>, Error> {
    op.fs.poll(&op.relative)
}

pub fn mmap(
    op: &VfsOp,
    offset: usize,
    length: usize,
    memory: Arc<PreemptSpinlock<MemoryManager>>,
) -> Result<MmapRegion, Error> {
    op.fs.mmap(&op.relative, offset, length, memory)
}

pub fn statfs(op: &VfsOp) -> Result<StatFs, Error> {
    op.fs.statfs()
}

// --- Mount table management ---

pub fn mount(mount_point: Path, mut entry: MountEntry) {
    entry.mount_id = NEXT_MOUNT_ID.fetch_add(1, Ordering::Relaxed);
    ranked_write!(RANK_VFS, "VFS", VFS).insert(mount_point, entry);
}

pub fn unmount(mount_point: &Path) -> bool {
    ranked_write!(RANK_VFS, "VFS", VFS)
        .remove(mount_point)
        .is_some()
}

pub fn list_mounts() -> Vec<MountInfo> {
    ranked_read!(RANK_VFS, "VFS", VFS)
        .iter()
        .map(|(path, entry)| MountInfo {
            mount_point: path.clone(),
            device_id: entry.device_id,
            partition_index: entry.partition_index,
            filesystem: entry.filesystem.clone(),
        })
        .collect()
}

/// Returns the names and full paths of direct child mount points under `parent`.
/// Used by list_files to synthesize directory entries for mount points.
pub fn child_mount_points(parent: &Path) -> Vec<(String, Path)> {
    let registry = ranked_read!(RANK_VFS, "VFS", VFS);
    let parent_depth = parent.component_count();
    let mut children = Vec::new();
    for (mount_path, _) in registry.iter() {
        if mount_path.starts_with(parent) && mount_path.component_count() == parent_depth + 1 {
            if let Some(name) = mount_path.last_component() {
                children.push((name.to_string(), mount_path.clone()));
            }
        }
    }
    children
}

/// Check if a path is a registered mount point.
pub fn is_mount_point(path: &Path) -> bool {
    ranked_read!(RANK_VFS, "VFS", VFS).contains_key(path)
}

/// Look up the filesystem for a given mount ID.
/// Iterates the mount registry and returns a clone of the matching Arc.
pub fn fs_by_mount_id(mount_id: usize) -> Option<Arc<dyn super::FileSystem + Send + Sync>> {
    let registry = ranked_read!(RANK_VFS, "VFS", VFS);
    for entry in registry.values() {
        if entry.mount_id == mount_id {
            return Some(entry.fs.clone());
        }
    }
    None
}

/// Register an inode as having dirty pages, so the writeback kthread flushes
/// it. Duplicate registrations are deduplicated.
///
/// The list holds a *strong* reference on purpose: closing the last descriptor
/// must not free pages that have never reached the disk. Writeback drops the
/// reference once it has flushed the inode, so the pin lasts exactly as long
/// as there is unwritten data.
pub fn register_dirty_inode(inode: &Arc<VfsInode>) {
    let mut list = DIRTY_INODES.lock_ranked(RANK_DIRTY_INODES, "DIRTY_INODES");
    if !list.iter().any(|held| Arc::ptr_eq(held, inode)) {
        list.push(Arc::clone(inode));
    }
}

/// Flush dirty pages for every registered dirty inode, then release the
/// writeback pin taken by `register_dirty_inode`.
///
/// Called by the writeback kthread on every pass. Taking the list wholesale
/// means an inode dirtied again during the flush re-registers and is picked up
/// by the next pass, rather than being dropped here with data still in memory.
pub fn flush_dirty_inodes() {
    // Take the list; flush outside the lock to avoid holding it across I/O.
    let live: Vec<Arc<VfsInode>> = {
        let mut list = DIRTY_INODES.lock_ranked(RANK_DIRTY_INODES, "DIRTY_INODES");
        core::mem::take(&mut *list)
    };

    for inode in live {
        let fs = match fs_by_mount_id(inode.mount_id) {
            Some(f) => f,
            None => continue,
        };
        let pc_ops = match fs.as_page_cache_ops() {
            Some(ops) => ops,
            None => continue,
        };
        // The writeback kthread does not know the target file size — it was
        // stamped synchronously by `page_cache_write` via `pc_ops.update_size`.
        // Pass None so EFS skips the redundant inode-size write.
        let ino = inode.ino;
        let _ = inode
            .pages
            .flush_dirty_bulk(|pages| pc_ops.flush_pages_bulk(ino, pages, None));
    }
}

/// D.2 — Truncate invalidation: unmap PTEs past `new_size` in every process
/// that has a FileBacked VMA referencing `inode`.
///
/// # Lock ordering
/// Caller must hold inode.lock (write) before calling this function.
/// Inside we acquire: inode.mappers.lock > vmas.lock > memory_manager.lock.
/// The vmas and mm locks are released before issuing the TLB shootdown.
/// See doc/invariants/lock-order.md for the full rank table.
///
/// # Partial-VMA handling
/// A VMA may straddle the new_size boundary.  Only pages whose file offset
/// is >= new_size are invalidated; pages before new_size are left mapped.
/// The VMA itself is NOT split; only the affected PTE slots are cleared.
pub fn invalidate_mappings_above(inode: &Arc<VfsInode>, new_size: u64) {
    use crate::thread::UserThread;

    // Collect live UserThread Arcs while holding the mappers lock, then
    // drop the lock before doing any MM work.
    let live: Vec<alloc::sync::Arc<spin::RwLock<UserThread>>> = {
        let mut mappers = ranked_lock!(RANK_MAPPERS, "inode.mappers", inode.mappers);
        // Compact tombstoned entries as a side effect.
        mappers.retain(|w| w.upgrade().is_some());
        mappers.iter().filter_map(|w| w.upgrade()).collect()
    };

    // Accumulate (start_virt, page_count) pairs for the TLB shootdown after
    // all per-process unmaps are done (cannot hold mm lock during shootdown).
    let mut shootdown_ranges: Vec<(VirtAddr, u64)> = Vec::new();

    for user_arc in &live {
        let user = user_arc.read();
        let mut vmas = ranked_lock!(RANK_VMAS, "user.vmas", user.vmas);
        let mut mm = ranked_lock!(RANK_USER_MM, "user.mm", user.memory_manager);

        // Walk all VMAs in this process looking for FileBacked on this inode.
        // Collect the list of (vma_start, slots_to_invalidate) first, then
        // process them, because we need mutable access to each VMA.
        let vma_starts: Vec<VirtAddr> = vmas
            .iter()
            .filter_map(|vma| match &vma.backing {
                VmaBacking::FileBacked { inode: vi, .. } if Arc::ptr_eq(vi, inode) => {
                    Some(vma.start)
                }
                _ => None,
            })
            .collect();

        for vma_start in vma_starts {
            let vma = match vmas.find_mut(vma_start) {
                Some(v) => v,
                None => continue,
            };

            let (file_offset, pages) = match &mut vma.backing {
                VmaBacking::FileBacked {
                    file_offset, pages, ..
                } => (*file_offset, pages),
                _ => continue,
            };

            // The file byte range covered by this VMA is [file_offset, file_offset + vma.size()).
            // Pages whose file offset >= new_size must be invalidated.
            let vma_size = vma.end.as_u64() - vma.start.as_u64();
            let num_slots = (vma_size / 4096) as usize;

            // First slot index (within the VMA) that is past new_size.
            let first_invalid_slot = if new_size <= file_offset {
                // The entire VMA is past new_size.
                0usize
            } else {
                let bytes_before_cut = new_size - file_offset;
                ((bytes_before_cut + 4095) / 4096) as usize
            };

            if first_invalid_slot >= num_slots {
                // No slots past new_size in this VMA.
                continue;
            }

            let mut invalidated_pages: u64 = 0;
            let first_invalid_virt =
                VirtAddr::new(vma.start.as_u64() + first_invalid_slot as u64 * 4096);

            for slot in first_invalid_slot..num_slots {
                let virt = VirtAddr::new(vma.start.as_u64() + slot as u64 * 4096);
                let page: Page<Size4KiB> = Page::containing_address(virt);

                if let Ok(phys) = mm.mapper.translate_page(page) {
                    if let Ok((_, flush)) = mm.mapper.unmap(page) {
                        flush.ignore();
                        // Decrement the refcount bumped at fault-in time.
                        frame_allocator().dec_refcount(phys);
                        invalidated_pages += 1;
                    }
                }
                // Drop the Arc<CachedPage> for this slot; the inode's page
                // cache entry will be freed by invalidate_from() after we return.
                if slot < pages.len() {
                    pages[slot] = None;
                }
            }

            if invalidated_pages > 0 {
                shootdown_ranges.push((first_invalid_virt, invalidated_pages));
            }
        }
        // vmas lock and mm lock drop here.
    }

    // Issue TLB shootdowns after all MM locks are released.
    for (start, count) in shootdown_ranges {
        crate::memory::tlb::tlb_shootdown(start, count);
    }
}

/// Fetch or fill a single page from an inode's page cache.
///
/// Returns an `Arc<CachedPage>` that callers can clone to keep the page alive
/// independent of the `PageGuard` drop guard.  The returned Arc has one
/// logical pin contributed by the caller; callers must call `page.unpin()`
/// when they no longer need the page held (e.g. on munmap).
///
/// Uses `PageCacheOps::fill_page` supplied by `fs` (the inode's filesystem,
/// passed in by the caller to avoid a redundant `fs_by_mount_id` lookup when
/// the caller has already resolved it).
///
/// Returns `Err(Errno::EINVAL)` if the filesystem does not support the page cache
/// or the inode number is zero.
pub fn get_or_fill_page(
    inode: &Arc<super::inode::VfsInode>,
    page_idx: u64,
    fs: &Arc<dyn super::FileSystem + Send + Sync>,
) -> Result<Arc<super::page_cache::CachedPage>, super::super::syscalls::Errno> {
    let pc_ops = fs
        .as_page_cache_ops()
        .ok_or(super::super::syscalls::Errno::EINVAL)?;
    let ino = inode.ino;
    let guard = page_fill::get_or_fill_async_sync(inode, page_idx, |buf| {
        let valid = pc_ops.fill_page(ino, page_idx, buf)?;
        if valid < 4096 {
            buf[valid..].fill(0);
        }
        Ok(())
    })
    .map_err(|_| super::super::syscalls::Errno::EIO)?;
    // Extract the Arc before the guard drops (and unpins).
    // Pin explicitly so the caller holds a pin independent of the guard.
    let page = guard.arc();
    page.pin();
    Ok(page)
}
