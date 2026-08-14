//! Kernel allocator with per-CPU freelists.
//!
//! Small allocations (up to 1024 bytes) are served from per-CPU caches,
//! avoiding the global heap lock entirely in the common case. Larger
//! allocations and cache misses fall through to the buddy allocator.

use core::{
    alloc::{GlobalAlloc, Layout},
    cell::UnsafeCell,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

use buddy_system_allocator::Heap;
use x86_64::{VirtAddr, align_up, structures::paging::PageTableFlags};

use crate::{
    memory::{KERNEL_HEAP, KERNEL_HEAP_SIZE, mapper::memory_mapper, valloc::vmalloc},
    println,
    thread::irqlock::IrqSpinlock,
};

// ---------------------------------------------------------------------------
// Per-CPU cache
// ---------------------------------------------------------------------------

/// Size classes served by per-CPU caches. Each is a power of two so objects
/// are naturally aligned to their size class.
const SIZE_CLASSES: [usize; 6] = [32, 64, 128, 256, 512, 1024];

/// Maximum cached objects per size class per CPU.
const CACHE_SLOTS: usize = 16;

/// How many objects to batch-refill or batch-drain at a time.
const BATCH: usize = 8;

/// Return the size-class index for a (size, align) pair, or `None` if the
/// request should go to the global allocator.
fn size_class_index(size: usize, align: usize) -> Option<usize> {
    for (i, &sc) in SIZE_CLASSES.iter().enumerate() {
        if size <= sc && align <= sc {
            return Some(i);
        }
    }
    None
}

/// Fixed-size pointer stack for one size class.
struct SizeClassCache {
    stack: [*mut u8; CACHE_SLOTS],
    count: usize,
}

impl SizeClassCache {
    const fn new() -> Self {
        Self {
            stack: [core::ptr::null_mut(); CACHE_SLOTS],
            count: 0,
        }
    }

    fn try_pop(&mut self) -> Option<*mut u8> {
        if self.count == 0 {
            return None;
        }
        self.count -= 1;
        Some(self.stack[self.count])
    }

    fn try_push(&mut self, ptr: *mut u8) -> bool {
        if self.count >= CACHE_SLOTS {
            return false;
        }
        self.stack[self.count] = ptr;
        self.count += 1;
        true
    }

    /// Drain up to `BATCH` entries, returning (array, count).
    fn drain_batch(&mut self) -> ([*mut u8; BATCH], usize) {
        let n = self.count.min(BATCH);
        let mut out = [core::ptr::null_mut(); BATCH];
        for slot in out.iter_mut().take(n) {
            self.count -= 1;
            *slot = self.stack[self.count];
        }
        (out, n)
    }
}

/// Per-CPU allocation cache. Lives inside `PerCpuData`, accessed only by the
/// owning CPU with interrupts disabled.
pub struct PerCpuCache {
    caches: [SizeClassCache; 6],
    ready: bool,
}

// SAFETY: PerCpuCache is only accessed by its owning CPU with IRQs off.
unsafe impl Send for PerCpuCache {}
unsafe impl Sync for PerCpuCache {}

impl PerCpuCache {
    pub const fn new() -> Self {
        Self {
            caches: [
                SizeClassCache::new(),
                SizeClassCache::new(),
                SizeClassCache::new(),
                SizeClassCache::new(),
                SizeClassCache::new(),
                SizeClassCache::new(),
            ],
            ready: false,
        }
    }

    pub fn enable(&mut self) {
        self.ready = true;
    }
}

/// Wrapper so we can store PerCpuCache in `PerCpuData` (which needs Sync).
pub struct PerCpuCacheCell(pub UnsafeCell<PerCpuCache>);
unsafe impl Sync for PerCpuCacheCell {}
unsafe impl Send for PerCpuCacheCell {}

impl PerCpuCacheCell {
    pub const fn new() -> Self {
        Self(UnsafeCell::new(PerCpuCache::new()))
    }

    /// Get mutable access. Only call from owning CPU with interrupts disabled.
    // The cache is per-CPU interior-mutable state: `&self` is the only handle a
    // CPU ever has to its own cell, and the safety contract restricts callers to
    // the owning CPU with interrupts off, so no second reference can exist.
    #[allow(clippy::mut_from_ref)]
    #[inline(always)]
    pub unsafe fn get_mut(&self) -> &mut PerCpuCache {
        unsafe { &mut *self.0.get() }
    }
}

// ---------------------------------------------------------------------------
// Global allocator
// ---------------------------------------------------------------------------

#[global_allocator]
pub static ALLOCATOR: Allocator = Allocator {
    inner: IrqSpinlock::new(Heap::empty()),
};

pub struct Allocator {
    pub(crate) inner: IrqSpinlock<Heap<32>>,
}

const MIN_EXPANSION: u64 = 1 << 20; // 1mb

/// Serialize heap expansion so only one CPU expands at a time.
static EXPANDING: AtomicBool = AtomicBool::new(false);

/// Set once GS base is initialized on the BSP. Avoids an expensive `rdmsr`
/// on every alloc/dealloc just to check if per-CPU data is available.
static GS_INITIALIZED: AtomicBool = AtomicBool::new(false);

#[inline(always)]
fn gs_ready() -> bool {
    GS_INITIALIZED.load(Ordering::Relaxed)
}

/// Mark GS base as initialized. Called once from BSP boot after `init_gs_for_bsp_static`.
pub fn mark_gs_ready() {
    GS_INITIALIZED.store(true, Ordering::Release);
}

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Try per-CPU cache first (interrupts disabled for the duration).
        if gs_ready()
            && let Some(ptr) = self.try_percpu_alloc(layout)
        {
            return ptr;
        }

        // Fall through to global allocator.
        self.global_alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Try per-CPU cache first.
        if gs_ready() && self.try_percpu_dealloc(ptr, layout) {
            return;
        }

        // Fall through to global allocator.
        unsafe {
            self.inner
                .lock()
                .dealloc(NonNull::new_unchecked(ptr), layout);
        }
    }
}

impl Allocator {
    /// Try to allocate from the per-CPU cache. Returns None on miss.
    fn try_percpu_alloc(&self, layout: Layout) -> Option<*mut u8> {
        let idx = size_class_index(layout.size(), layout.align())?;
        let sc = SIZE_CLASSES[idx];

        x86_64::instructions::interrupts::without_interrupts(|| {
            let cache = unsafe { crate::util::per_cpu::get_percpu_data().heap_cache.get_mut() };
            if !cache.ready {
                return None;
            }

            // Fast path: cache hit.
            if let Some(ptr) = cache.caches[idx].try_pop() {
                return Some(ptr);
            }

            // Slow path: batch refill from global heap (lock taken here).
            let sc_layout = unsafe { Layout::from_size_align_unchecked(sc, sc) };
            let mut heap = self.inner.lock();
            let mut last = None;
            for _ in 0..BATCH {
                if let Ok(block) = heap.alloc(sc_layout) {
                    if let Some(prev) = last {
                        // Push previous into cache.
                        let _ = cache.caches[idx].try_push(prev);
                    }
                    last = Some(block.as_ptr());
                } else {
                    break;
                }
            }
            last
        })
    }

    /// Try to dealloc into the per-CPU cache. Returns false if not applicable.
    fn try_percpu_dealloc(&self, ptr: *mut u8, layout: Layout) -> bool {
        let idx = match size_class_index(layout.size(), layout.align()) {
            Some(i) => i,
            None => return false,
        };
        let sc = SIZE_CLASSES[idx];

        x86_64::instructions::interrupts::without_interrupts(|| {
            let cache = unsafe { crate::util::per_cpu::get_percpu_data().heap_cache.get_mut() };
            if !cache.ready {
                return false;
            }

            // Fast path: cache has room.
            if cache.caches[idx].try_push(ptr) {
                return true;
            }

            // Slow path: cache full. Drain half, then push this one.
            let (drained, n) = cache.caches[idx].drain_batch();
            let _ = cache.caches[idx].try_push(ptr); // guaranteed to succeed

            // Return drained objects to global heap.
            let sc_layout = unsafe { Layout::from_size_align_unchecked(sc, sc) };
            let mut heap = self.inner.lock();
            for ptr in drained.iter().take(n) {
                unsafe {
                    heap.dealloc(NonNull::new_unchecked(*ptr), sc_layout);
                }
            }

            true
        })
    }

    /// Global allocator path (no per-CPU cache).
    fn global_alloc(&self, layout: Layout) -> *mut u8 {
        // If the layout fits a size class, round up to the size-class layout.
        // This ensures all small blocks use a consistent layout regardless of
        // whether they were allocated before or after the per-CPU cache was
        // enabled, so dealloc through the cache drain uses the correct layout.
        let layout = match size_class_index(layout.size(), layout.align()) {
            Some(idx) => {
                let sc = SIZE_CLASSES[idx];
                unsafe { Layout::from_size_align_unchecked(sc, sc) }
            }
            None => layout,
        };

        {
            let mut heap = self.inner.lock();
            if let Ok(block) = heap.alloc(layout) {
                return block.as_ptr();
            }
        }

        // Serialize expansion.
        loop {
            match EXPANDING.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed) {
                Ok(_) => break,
                Err(_) => {
                    while EXPANDING.load(Ordering::Relaxed) {
                        core::hint::spin_loop();
                    }
                    let mut heap = self.inner.lock();
                    if let Ok(block) = heap.alloc(layout) {
                        return block.as_ptr();
                    }
                }
            }
        }

        let padded = layout.pad_to_align();
        let mut need = padded.size() as u64;
        need = align_up(need, 4096);

        let mut chunk = need.next_power_of_two().max(MIN_EXPANSION);
        let max_block: u64 = 1u64 << 32;
        if chunk > max_block {
            chunk = max_block;
        }

        let reserve = chunk * 2;
        let raw = vmalloc(reserve);

        let base = align_up(raw.as_u64(), chunk);
        let end = base + chunk;

        {
            let mut mapper = memory_mapper();
            mapper
                .map_memory(
                    VirtAddr::new(base),
                    chunk,
                    PageTableFlags::WRITABLE | PageTableFlags::GLOBAL,
                )
                .expect("failed to map heap expansion");
        }

        let result = {
            let mut heap = self.inner.lock();
            unsafe { heap.add_to_heap(base as usize, end as usize) };
            heap.alloc(layout)
                .map(|b| b.as_ptr())
                .unwrap_or(core::ptr::null_mut())
        };

        EXPANDING.store(false, Ordering::Release);
        result
    }
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

pub fn init_heap() {
    let heap_start = KERNEL_HEAP;
    let heap_end = KERNEL_HEAP + KERNEL_HEAP_SIZE;
    let heap_size = heap_end - heap_start;

    let mut mapper = memory_mapper();
    mapper
        .map_memory(
            heap_start,
            KERNEL_HEAP_SIZE,
            PageTableFlags::WRITABLE | PageTableFlags::GLOBAL,
        )
        .expect("failed to map heap");

    println!("Mapped kernel heap at {:p}-{:p}", heap_start, heap_end);

    unsafe {
        ALLOCATOR
            .inner
            .lock()
            .init(heap_start.as_u64() as usize, heap_size as usize);
    }
}

/// Enable the per-CPU allocation cache for the calling CPU.
/// Call after the heap and per-CPU data are initialized.
pub fn enable_percpu_cache() {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        crate::util::per_cpu::get_percpu_data()
            .heap_cache
            .get_mut()
            .enable();
    });
}

pub fn print_alloc_stats() {
    let used = ALLOCATOR.inner.lock().stats_alloc_actual();
    let size = ALLOCATOR.inner.lock().stats_total_bytes();

    println!("Kernel heap {} kb / {} kb", used / 1024, size / 1024);
}
