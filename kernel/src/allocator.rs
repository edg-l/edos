//! Kernel allocator with per-CPU freelists.
//!
//! Small allocations (up to 4096 bytes) are served from per-CPU caches,
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
///
/// The top of the range is where the class list earns its keep rather than
/// where allocations are most frequent: everything above it takes the one
/// global heap lock, so a request a page long serialises against every other
/// CPU. Measured through `/proc/alloc_bench` before 2048 and 4096 were added,
/// a 1024-byte allocation cost 17 ns and a 4096-byte one 63.
const SIZE_CLASSES: [usize; 8] = [32, 64, 128, 256, 512, 1024, 2048, 4096];

/// Deepest a size class's stack can be. The array costs one pointer per slot
/// whatever the class, so this only bounds the bookkeeping.
const CACHE_SLOTS: usize = 16;

/// Memory one size class may park per CPU. This is the dial: the slot counts
/// below are derived from it, so adding a class cannot quietly multiply what
/// the caches hold. Eight CPUs across eight classes hold at most 1 MiB.
const CACHE_BYTES: usize = 16 * 1024;

/// Objects `class` may hold per CPU, which is [`CACHE_SLOTS`] until the class
/// is large enough for [`CACHE_BYTES`] to bind: 16 of the 1024-byte class, 8 of
/// the 2048 and 4 of the 4096.
const fn cache_limit(class: usize) -> usize {
    let by_bytes = CACHE_BYTES / SIZE_CLASSES[class];
    if by_bytes < CACHE_SLOTS {
        by_bytes
    } else {
        CACHE_SLOTS
    }
}

/// How many objects to batch-refill or batch-drain at a time, capped by what
/// the class may hold so a refill cannot overshoot its own limit.
const fn batch_for(class: usize) -> usize {
    let limit = cache_limit(class);
    if limit < BATCH { limit } else { BATCH }
}

/// Largest batch any class uses, and so the size of the drain buffer.
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
#[derive(Clone, Copy)]
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

    fn try_push(&mut self, ptr: *mut u8, limit: usize) -> bool {
        if self.count >= limit {
            return false;
        }
        self.stack[self.count] = ptr;
        self.count += 1;
        true
    }

    /// Drain up to `batch` entries, returning (array, count).
    fn drain_batch(&mut self, batch: usize) -> ([*mut u8; BATCH], usize) {
        let n = self.count.min(batch);
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
    caches: [SizeClassCache; SIZE_CLASSES.len()],
    ready: bool,
}

// SAFETY: PerCpuCache is only accessed by its owning CPU with IRQs off.
unsafe impl Send for PerCpuCache {}
unsafe impl Sync for PerCpuCache {}

impl PerCpuCache {
    pub const fn new() -> Self {
        Self {
            caches: [SizeClassCache::new(); SIZE_CLASSES.len()],
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
            let limit = cache_limit(idx);
            let mut heap = self.inner.lock();
            let mut last = None;
            for _ in 0..batch_for(idx) {
                if let Ok(block) = heap.alloc(sc_layout) {
                    if let Some(prev) = last {
                        // Push previous into cache.
                        let _ = cache.caches[idx].try_push(prev, limit);
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
            let limit = cache_limit(idx);
            if cache.caches[idx].try_push(ptr, limit) {
                return true;
            }

            // Slow path: cache full. Drain half, then push this one.
            let (drained, n) = cache.caches[idx].drain_batch(batch_for(idx));
            let _ = cache.caches[idx].try_push(ptr, limit); // guaranteed to succeed

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
        let raw = vmalloc(reserve).expect("vmalloc: no address space for a heap expansion");

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

// ---------------------------------------------------------------------------
// Benchmark
// ---------------------------------------------------------------------------

/// What a kernel allocation costs, and whether that cost depends on how much
/// the heap already holds.
///
/// The userspace answer to the same question is `programs/allocbench`, and this
/// asks it in the same shape so the two can be read side by side: the floor at
/// each size, the cost against a live population, and what reuse costs after a
/// population is freed.
///
/// Read `/proc/alloc_bench` to run it. Three things bound how the numbers
/// should be read:
///
/// - **The first read is not like the ones after it.** The heap only ever
///   grows, so the first run pays for the expansions its population forces and
///   later runs find the memory already mapped. Read it twice.
/// - **Sizes up to 4096 bytes are answered by the running CPU's own cache**
///   and everything above them takes the one global heap lock, so the step
///   between 4096 and 16384 is the interesting part of the table.
/// - **It measures one CPU.** The per-CPU caches make contention invisible
///   here; what several CPUs allocating at once costs is a different question
///   and wants a different instrument.
pub mod bench {
    use alloc::{format, string::String, vec::Vec};
    use core::{alloc::Layout, hint::black_box, ptr::NonNull};

    use crate::timer::Instant;

    /// Sizes spanning the per-CPU classes and the global heap above them.
    const SIZES: [usize; 10] = [16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 16384];

    /// Sizes a population is built from: a few words for a node, a string, a
    /// buffer. The same four `allocbench` uses.
    const MIX: [usize; 4] = [24, 64, 256, 1024];

    fn layout(size: usize) -> Layout {
        // Every request in this benchmark is a plain byte buffer, so the
        // alignment is the one a `Vec<u8>` would ask for.
        unsafe { Layout::from_size_align_unchecked(size, 1) }
    }

    /// Allocates and touches one block, so a mapping that is never written
    /// cannot make the number look better than the allocation was.
    fn take(size: usize) -> Option<NonNull<u8>> {
        let ptr = unsafe { alloc::alloc::alloc(layout(size)) };
        let ptr = NonNull::new(ptr)?;
        unsafe { ptr.write(1) };
        Some(ptr)
    }

    fn give(ptr: NonNull<u8>, size: usize) {
        unsafe { alloc::alloc::dealloc(ptr.as_ptr(), layout(size)) };
    }

    /// Allocate and free the same size in a loop. Nothing accumulates, so this
    /// is the floor: a cache hit and a cache push.
    fn churn(out: &mut String) {
        out.push_str("churn      alloc+free, nothing live\n");
        for size in SIZES {
            let rounds = if size <= 4096 { 20_000 } else { 4_000 };
            let start = Instant::now();
            for _ in 0..rounds {
                match take(size) {
                    Some(ptr) => {
                        black_box(ptr);
                        give(ptr, size);
                    }
                    None => {
                        out.push_str(&format!("  {size:>6} bytes: out of memory\n"));
                        return;
                    }
                }
            }
            let each = start.elapsed().as_nanos() as u64 / rounds as u64;
            let path = if super::size_class_index(size, 1).is_some() {
                "percpu"
            } else {
                "global"
            };
            out.push_str(&format!("  {size:>6} bytes: {each:>6} ns/op  ({path})\n"));
        }
    }

    /// Hold `live` blocks, then time allocations made against that population.
    ///
    /// Every other block is freed first, so the heap holds a run of holes
    /// rather than one clean tail to carve from.
    fn scaling(out: &mut String) {
        out.push_str("scaling    cost against a live population\n");
        for live in [500usize, 2_000, 8_000] {
            let mut held: Vec<Option<NonNull<u8>>> = Vec::with_capacity(live);
            for i in 0..live {
                held.push(take(MIX[i % MIX.len()]));
            }
            for i in (0..held.len()).step_by(2) {
                if let Some(ptr) = held[i].take() {
                    give(ptr, MIX[i % MIX.len()]);
                }
            }

            const SAMPLES: usize = 2_000;
            let mut sink: Vec<Option<NonNull<u8>>> = Vec::with_capacity(SAMPLES);
            let start = Instant::now();
            for i in 0..SAMPLES {
                sink.push(take(MIX[i % MIX.len()]));
            }
            let each = start.elapsed().as_nanos() as u64 / SAMPLES as u64;
            out.push_str(&format!("  {live:>6} live : {each:>6} ns/alloc\n"));

            for (i, slot) in sink.into_iter().enumerate() {
                if let Some(ptr) = slot {
                    give(ptr, MIX[i % MIX.len()]);
                }
            }
            for (i, slot) in held.into_iter().enumerate() {
                if let Some(ptr) = slot {
                    give(ptr, MIX[i % MIX.len()]);
                }
            }
        }
    }

    /// Free a large population and allocate again at the same sizes. An
    /// allocator that reuses what it just freed answers from its own lists.
    fn reuse(out: &mut String) {
        const N: usize = 8_000;
        let mut held: Vec<Option<NonNull<u8>>> = Vec::with_capacity(N);
        for i in 0..N {
            held.push(take(MIX[i % MIX.len()]));
        }
        for (i, slot) in held.iter_mut().enumerate() {
            if let Some(ptr) = slot.take() {
                give(ptr, MIX[i % MIX.len()]);
            }
        }

        let start = Instant::now();
        for i in 0..N {
            held[i] = take(MIX[i % MIX.len()]);
        }
        let each = start.elapsed().as_nanos() as u64 / N as u64;
        out.push_str(&format!("reuse      after freeing {N}: {each} ns/alloc\n"));

        for (i, slot) in held.into_iter().enumerate() {
            if let Some(ptr) = slot {
                give(ptr, MIX[i % MIX.len()]);
            }
        }
    }

    /// Runs the benchmark and renders its table.
    pub fn render() -> String {
        let mut out = String::new();
        churn(&mut out);
        scaling(&mut out);
        reuse(&mut out);

        let heap = super::ALLOCATOR.inner.lock();
        let (used, total) = (heap.stats_alloc_actual(), heap.stats_total_bytes());
        drop(heap);
        out.push_str(&format!(
            "heap       {} KiB used of {} KiB mapped\n",
            used / 1024,
            total / 1024
        ));
        out
    }
}

pub fn print_alloc_stats() {
    let used = ALLOCATOR.inner.lock().stats_alloc_actual();
    let size = ALLOCATOR.inner.lock().stats_total_bytes();

    println!("Kernel heap {} kb / {} kb", used / 1024, size / 1024);
}
