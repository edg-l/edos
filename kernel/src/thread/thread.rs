use core::{
    cell::UnsafeCell,
    ops::Deref,
    sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};

use alloc::{
    collections::btree_map::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use spin::{Mutex, RwLock};
use x86_64::{
    PhysAddr, VirtAddr,
    instructions::interrupts::without_interrupts,
    registers::control::{Cr3, Cr3Flags},
    structures::paging::{OffsetPageTable, PageTableFlags, PhysFrame},
};

use intrusive_list::Link;

use crate::thread::preempt::{PreemptRwLock, PreemptSpinlock};
use crate::{
    boot::boot_info,
    debug::lock_order::{RANK_MAPPERS, RANK_USER_MM, RANK_VMAS},
    drivers::fpu::FpuState,
    fs::{self, inode::VfsInode, path::Path},
    loader::{ElfLoadError, TlsTemplate, load_elf},
    memory::{
        USER_STACK_SIZE, USER_STACK_TOP,
        frame_allocator::frame_allocator,
        mapper::{MemoryManager, get_level_4_table},
        shared::SharedMemory,
        vma::{Vma, VmaBacking, VmaFlags, VmaProt, VmaSet},
    },
    println, ranked_lock,
    syscalls::Errno,
    thread::{
        UserThread, UserThreadInfo, UserThreadTls,
        cancel::{ArcCancellableOp, OWNED_OPS_CAP},
        context::CpuContext,
        fd::FileDescriptorTable,
        irqlock::IrqSpinlock,
        mutex::BlockingMutex,
        paging::allocate_process_pml4,
        runqueue::{DEFAULT_PRIORITY, PRIORITY_LEVELS},
        scheduler::switch_to_kernel_page,
        signal::SignalState,
        util::{kthread_stack_alloc, kthread_stack_free, thread_stack_free},
        {ProcessAuxv, setup_user_stack},
    },
    timer::Instant,
    window,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ThreadId(pub u64);

impl Deref for ThreadId {
    type Target = u64;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Sentinel for `Thread::last_syscall` meaning "thread has never entered a
/// syscall since spawn." Distinct from any real syscall number (SYS_READ=0
/// rules out 0; we use the high end of u32 instead).
pub const NO_SYSCALL: u32 = u32::MAX;

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Ready = 0,
    Running = 1,
    Sleeping = 2,
    Parked = 3,
    Waking = 4,
    Dying = 5,
}

/// Valid state machine transitions:
///   Ready    -> Running  (scheduler picks thread)
///   Running  -> Ready    (preempted, re-enqueued)
///   Running  -> Parked   (thread_park / thread_park_while)
///   Running  -> Sleeping (thread_sleep)
///   Running  -> Dying    (thread_exit)
///   Parked   -> Waking   (try_wake)
///   Parked   -> Running  (park abort: token consumed or closure said don't park)
///   Sleeping -> Waking   (try_wake)
///   Sleeping -> Running  (sleep abort: token consumed)
///   Waking   -> Ready    (complete_wake)
fn is_valid_transition(from: State, to: State) -> bool {
    matches!(
        (from, to),
        (State::Ready, State::Running)
            | (State::Running, State::Ready)
            | (State::Running, State::Parked)
            | (State::Running, State::Sleeping)
            | (State::Running, State::Dying)
            | (State::Parked, State::Waking)
            | (State::Parked, State::Running)
            | (State::Sleeping, State::Waking)
            | (State::Sleeping, State::Running)
            | (State::Waking, State::Ready)
    )
}

impl From<u8> for State {
    fn from(v: u8) -> Self {
        match v {
            0 => State::Ready,
            1 => State::Running,
            2 => State::Sleeping,
            3 => State::Parked,
            4 => State::Waking,
            5 => State::Dying,
            _ => unreachable!("invalid thread state: {v}"),
        }
    }
}

bitflags::bitflags! {
    pub struct Flags: u32 {
        const NEED_RESCHED = 1<<0;   // set by syscalls/irq to request preemption
        const PENDING_SIG  = 1<<1;   // optional
    }
}

#[derive(Debug)]
pub struct Thread {
    pub id: ThreadId,
    pub cpu: AtomicU32,
    pub name: Arc<String>,

    // Scheduling-visible fields as atomics:
    pub state: AtomicU8,         // State as u8
    pub priority: AtomicU8,      // 0..16 small static priority, higher means more prio
    pub cpu_affinity: AtomicU32, // bitmask of allowed CPUs
    pub flags: AtomicU32,        // Flags

    // Every field below is an `Instant` in nanoseconds, with 0 meaning unset.
    pub slice_deadline: AtomicU64,

    // Sleep data split to avoid locking:
    pub sleep_deadline: AtomicU64,

    // CPU time accounting: total, and the start of the current run.
    pub cpu_time_ns: AtomicU64,
    pub run_start_ns: AtomicU64,

    // Thread creation, for the wall lifetime reported on exit.
    pub created_at_ns: AtomicU64,
    // Demand page faults served on behalf of this thread (userspace-triggered).
    pub demand_faults: AtomicU32,

    pub tls_base: AtomicU64,

    /// Thread that created this one, or 0 for kernel threads and for children
    /// whose creator has since exited without an init to inherit them.
    ///
    /// Only used for lifetime bookkeeping: an exit record nobody can collect is
    /// dropped rather than kept forever, and orphans are handed to init.
    pub parent: AtomicU64,

    /// Process group, or 0 meaning "this thread leads its own".
    ///
    /// The indirection is what makes a *job* addressable: a terminal delivers
    /// Ctrl+C to a group rather than a pid, so every process in a pipeline
    /// gets it. Read through [`Thread::pgid`], never directly, because 0 is a
    /// sentinel and not a group.
    pub pgid: AtomicU64,

    pub exit_code: AtomicI32,
    /// Set when the process has been killed (e.g. by Ctrl+C). Sleeping syscalls
    /// check this flag on wakeup and return EINTR to unblock the process; the
    /// thread then dies at whichever boundary it reaches first, the syscall
    /// return or a timer tick out of user code.
    pub killed: AtomicBool,

    /// Set by a stop signal and cleared by `SIGCONT`.
    ///
    /// A thread is not suspended where the signal lands — it is suspended at
    /// the next boundary where it provably holds nothing, which is the same
    /// boundary `killed` uses. Between the two it keeps running, exactly as a
    /// thread marked for death does.
    pub stop_requested: AtomicBool,
    /// Set while the thread is parked in `stop_if_signalled`, so `ps` and the
    /// shell can tell a suspended job from one blocked in a syscall.
    pub stopped: AtomicBool,

    /// Wake-pending token. Wakers publish their intent here BEFORE probing
    /// `state`; parkers consume it (swap to false) AFTER CAS to Parked/Sleeping
    /// and revert to Running if it was set. Closes the lost-wakeup race for
    /// every park/sleep site without relying on retry loops or condition
    /// rechecks. Multiple wakes coalesce into one consume.
    pub wake_pending: AtomicBool,

    /// Syscall number of the most recent SYSCALL entry, or `NO_SYSCALL`
    /// (`u32::MAX`) if the thread has never entered a syscall (kthreads).
    /// Written by the syscall dispatcher; read by debug tooling. Single
    /// relaxed store on a hot cache line — sub-cycle cost.
    pub last_syscall: AtomicU32,
    /// Trace-session generation this thread was marked under, or 0 for
    /// unmarked. Meaningful only against `syscalls::trace::TRACE_GEN`, which
    /// is what lets a session end without walking the thread table.
    pub traced: AtomicU64,
    /// Signal state: pending bitmask and per-signal disposition.
    pub signal: SignalState,

    pub user: Option<Arc<RwLock<UserThread>>>,

    /// The architectural `CR3` this thread runs on -- the PML4 frame address
    /// with the flag bits -- or 0 for a kernel thread, which runs on the
    /// kernel page tables.
    ///
    /// A mirror of `UserThread::cr3`, so the switch path can ask "has the
    /// address space changed" without taking the `RwLock` that `user` sits
    /// behind. That lock is the reason, not its cost: `execve` holds it for
    /// writing while it installs an image, and a switch to a sibling thread of
    /// that process would spin behind the guard inside the switch. (It is not
    /// worth measurable time -- see `doc/SCHED-ROADMAP.md`.)
    ///
    /// Write it only through [`Thread::set_user_cr3`], which sets both halves
    /// under the write guard, or in a `Thread` literal beside the `UserThread`
    /// it mirrors. A stale value here runs a thread on another process's page
    /// tables, so the four sites that install an address space are enforced by
    /// the field being mandatory rather than defaulted.
    pub user_cr3: AtomicU64,

    // Context and kernel stack pointer:
    pub ctx: Mutex<CpuContext>, // only scheduler touches ctx under lock
    pub kstack_top: u64,

    // Intrusive runqueue link — only touched while the runqueue lock is held.
    pub rq_link: Link,

    // Set after save_current_thread writes valid ctx, cleared when the
    // thread starts running via context_switch_to. Work-stealing skips
    // threads where this is false to avoid loading stale register state.
    pub context_saved: AtomicBool,

    // FPU state — only the running CPU touches this during context switch.
    // UnsafeCell because it's accessed without a lock (only current CPU writes).
    pub fpu: UnsafeCell<FpuState>,
    pub fpu_init: AtomicBool,
    /// The CPU that last loaded this thread's FPU state into its registers, or
    /// `u32::MAX` for none.
    ///
    /// Paired with that CPU's `fpu_owner`, and both halves are needed to skip a
    /// reload: this side alone cannot see that the CPU has since run another
    /// thread, and that side alone cannot see that this thread has since run
    /// somewhere else and changed its registers there.
    pub fpu_cpu: AtomicU32,

    /// Async operations the thread currently owns.  On wake-after-completion
    /// the op unregisters itself (`owned_ops_remove`).  On death, `Thread::free`
    /// calls `owned_ops_cancel_all` which drains this list and invokes
    /// `cancel()` on every entry.
    ///
    /// `IrqSpinlock` because cancel enumeration can overlap contexts where
    /// the global allocator is held; we avoid any blocking locks.  Pushes
    /// happen on the submitting thread BEFORE park, outside scheduler/IRQ paths.
    pub owned_ops: IrqSpinlock<heapless::Vec<ArcCancellableOp, OWNED_OPS_CAP>>,

    /// Per-thread lock-rank stack for debug-only lock-order enforcement.
    ///
    /// Only read/written by the thread that owns this `Thread` struct, via
    /// `crate::debug::lock_order::enter` and `exit`. The `enter()` helper
    /// asserts `current_thread_id() == self.id` before every access.
    ///
    /// `UnsafeCell` is required because Rust's aliasing rules prohibit
    /// interior mutability through a shared `&Thread` reference without it.
    /// The single-owner invariant makes concurrent access impossible: no
    /// other CPU thread ever reaches into another thread's rank stack.
    ///
    /// Absent in release builds; carries zero size and zero runtime overhead.
    #[cfg(debug_assertions)]
    #[allow(dead_code)]
    pub lock_ranks: core::cell::UnsafeCell<
        heapless::Vec<(u16, &'static str), { crate::debug::lock_order::LOCK_RANK_DEPTH }>,
    >,
}

intrusive_list::impl_linked!(Thread, rq_link);

// SAFETY: The UnsafeCell<FpuState> field is only accessed by the CPU currently
// running this thread (during save_current_thread / context_switch_to), never
// concurrently from multiple CPUs.
unsafe impl Sync for Thread {}

// For now kernel threads and user share id
static THREAD_ID_NEXT_ID: AtomicU64 = AtomicU64::new(1);

const TLS_REGION_STRIDE: u64 = 0x20000; // 128 KiB per-thread slot
const TLS_GUARD_GAP: u64 = 0x20000; // Keep a gap below the user stack
const TLS_TCB_MIN_SIZE: u64 = 64;
const PAGE_SIZE: u64 = 4096;

/// A complete process image in a fresh address space, not yet attached to any
/// thread.
///
/// Building the image before touching the caller is what makes `execve`
/// tractable: the new address space is fully constructed while the old one is
/// still live and running this code, so a failure anywhere in the load leaves
/// the caller untouched and the syscall can return an error.
pub(crate) struct LoadedImage {
    pub pml4_frame: PhysFrame,
    pub memory_manager: MemoryManager,
    pub vma_set: Arc<PreemptSpinlock<VmaSet>>,
    pub entry_point: VirtAddr,
    pub user_stack_pointer: u64,
    pub argc: usize,
    pub argv_ptr: u64,
    pub envp_ptr: u64,
    pub heap_break: u64,
    pub tls: Option<UserThreadTls>,
    pub tls_fs_base: u64,
    pub stack_top: u64,
    /// The arguments the image was started with, space-joined, for procfs.
    /// The stack copy lives in user memory, which the kernel cannot read once
    /// the process has had a chance to overwrite it.
    pub cmdline: Arc<String>,
}

/// Render `argv` as one line for `/proc/<tid>/cmdline`, falling back to the
/// path for an image started with no arguments at all.
fn cmdline_of(path: &Path, argv: &[&[u8]]) -> Arc<String> {
    let mut out = String::new();
    for arg in argv {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&String::from_utf8_lossy(arg));
    }
    if out.is_empty() {
        out = alloc::format!("{path}");
    }
    Arc::new(out)
}

/// Build a fresh address space and load `inode` into it, with `argv`/`envp`
/// already pushed onto the new user stack.
///
/// Shared by process creation ([`Thread::new_user`]) and `execve`, which differ
/// only in what they attach the result to.
pub(crate) fn load_process_image(
    inode: &Arc<VfsInode>,
    path: &Path,
    argv: &[&[u8]],
    envp: &[&[u8]],
) -> Result<LoadedImage, ElfLoadError> {
    let kernel_pml4 = boot_info().cr3;
    let physical_memory_offset = boot_info().physical_memory_offset;
    let kernel_table = unsafe { get_level_4_table(kernel_pml4) };
    let page = unsafe { allocate_process_pml4(kernel_table) };

    // Build the new page table via HHDM (no CR3 switch needed).
    let child_page_table = unsafe { get_level_4_table((page, kernel_pml4.1)) };
    let table = unsafe { OffsetPageTable::new(child_page_table, physical_memory_offset) };

    let mut memory_manager = MemoryManager::new(table);
    memory_manager.pml4_frame = Some(page);

    // Build VmaSet early with the Stack VMA so copy_to_user can demand-fault
    // stack pages during setup_user_stack.
    let stack_top = USER_STACK_TOP.as_u64();
    let vma_set = Arc::new(PreemptSpinlock::new(VmaSet::new()));
    let stack_bottom = VirtAddr::new(stack_top - USER_STACK_SIZE);
    vma_set
        .lock()
        .insert(Vma {
            start: stack_bottom,
            end: USER_STACK_TOP,
            prot: VmaProt::READ | VmaProt::WRITE,
            flags: VmaFlags::PRIVATE | VmaFlags::GROWSDOWN | VmaFlags::LAZY,
            backing: VmaBacking::Stack,
        })
        .map_err(|_| ElfLoadError::MappingFailed)?;
    memory_manager.vmas = Some(vma_set.clone());

    let mut load_info = load_elf(inode, path, &mut memory_manager)?;

    // The stack is built after the image because the auxiliary vector describes
    // where the image landed: AT_PHDR and AT_ENTRY are not known until the ELF
    // has been parsed.
    let aux = ProcessAuxv {
        phdr: load_info.phdr_vaddr,
        phentsize: load_info.phentsize,
        phnum: load_info.phnum,
        entry: load_info.entry_point.as_u64(),
        base: 0,
    };
    let execfn = alloc::format!("{path}");
    let (user_stack_pointer, argv_ptr, argc, envp_ptr) = setup_user_stack(
        stack_top,
        argv,
        envp,
        execfn.as_bytes(),
        &aux,
        &memory_manager,
    )
    .map_err(|_| ElfLoadError::MappingFailed)?;

    // Reloc table and VMA range live on the MemoryManager for lazy fault-time
    // application; load_base is what relocated values are computed against.
    memory_manager.reloc_table = load_info.reloc_table.take();
    memory_manager.reloc_vma_range = load_info.reloc_vma_range.take();
    memory_manager.load_base = load_info.load_base;

    let mut tls_runtime: Option<UserThreadTls> = None;
    let mut tls_fs_base = 0u64;

    if let Some(template) = load_info.tls_template.take() {
        let template = Arc::new(template);
        let allocation = allocate_tls_region(&template, 0, &mut memory_manager)?;
        tls_fs_base = allocation.fs_base;
        vma_set
            .lock()
            .insert(allocation.vma)
            .map_err(|_| ElfLoadError::MappingFailed)?;
        tls_runtime = Some(allocation.runtime);
    }

    {
        let mut vmas = vma_set.lock();
        for vma in load_info.memory_regions {
            vmas.insert(vma).map_err(|_| ElfLoadError::InvalidSegment)?;
        }
    }

    Ok(LoadedImage {
        pml4_frame: page,
        memory_manager,
        vma_set,
        entry_point: load_info.entry_point,
        user_stack_pointer,
        argc,
        argv_ptr,
        envp_ptr,
        heap_break: load_info.heap_break,
        tls: tls_runtime,
        tls_fs_base,
        stack_top,
        cmdline: cmdline_of(path, argv),
    })
}

/// Unmap and release every user mapping in `user`'s address space.
///
/// Shared by process teardown and `execve`, which need exactly the same work
/// and differ only in what happens next. Deliberately excluded:
///
/// - **The file descriptor table**, which survives an exec. Process exit closes
///   it separately, before calling this.
/// - **The PML4 frame**, because the caller decides what CR3 to run on next:
///   exit switches to the kernel table and frees it, exec switches to the new
///   image's table and frees the old one.
/// - **Per-thread TLS**, which each thread releases in its own `free`.
///
/// The caller must hold no `mm` or `vmas` guard, and must have established that
/// no other thread can still be running in this address space.
pub(crate) fn release_user_mappings(user: &UserThread) {
    // vmas (rank 70) is taken before mm (rank 80).
    let vmas = ranked_lock!(RANK_VMAS, "user.vmas", user.vmas).clone();
    // Before mm, never under it: writing a shared mapping back goes through
    // the VFS (rank 10), and `sys_mmap` resolves its path under the VFS and
    // then takes mm. Doing both in that order here would be the inversion of
    // that, and the two together are a deadlock.
    flush_shared_mappings(&vmas);
    let mut memory_manager = ranked_lock!(RANK_USER_MM, "user.mm", user.memory_manager);
    let stack_top = user.process_stack_top.load(Ordering::Acquire);
    release_mappings(&mut memory_manager, &vmas, user.pid, stack_top);
}

/// Write back every dirty page of every `MAP_SHARED` file mapping in `vmas`.
///
/// Linux's msync-on-exit semantics: a process that stored through a shared
/// mapping and exited without unmapping still has its writes reach the disk.
///
/// Separate from [`release_mappings`] because of the lock ladder, not for
/// tidiness. This reaches the filesystem, so it must run with no `mm` guard
/// held; the unmapping that follows needs that guard. Errors are logged and
/// never stop the teardown.
pub(crate) fn flush_shared_mappings(vmas: &VmaSet) {
    for vma in vmas.iter() {
        if let VmaBacking::FileBacked {
            inode,
            file_offset,
            shared: true,
            pages,
            ..
        } = &vma.backing
        {
            crate::syscalls::memory::flush_shared_vma_pages(inode, *file_offset, pages);
        }
    }
}

/// The mapping teardown itself, over an address space that may already be
/// detached from any process.
///
/// `execve` detaches the outgoing `MemoryManager` and `VmaSet` before calling
/// this, so that the process is already running on its new address space and a
/// preemption in the middle cannot restore a page table that is being freed.
///
/// Shared mappings must already have been written back by
/// [`flush_shared_mappings`]: this function only unmaps.
pub(crate) fn release_mappings(
    memory_manager: &mut MemoryManager,
    vmas: &VmaSet,
    pid: u64,
    stack_top: u64,
) {
    // Windows belong to the image being torn down; the pid may live on.
    window::cleanup_process_windows(pid);
    for vma in vmas.iter() {
        match &vma.backing {
            VmaBacking::Anonymous => {
                let _ = memory_manager.unmap_memory(vma.start, vma.size());
            }
            VmaBacking::Tls => {
                // Already cleaned up above (per-thread TLS cleanup)
            }
            VmaBacking::SharedMemory { shm_id } => {
                use x86_64::structures::paging::{Mapper, Page, Size4KiB};
                let page_count = (vma.size() + 0xFFF) / 4096;
                for i in 0..page_count {
                    let virt_addr = VirtAddr::new(vma.start.as_u64() + i * 4096);
                    let page: Page<Size4KiB> = Page::containing_address(virt_addr);
                    if let Ok((_, flush)) = memory_manager.mapper.unmap(page) {
                        flush.flush();
                    }
                }
                if let Some(shm) = SharedMemory::get(*shm_id) {
                    shm.dec_ref();
                }
            }
            VmaBacking::Physical { .. } => {
                use x86_64::structures::paging::{Mapper, Page, Size4KiB};
                let page_count = (vma.size() + 0xFFF) / 4096;
                for i in 0..page_count {
                    let virt_addr = VirtAddr::new(vma.start.as_u64() + i * 4096);
                    let page: Page<Size4KiB> = Page::containing_address(virt_addr);
                    if let Ok((_, flush)) = memory_manager.mapper.unmap(page) {
                        flush.flush();
                    }
                }
            }
            VmaBacking::Stack => {
                // Handled below by thread_stack_free
            }
            VmaBacking::FileBacked {
                inode,
                file_offset,
                shared,
                pages,
                ..
            } => {
                // Shared pages were written back by `flush_shared_mappings`
                // before the mm guard this runs under was taken.
                let _ = (inode, file_offset, shared, pages);
                // Unmap each present PTE and decrement the frame refcount
                // that was bumped at fault-in time. Drop the pages Vec
                // AFTER the dec_refcount loop so the Arc<CachedPage>
                // refs are released last; the BTreeMap entry in
                // inode.pages keeps the cache frame alive.
                use x86_64::structures::paging::{Mapper, Page, Size4KiB};
                let page_count = (vma.size() + 0xFFF) / 4096;
                let mut fa = frame_allocator();
                for i in 0..page_count {
                    let virt_addr = VirtAddr::new(vma.start.as_u64() + i * 4096);
                    let page: Page<Size4KiB> = Page::containing_address(virt_addr);
                    if let Ok(phys) = memory_manager.mapper.translate_page(page) {
                        if let Ok((_, flush)) = memory_manager.mapper.unmap(page) {
                            flush.ignore();
                            fa.dec_refcount(phys);
                        }
                    }
                }
                // VMA (including its inode Arc) drops at end of this
                // arm. If this was the final Arc and the inode was
                // previously orphaned, VfsInode::drop triggers
                // FileSystem::evict_inode to free on-disk allocations.
            }
        }
    }

    thread_stack_free(memory_manager, stack_top);

    // No TLB shootdown needed: each process has its own address space
    // (address_space_refs starts at 1, COW fork creates a new PML4), and
    // `execve` has already established that no sibling is left running in it.
    memory_manager.clean_lower_half();
}

/// Terminate every other thread sharing `caller`'s address space and wait for
/// them to release it.
///
/// `execve` replaces an address space that siblings may be executing in, so it
/// must establish that no other CPU can touch it before anything is unmapped.
/// `address_space_refs` reaching 1 is that proof: a thread only decrements it
/// in `Thread::free`, after it has stopped running.
///
/// Returns false on timeout, in which case the caller must abandon the exec.
/// Unmapping an address space out from under a running thread would be memory
/// corruption, so refusing is the only safe answer. A sibling observes `killed`
/// at its next syscall boundary or, if it makes none, at its next timer tick
/// out of user code; a sibling spinning inside the kernel is the case that can
/// still outlast the wait.
pub(crate) fn quiesce_address_space(caller: &Arc<Thread>) -> bool {
    use crate::thread::scheduler::{WakePriority, sched, thread_sleep};

    let Some(caller_user) = caller.user.as_ref() else {
        return true;
    };
    // `sys_clone` gives each thread its own `UserThread`, sharing the inner
    // Arcs, so thread identity is not address-space identity. The refcount Arc
    // is: it is created once per address space and cloned into every thread
    // that joins it.
    let caller_space = caller_user.read().address_space_refs.clone();

    for thread in THREADS.list() {
        if thread.id == caller.id {
            continue;
        }
        let shares = thread
            .user
            .as_ref()
            .is_some_and(|u| Arc::ptr_eq(&u.read().address_space_refs, &caller_space));
        if !shares {
            continue;
        }

        thread.killed.store(true, Ordering::Release);
        thread.exit_code.store(0, Ordering::Release);
        sched().wake_thread(&Arc::downgrade(&thread), WakePriority::Normal);
    }

    // One second is far longer than a thread needs to reach its next syscall
    // boundary; anything still alive after it is not going to die on its own.
    const ATTEMPTS: u32 = 200;
    for _ in 0..ATTEMPTS {
        if caller_user
            .read()
            .address_space_refs
            .load(Ordering::Acquire)
            == 1
        {
            return true;
        }
        thread_sleep(Duration::from_millis(5));
    }

    caller_user
        .read()
        .address_space_refs
        .load(Ordering::Acquire)
        == 1
}

pub(crate) struct TlsAllocation {
    pub runtime: UserThreadTls,
    pub vma: Vma,
    pub fs_base: u64,
}

fn align_up_u64(value: u64, align: u64) -> u64 {
    if align <= 1 {
        value
    } else {
        value.checked_add(align - 1).unwrap_or(u64::MAX) / align * align
    }
}

fn align_down_u64(value: u64, align: u64) -> u64 {
    if align <= 1 {
        value
    } else {
        value / align * align
    }
}

pub(crate) fn allocate_tls_region(
    template: &Arc<TlsTemplate>,
    tls_slot: u64,
    memory_manager: &mut MemoryManager,
) -> Result<TlsAllocation, ElfLoadError> {
    let align = template.align.max(1);
    let init_len = template.init_data.len() as u64;
    let data_size = align_up_u64(template.mem_size.max(init_len), align);

    // Reserve extra space for alignment padding between TLS block and TCB.
    let total_required = data_size
        .saturating_add(TLS_TCB_MIN_SIZE)
        .saturating_add(align);
    let map_size = align_up_u64(total_required, PAGE_SIZE);

    if map_size > TLS_REGION_STRIDE {
        return Err(ElfLoadError::MappingFailed);
    }

    let stack_bottom = USER_STACK_TOP.as_u64().saturating_sub(USER_STACK_SIZE);
    let base_anchor = stack_bottom.saturating_sub(TLS_GUARD_GAP);
    let slot_offset = tls_slot
        .checked_mul(TLS_REGION_STRIDE)
        .ok_or(ElfLoadError::MappingFailed)?;

    if slot_offset >= base_anchor {
        return Err(ElfLoadError::MappingFailed);
    }

    let region_top = base_anchor - slot_offset;
    let region_top_aligned = align_down_u64(region_top, PAGE_SIZE);
    if region_top_aligned < map_size {
        return Err(ElfLoadError::MappingFailed);
    }

    let mapping_base_u64 = region_top_aligned - map_size;
    let mapping_base = VirtAddr::new(mapping_base_u64);
    let tls_region_top = VirtAddr::new(region_top_aligned);

    let flags =
        PageTableFlags::USER_ACCESSIBLE | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;

    memory_manager
        .map_memory(mapping_base, map_size, flags)
        .map_err(|_| ElfLoadError::MappingFailed)?;

    // Layout: [padding][TLS data][TCB]
    let min_tcb_base_u64 = tls_region_top.as_u64().saturating_sub(TLS_TCB_MIN_SIZE);
    let aligned_tcb_base_u64 = align_down_u64(min_tcb_base_u64, align);
    let tcb_base_u64 = aligned_tcb_base_u64.max(mapping_base_u64 + data_size);

    if tcb_base_u64 + TLS_TCB_MIN_SIZE > tls_region_top.as_u64() {
        let _ = memory_manager.unmap_memory(mapping_base, map_size);
        return Err(ElfLoadError::MappingFailed);
    }

    let tls_data_base_u64 = tcb_base_u64
        .checked_sub(data_size)
        .ok_or(ElfLoadError::MappingFailed)?;

    if tls_data_base_u64 < mapping_base_u64 {
        let _ = memory_manager.unmap_memory(mapping_base, map_size);
        return Err(ElfLoadError::MappingFailed);
    }

    let tls_data_base = VirtAddr::new(tls_data_base_u64);
    let tcb_base = VirtAddr::new(tcb_base_u64);

    // Safe under rank-80 mm because map_memory above is eager (PRESENT bit set on every
    // page before returning), so translate_to_hhdm_ptr takes its fast path and never
    // acquires rank-70 vmas. See doc/invariants/lock-order.md rank-80 note.
    memory_manager.zero_user(mapping_base, map_size as usize);

    if !template.init_data.is_empty() {
        memory_manager.copy_to_user(tls_data_base, &template.init_data);
    }

    memory_manager.write_val_to_user::<u64>(tcb_base, tcb_base.as_u64());

    let runtime = UserThreadTls {
        template: Arc::clone(template),
        data_base: tls_data_base,
        data_size,
        tcb_base,
        tcb_size: tls_region_top.as_u64() - tcb_base.as_u64(),
        mapping_base,
        mapping_size: map_size,
    };

    let vma = Vma {
        start: mapping_base,
        end: mapping_base + map_size,
        prot: VmaProt::READ | VmaProt::WRITE,
        flags: VmaFlags::PRIVATE,
        backing: VmaBacking::Tls,
    };

    Ok(TlsAllocation {
        runtime,
        vma,
        fs_base: tcb_base.as_u64(),
    })
}

impl Thread {
    /// Pack a `CR3` pair into the value the register itself holds: the PML4 is
    /// 4 KiB-aligned, so the flag bits live below it and neither loses
    /// information.
    pub fn encode_cr3((frame, flags): (PhysFrame, Cr3Flags)) -> u64 {
        frame.start_address().as_u64() | flags.bits()
    }

    fn decode_cr3(raw: u64) -> (PhysFrame, Cr3Flags) {
        (
            PhysFrame::containing_address(PhysAddr::new(raw & 0x000f_ffff_ffff_f000)),
            Cr3Flags::from_bits_truncate(raw & 0xfff),
        )
    }

    /// Install an address space on this thread, keeping the mirror the switch
    /// path reads in step with the `UserThread` field it mirrors.
    ///
    /// The caller holds the `UserThread` write guard, which is what makes the
    /// pair atomic with respect to anything that reads both.
    pub fn set_user_cr3(&self, user: &mut UserThread, cr3: (PhysFrame, Cr3Flags)) {
        user.cr3 = cr3;
        self.user_cr3
            .store(Self::encode_cr3(cr3), Ordering::Release);
    }

    pub fn switch_to_page(&self) {
        let target = self.user_cr3.load(Ordering::Acquire);
        if target == 0 {
            switch_to_kernel_page();
            return;
        }
        let (frame, flags) = Self::decode_cr3(target);
        if Cr3::read().0.start_address() != frame.start_address() {
            unsafe { Cr3::write(frame, flags) };
        }
    }

    pub fn state(&self) -> State {
        State::from(self.state.load(Ordering::Acquire))
    }

    pub fn mark_need_resched(&self) {
        self.flags
            .fetch_or(Flags::NEED_RESCHED.bits(), Ordering::AcqRel);
    }

    /// Publish a wake intent. Producers call this BEFORE probing `state`.
    /// If the parker reaches its CAS-to-Parked after this returns, it will
    /// observe the token in `consume_wake_pending` and abort park.
    #[inline]
    pub fn signal_wake(&self) {
        self.wake_pending.store(true, Ordering::Release);
    }

    /// Consume a pending wake. Parkers call this AFTER CAS-to-Parked (or
    /// CAS-to-Sleeping). Returns true if a wake was published since the
    /// last consume.
    #[inline]
    pub fn consume_wake_pending(&self) -> bool {
        self.wake_pending.swap(false, Ordering::AcqRel)
    }

    pub fn set_priority(&self, prio: u8) {
        self.priority
            .store(prio.min((PRIORITY_LEVELS - 1) as u8), Ordering::Release);
        self.mark_need_resched();
    }

    pub fn priority(&self) -> u8 {
        self.priority.load(Ordering::Acquire)
    }

    /// Restrict this thread to the CPUs named by `mask`; zero means anywhere.
    ///
    /// Affinity is enforced where a thread is *placed* — spawn, wake, and
    /// work-stealing — not where it is picked, so a mask set on a thread that
    /// is already running takes effect at its next placement rather than
    /// immediately. Set it before publishing the thread when the first
    /// placement has to honour it.
    #[cfg_attr(not(feature = "sched-test"), allow(dead_code))]
    pub fn set_affinity_mask(&self, mask: u32) {
        self.cpu_affinity.store(mask, Ordering::Release);
        self.mark_need_resched();
    }

    /// Whether this thread's affinity permits `cpu`.
    ///
    /// An empty mask means unrestricted, which is every thread today. A set
    /// mask is 32 bits wide and so cannot name a CPU above 31; a thread pinned
    /// by mask never runs on those, which is why the shift is guarded rather
    /// than wrapped.
    pub fn allows_cpu(&self, cpu: u32) -> bool {
        let mask = self.cpu_affinity.load(Ordering::Acquire);
        mask == 0 || (cpu < 32 && mask & (1u32 << cpu) != 0)
    }

    pub fn begin_run(&self, start_ns: u64) {
        self.run_start_ns.store(start_ns, Ordering::Release);
    }

    pub fn end_run(&self, end_ns: u64) {
        let start_ns = self.run_start_ns.swap(0, Ordering::AcqRel);
        if start_ns == 0 {
            return;
        }

        let elapsed_ns = end_ns.saturating_sub(start_ns);
        if elapsed_ns == 0 {
            return;
        }

        self.cpu_time_ns.fetch_add(elapsed_ns, Ordering::AcqRel);
    }

    /// The process group this thread belongs to.
    ///
    /// A stored 0 means the thread leads its own group, which is the right
    /// default for anything nobody has placed: a process started outside a job
    /// is its own job.
    pub fn pgid(&self) -> u64 {
        match self.pgid.load(Ordering::Acquire) {
            0 => self.id.0,
            pgid => pgid,
        }
    }

    pub fn cpu_time_ns(&self) -> u64 {
        let accumulated = self.cpu_time_ns.load(Ordering::Acquire);
        let start_ns = self.run_start_ns.load(Ordering::Acquire);

        if start_ns == 0 {
            return accumulated;
        }

        // Only report in-flight runtime while the scheduler still considers us running.
        if !matches!(self.state(), State::Running) {
            return accumulated;
        }

        let extra_ns = Instant::now().as_nanos().saturating_sub(start_ns);
        accumulated.saturating_add(extra_ns)
    }

    pub fn new_kernel(name: Option<String>, entry_point: u64, arg: u64) -> Arc<Self> {
        let initial_kstack_top = kthread_stack_alloc();
        let mut context =
            CpuContext::new_kernel_thread(entry_point as *const u8 as u64, initial_kstack_top);
        context.rdi = arg;

        let id = ThreadId(THREAD_ID_NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed));

        let name = Arc::new(name.unwrap_or_default());

        let thread = Arc::new(Self {
            id,
            kstack_top: initial_kstack_top,
            ctx: Mutex::new(context),
            state: AtomicU8::new(State::Ready as u8),
            name,
            user: None,
            user_cr3: AtomicU64::new(0), // kernel threads run on the kernel page tables
            cpu_affinity: AtomicU32::new(0),
            flags: AtomicU32::new(0),
            slice_deadline: AtomicU64::new(0),
            priority: AtomicU8::new(DEFAULT_PRIORITY),
            sleep_deadline: AtomicU64::new(0),
            cpu_time_ns: AtomicU64::new(0),
            run_start_ns: AtomicU64::new(0),
            created_at_ns: AtomicU64::new(Instant::now().as_nanos()),
            demand_faults: AtomicU32::new(0),
            tls_base: AtomicU64::new(0),
            parent: AtomicU64::new(0),
            cpu: AtomicU32::new(0),
            exit_code: AtomicI32::new(0),
            killed: AtomicBool::new(false),
            stop_requested: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            wake_pending: AtomicBool::new(false),
            last_syscall: AtomicU32::new(NO_SYSCALL),
            traced: AtomicU64::new(0),
            pgid: AtomicU64::new(0),
            signal: SignalState::new(),
            rq_link: Link::new(),
            context_saved: AtomicBool::new(true),
            fpu: UnsafeCell::new(FpuState::default()),
            fpu_init: AtomicBool::new(false),
            fpu_cpu: AtomicU32::new(u32::MAX),
            owned_ops: IrqSpinlock::new(heapless::Vec::new()),
            #[cfg(debug_assertions)]
            lock_ranks: core::cell::UnsafeCell::new(heapless::Vec::new()),
        });

        THREADS.insert(thread.clone());

        thread
    }

    /// Must provide entry point and cr3 page table.
    pub fn new_user(
        inode: Arc<VfsInode>,
        path: &Path,
        name: Option<String>,
        argv: &[&[u8]],
        envp: &[&[u8]],
        user: u32,
        group: u32,
        cwd: Path,
    ) -> Result<Arc<Self>, ElfLoadError> {
        let kernel_stack_top = kthread_stack_alloc();

        let image = load_process_image(&inode, path, argv, envp)?;

        let LoadedImage {
            pml4_frame: page,
            memory_manager: process_memory_manager,
            vma_set,
            entry_point,
            user_stack_pointer,
            argc,
            argv_ptr,
            envp_ptr,
            heap_break,
            tls: tls_runtime,
            tls_fs_base,
            stack_top,
            cmdline,
        } = image;

        let kernel_pml4 = boot_info().cr3;

        let id = ThreadId(THREAD_ID_NEXT_ID.fetch_add(1, Ordering::Relaxed));

        let mut context = CpuContext::new_user_thread(entry_point.as_u64(), user_stack_pointer);
        context.rdi = argc as u64;
        context.rsi = argv_ptr;
        context.rdx = envp_ptr;

        let name = Arc::new(name.unwrap_or_default());

        let mm = Arc::new(PreemptSpinlock::new(process_memory_manager));

        let address_space_refs = Arc::new(AtomicUsize::new(1));
        let process_stack_top = Arc::new(AtomicU64::new(stack_top));

        let user_cr3 = Self::encode_cr3((page, kernel_pml4.1));
        let user_state = Arc::new(RwLock::new(UserThread {
            pid: id.0,
            cr3: (page, kernel_pml4.1),
            memory_manager: mm.clone(),
            vmas: vma_set, // same Arc, shared between MemoryManager and UserThread
            tls: tls_runtime,
            heap_break,
            address_space_refs,
            process_stack_top,
            next_tls_slot: Arc::new(AtomicU64::new(1)), // slot 0 used by initial thread
            cmdline,
        }));

        // Task 2.9: Register the new process as a mapper of every FileBacked VMA
        // so that truncate/invalidate_mappings_above can unmap pages in this process.
        // Collect inode Arcs while holding the VmaSet lock, then register while
        // NOT holding the VmaSet lock to respect lock ordering (inode.mappers > vmas).
        // Registers this process as a mapper of every FileBacked VMA so that
        // truncate/invalidate_mappings_above can unmap pages in this process.
        {
            let file_backed_inodes: Vec<Arc<crate::fs::inode::VfsInode>> = {
                let user = user_state.read();
                let vmas = ranked_lock!(RANK_VMAS, "user.vmas", user.vmas);
                vmas.iter()
                    .filter_map(|vma| {
                        if let crate::memory::vma::VmaBacking::FileBacked { inode, .. } =
                            &vma.backing
                        {
                            Some(Arc::clone(inode))
                        } else {
                            None
                        }
                    })
                    .collect()
            };
            let weak = Arc::downgrade(&user_state);
            for inode in file_backed_inodes {
                ranked_lock!(RANK_MAPPERS, "inode.mappers", inode.mappers).push(weak.clone());
            }
        }

        let thread = Arc::new(Thread {
            id,
            kstack_top: kernel_stack_top,
            ctx: Mutex::new(context),
            state: AtomicU8::new(State::Ready as u8),
            name,
            cpu_affinity: AtomicU32::new(0),
            flags: AtomicU32::new(0),
            slice_deadline: AtomicU64::new(0),
            priority: AtomicU8::new(DEFAULT_PRIORITY),
            sleep_deadline: AtomicU64::new(0),
            cpu_time_ns: AtomicU64::new(0),
            run_start_ns: AtomicU64::new(0),
            created_at_ns: AtomicU64::new(Instant::now().as_nanos()),
            demand_faults: AtomicU32::new(0),
            tls_base: AtomicU64::new(tls_fs_base),
            parent: AtomicU64::new(0),
            cpu: AtomicU32::new(0),
            exit_code: AtomicI32::new(0),
            killed: AtomicBool::new(false),
            stop_requested: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            wake_pending: AtomicBool::new(false),
            last_syscall: AtomicU32::new(NO_SYSCALL),
            traced: AtomicU64::new(0),
            pgid: AtomicU64::new(0),
            signal: SignalState::new(),
            user: Some(user_state),
            user_cr3: AtomicU64::new(user_cr3),
            rq_link: Link::new(),
            context_saved: AtomicBool::new(true),
            fpu: UnsafeCell::new(FpuState::default()),
            fpu_init: AtomicBool::new(false),
            fpu_cpu: AtomicU32::new(u32::MAX),
            owned_ops: IrqSpinlock::new(heapless::Vec::new()),
            #[cfg(debug_assertions)]
            lock_ranks: core::cell::UnsafeCell::new(heapless::Vec::new()),
        });

        THREADS.insert(thread.clone());
        THREADS.insert_info(
            id,
            Arc::new(IrqSpinlock::new(UserThreadInfo {
                pid: id.0,
                errno: Errno::Clear,
                fd_table: Arc::new(BlockingMutex::new(FileDescriptorTable::new())),
                next_mmap_addr: Arc::new(AtomicU64::new(heap_break)),
                memory_manager: mm,
                cwd: Arc::new(BlockingMutex::new(cwd)),
                user_id: user,
                group_id: group,
            })),
        );

        Ok(thread)
    }

    /// Load a user thread from an ELF file on the filesystem.
    ///
    #[expect(unused)]
    pub fn new_user_from_path(
        path: &Path,
        name: Option<String>,
        argv: &[&[u8]],
        envp: &[&[u8]],
        user: u32,
        group: u32,
        cwd: Path,
    ) -> Result<Arc<Self>, ElfLoadError> {
        let inode = fs::api::resolve_inode(path).map_err(|e| {
            println!("Failed to resolve inode for {:?}: {:?}", path, e);
            ElfLoadError::MappingFailed
        })?;

        Self::new_user(inode, path, name, argv, envp, user, group, cwd)
    }

    pub fn free(&self) {
        debug_assert_eq!(
            self.state(),
            State::Dying,
            "free: thread {} not Dying (state={:?})",
            self.id.0,
            self.state()
        );
        debug_assert!(
            !self.rq_link.is_linked(),
            "free: thread {} still linked on runqueue",
            self.id.0
        );
        // Cancel all in-flight async operations before any resource teardown.
        // This releases AHCI slots and any other driver resources owned by
        // this thread, so that drivers don't observe dangling Weak<Thread>
        // references during their completion paths.
        self.owned_ops_cancel_all();

        kthread_stack_free(self.kstack_top);

        let Some(user_lock) = &self.user else {
            return;
        };

        let user = user_lock.write();
        let remaining = user.address_space_refs.fetch_sub(1, Ordering::AcqRel);
        let is_last_thread = remaining == 1;

        // Lock ordering note: vmas (rank 70) must be acquired before mm (rank 80).
        // This site uses mm to unmap TLS, then vmas to remove the VMA entry.
        // The operations are sequential (mm dropped before vmas), so no nesting occurs.
        let mut memory_manager = ranked_lock!(RANK_USER_MM, "user.mm", user.memory_manager);

        // Clean up this thread's TLS region (each thread has its own TLS slot).
        // Must happen for ALL threads, not just the last one, to avoid leaking
        // TLS pages in multi-threaded processes.
        if let Some(tls) = &user.tls {
            let tls_start = tls.mapping_base;
            let tls_size = tls.mapping_size;
            let _ = memory_manager.unmap_memory(tls_start, tls_size);
            // Drop mm (rank 80) before taking vmas (rank 70) to preserve order.
            drop(memory_manager);
            ranked_lock!(RANK_VMAS, "user.vmas", user.vmas).remove(&tls_start);
            // Re-acquire mm for subsequent unmap operations below.
            memory_manager = ranked_lock!(RANK_USER_MM, "user.mm", user.memory_manager);
        }

        if is_last_thread {
            // Close all file descriptors (pipes need proper shutdown for EOF)
            if let Some(info) = THREADS.get_info(self.id) {
                let fds: alloc::vec::Vec<(u64, super::pipe::FileDescriptor)> =
                    info.lock().fd_table.lock().drain_all();
                for (_fd_num, descriptor) in fds {
                    super::pipe::close_descriptor(descriptor, self.id.0);
                }
            }

            // The last thread out releases the address space. Everything but
            // the fd table (already drained above) and the PML4 frame.
            drop(memory_manager);
            release_user_mappings(&user);

            switch_to_kernel_page();

            // The reaper frees a thread before dropping it from the registry,
            // so a reader that snapshotted the registry -- procfs does -- can
            // still hold an Arc to this thread and reach its MemoryManager
            // after the frame below is back in the allocator. Say so under the
            // lock, before the frame goes.
            ranked_lock!(RANK_USER_MM, "user.mm", user.memory_manager).release_page_tables();

            let pml4 = user.cr3.0;
            unsafe { frame_allocator().deallocate_frame(pml4) };
        }
    }

    #[inline]
    pub fn cas_state(&self, from: State, to: State) -> bool {
        // Validate state transitions are legal
        debug_assert!(
            is_valid_transition(from, to),
            "cas_state: illegal transition {:?} -> {:?} for thread {}",
            from,
            to,
            self.id.0
        );
        self.state
            .compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn try_wake(&self) -> bool {
        loop {
            let state = self.state.load(Ordering::Acquire);

            match state {
                x if x == State::Sleeping as u8 || x == State::Parked as u8 => {
                    // Attempt to claim ownership of the wakeup
                    if self
                        .state
                        .compare_exchange(
                            state,
                            State::Waking as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        debug_assert!(
                            !self.rq_link.is_linked(),
                            "try_wake: thread {} linked on runqueue while Sleeping/Parked",
                            self.id.0
                        );
                        return true;
                    } else {
                        continue; // retry if raced
                    }
                }
                x if x == State::Ready as u8 || x == State::Running as u8 => {
                    return false; // already runnable
                }
                _ => return false, // Waking, Dying, etc.
            }
        }
    }

    // -----------------------------------------------------------------------
    // Owned-ops registry (Foundation #2: cancellable waits)
    // -----------------------------------------------------------------------

    /// Register an in-flight async operation before parking.
    ///
    /// Called by the submitting thread BEFORE it parks.  This is thread
    /// context, so heap allocation (Arc construction) is allowed.
    ///
    /// Returns `Err(op)` if the registry is full (`OWNED_OPS_CAP` reached).
    /// On overflow the caller falls back to the pre-Foundation-#2 behaviour
    /// (no cancel hookup for that op).
    #[allow(dead_code)] // used by AHCI cancel wiring (Phase 3b, Session B)
    pub fn owned_ops_push(&self, op: ArcCancellableOp) -> Result<(), ArcCancellableOp> {
        self.owned_ops.lock().push(op)
    }

    /// Deregister a completed op by pointer identity.
    ///
    /// Called by the driver or the submitter after wake-after-completion.
    /// O(N) scan with N ≤ `OWNED_OPS_CAP` (≤ 32); cheap.
    #[allow(dead_code)] // used by AHCI cancel wiring (Phase 3b, Session B)
    pub fn owned_ops_remove(&self, op_ptr: *const ()) {
        let mut guard = self.owned_ops.lock();
        if let Some(pos) = guard
            .iter()
            .position(|arc| Arc::as_ptr(arc) as *const () == op_ptr)
        {
            guard.swap_remove(pos);
        }
    }

    /// Called from `Thread::free` (reaper kthread context).
    ///
    /// Drains the registry into a local buffer, releases the lock, then
    /// calls `cancel()` on each entry.  Draining first avoids holding the
    /// `IrqSpinlock` across arbitrary `cancel()` implementations that may
    /// themselves acquire driver locks.
    pub fn owned_ops_cancel_all(&self) {
        // Drain under lock — heapless::Vec::take gives us the contents.
        let ops: heapless::Vec<ArcCancellableOp, OWNED_OPS_CAP> = {
            let mut guard = self.owned_ops.lock();
            core::mem::take(&mut *guard)
        };
        // Cancel outside the lock.
        for op in ops {
            op.cancel();
        }
    }
}

pub struct ThreadRegistry {
    pub(super) map: PreemptRwLock<BTreeMap<ThreadId, Arc<Thread>>>,
    infos: PreemptRwLock<BTreeMap<ThreadId, Arc<IrqSpinlock<UserThreadInfo>>>>,
}

impl ThreadRegistry {
    pub const fn new() -> Self {
        Self {
            map: PreemptRwLock::new(BTreeMap::new()),
            infos: PreemptRwLock::new(BTreeMap::new()),
        }
    }

    pub fn insert(&self, t: Arc<Thread>) {
        without_interrupts(|| {
            self.map.write().insert(t.id, t);
        })
    }

    pub fn remove(&self, tid: ThreadId) -> Option<Arc<Thread>> {
        without_interrupts(|| self.map.write().remove(&tid))
    }

    pub fn remove_info(&self, tid: ThreadId) -> Option<Arc<IrqSpinlock<UserThreadInfo>>> {
        without_interrupts(|| self.infos.write().remove(&tid))
    }

    pub fn insert_info(&self, tid: ThreadId, t: Arc<IrqSpinlock<UserThreadInfo>>) {
        without_interrupts(|| {
            self.infos.write().insert(tid, t);
        })
    }

    pub fn get(&self, tid: ThreadId) -> Option<Arc<Thread>> {
        without_interrupts(|| self.map.read().get(&tid).cloned())
    }

    pub fn get_info(&self, tid: ThreadId) -> Option<Arc<IrqSpinlock<UserThreadInfo>>> {
        without_interrupts(|| self.infos.read().get(&tid).cloned())
    }

    pub fn list(&self) -> Vec<Arc<Thread>> {
        without_interrupts(|| self.map.read().values().cloned().collect())
    }
}

// single global instance
pub(super) static THREADS: ThreadRegistry = ThreadRegistry::new();

/// An exited thread's status, plus the thread entitled to collect it.
#[derive(Debug, Clone, Copy)]
struct ExitRecord {
    code: i32,
    /// Whoever created the thread. When that thread dies, nothing can ever
    /// `waitpid` this record, so it is dropped instead of kept forever.
    parent: u64,
}

pub struct ThreadExitRegistry {
    map: PreemptRwLock<BTreeMap<ThreadId, ExitRecord>>,
    /// Threads waiting for another thread to exit: child_tid -> Weak<Thread>.
    waiters: PreemptRwLock<BTreeMap<ThreadId, Weak<Thread>>>,
}

impl ThreadExitRegistry {
    pub const fn new() -> Self {
        Self {
            map: PreemptRwLock::new(BTreeMap::new()),
            waiters: PreemptRwLock::new(BTreeMap::new()),
        }
    }

    pub fn insert(&self, tid: ThreadId, code: i32, parent: u64) {
        without_interrupts(|| {
            self.map.write().insert(tid, ExitRecord { code, parent });
        })
    }

    pub fn take(&self, tid: ThreadId) -> Option<i32> {
        without_interrupts(|| self.map.write().remove(&tid).map(|r| r.code))
    }

    /// Number of statuses waiting to be collected. Exposed through procfs so an
    /// unbounded one is visible rather than inferred.
    pub fn pending(&self) -> usize {
        without_interrupts(|| self.map.read().len())
    }

    /// Drop every record whose collector was `parent`.
    ///
    /// Called when `parent` exits: `waitpid` names a specific thread, so a
    /// status whose only interested party is gone can never be consumed.
    fn discard_children_of(&self, parent: u64) -> usize {
        without_interrupts(|| {
            let mut map = self.map.write();
            let doomed: alloc::vec::Vec<ThreadId> = map
                .iter()
                .filter(|(_, r)| r.parent == parent)
                .map(|(&tid, _)| tid)
                .collect();
            for tid in &doomed {
                map.remove(tid);
            }
            doomed.len()
        })
    }

    /// Check if an exit code exists without consuming it.
    pub fn has_exited(&self, tid: ThreadId) -> bool {
        without_interrupts(|| self.map.read().contains_key(&tid))
    }

    /// Register that `waiter` wants to be woken when `target` exits.
    pub fn register_waiter(&self, target: ThreadId, waiter: Weak<Thread>) {
        without_interrupts(|| {
            self.waiters.write().insert(target, waiter);
        })
    }

    /// Remove a waiter registration.
    pub fn unregister_waiter(&self, target: ThreadId) {
        without_interrupts(|| {
            self.waiters.write().remove(&target);
        })
    }

    /// Take and wake the waiter for a given target thread, if any.
    ///
    /// Called from the reaper kthread (thread context, not IRQ), so we use
    /// `wake_thread` (which has a self-skip check). The waiter Weak
    /// is valid as long as THREADS holds the canonical strong ref.
    pub fn wake_waiter(&self, target: ThreadId) {
        let waiter = without_interrupts(|| self.waiters.write().remove(&target));
        if let Some(waiter_handle) = waiter {
            use crate::thread::scheduler::{WakePriority, sched};
            sched().wake_thread(&waiter_handle, WakePriority::Normal);
        }
    }
}

pub static EXITED_THREADS: ThreadExitRegistry = ThreadExitRegistry::new();

/// The process that adopts orphans, or 0 before init has registered.
static INIT_PID: AtomicU64 = AtomicU64::new(0);

/// Declare which process inherits orphaned children. The kernel spawns init and
/// then tells the registry, so reparenting has somewhere to point.
pub fn set_init_pid(pid: u64) {
    INIT_PID.store(pid, Ordering::Release);
}

pub fn init_pid() -> u64 {
    INIT_PID.load(Ordering::Acquire)
}

/// Record an exited thread's status.
///
/// Runs on the exit path, which may have interrupts disabled, so it must not
/// allocate and must not walk the thread registry: `parent` is read from the
/// dying thread by the caller, which already holds it. The orphan bookkeeping
/// that *does* need to allocate is [`adopt_orphans_of`], called from the reaper.
pub fn record_thread_exit(tid: ThreadId, code: i32, parent: u64) {
    EXITED_THREADS.insert(tid, code, parent);
    EXITED_THREADS.wake_waiter(tid);
}

/// Hand `tid`'s surviving children to init and drop the statuses of those that
/// already exited.
///
/// `waitpid` names a specific thread, so once a creator is gone nothing can
/// ever collect its children's statuses; keeping them is an unbounded leak.
/// Called from the reaper, which is an ordinary kthread context: this walks the
/// registry and allocates, neither of which the exit path itself may do.
pub fn adopt_orphans_of(tid: ThreadId) {
    let init = INIT_PID.load(Ordering::Acquire);
    for thread in THREADS.list() {
        if thread.parent.load(Ordering::Acquire) == tid.0 {
            thread.parent.store(init, Ordering::Release);
        }
    }

    let dropped = EXITED_THREADS.discard_children_of(tid.0);
    if dropped > 0 {
        crate::log_debug!(
            "reap: dropped {dropped} uncollectable status(es) of {}",
            tid.0
        );
    }
}

pub fn take_thread_exit_code(tid: ThreadId) -> Option<i32> {
    EXITED_THREADS.take(tid)
}

// simple wrapper
pub fn get_thread_by_id(tid: ThreadId) -> Option<Arc<Thread>> {
    without_interrupts(|| THREADS.get(tid))
}

/// Return a `Weak<Thread>` for `tid` without keeping a strong reference.
/// Takes the THREADS read lock briefly; `Arc::downgrade` is refcount-only.
pub fn get_thread_weak(tid: ThreadId) -> Option<Weak<Thread>> {
    without_interrupts(|| THREADS.get(tid).map(|arc| Arc::downgrade(&arc)))
}

pub fn get_thread_info_by_id(tid: ThreadId) -> Option<Arc<IrqSpinlock<UserThreadInfo>>> {
    without_interrupts(|| THREADS.get_info(tid))
}

pub fn list_threads() -> Vec<Arc<Thread>> {
    without_interrupts(|| THREADS.list())
}

pub fn insert_thread(thread: Arc<Thread>) {
    THREADS.insert(thread);
}

pub fn insert_thread_info(tid: ThreadId, info: Arc<IrqSpinlock<UserThreadInfo>>) {
    THREADS.insert_info(tid, info);
}

pub fn allocate_thread_id() -> ThreadId {
    ThreadId(THREAD_ID_NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed))
}

/// Send a signal to every process in group `pgid`.
///
/// Returns whether the group had any members. This is what a terminal uses:
/// Ctrl+C is aimed at the foreground *job*, so every stage of a pipeline gets
/// it rather than only whichever process the shell happened to name.
pub fn signal_process_group(pgid: u64, signum: u32) -> bool {
    let members: Vec<ThreadId> = THREADS
        .list()
        .into_iter()
        .filter(|thread| thread.user.is_some() && thread.pgid() == pgid)
        .map(|thread| thread.id)
        .collect();

    for tid in &members {
        kill_process_with_signal(tid.0, signum);
    }
    !members.is_empty()
}

/// Place `pid` in process group `pgid`.
///
/// `pid` of 0 means the caller and `pgid` of 0 means "lead a new group", which
/// is how a shell puts the first process of a job into a group of its own.
pub fn set_process_group(pid: u64, pgid: u64, caller: u64) -> Result<(), Errno> {
    let pid = if pid == 0 { caller } else { pid };
    let Some(thread) = THREADS.get(ThreadId(pid)) else {
        return Err(Errno::ENOENT);
    };
    if thread.user.is_none() {
        return Err(Errno::EPERM);
    }
    let pgid = if pgid == 0 { pid } else { pgid };
    thread.pgid.store(pgid, Ordering::Release);
    Ok(())
}

/// The process group of `pid`, or of the caller when `pid` is 0.
pub fn process_group_of(pid: u64, caller: u64) -> Result<u64, Errno> {
    let pid = if pid == 0 { caller } else { pid };
    THREADS
        .get(ThreadId(pid))
        .map(|thread| thread.pgid())
        .ok_or(Errno::ENOENT)
}

/// Send a signal to a process by PID.
///
/// For signals whose default action is Terminate, also sets the `killed` flag
/// (for backward compatibility with PTY slave read checks) and wakes the thread.
pub fn kill_process_with_signal(pid: u64, signum: u32) -> bool {
    use crate::thread::scheduler::{WakePriority, sched};
    use crate::thread::signal;

    if let Some(thread) = THREADS.get(ThreadId(pid)) {
        let catchable = !signal::is_uncatchable(signum);

        if catchable && thread.signal.get_handler(signum) == signal::SIG_IGN {
            return true; // Signal was "sent" but ignored
        }

        // Set pending signal
        thread.signal.send(signum);

        // A blocked signal stays pending and acts on nothing until
        // `sigprocmask` unblocks it. SIGKILL and SIGSTOP cannot be blocked.
        if catchable && thread.signal.is_blocked(signum) {
            return true;
        }

        // A signal with a handler stays pending too: it is delivered on the
        // target's own stack at its next syscall return, not here. Applying
        // the default action as well would kill a process that asked to
        // handle the signal instead.
        if catchable && thread.signal.has_user_handler(signum) {
            sched().wake_thread(&Arc::downgrade(&thread), WakePriority::Normal);
            return true;
        }

        apply_default_action(&thread, signum);

        // Wake the thread so it can observe the signal. A stop takes effect at
        // the next boundary, so the target has to be running to reach it; a
        // continue has to undo the park the stop put it in. Both need a wake.
        sched().wake_thread(&Arc::downgrade(&thread), WakePriority::Normal);
        true
    } else {
        false
    }
}

/// Apply a signal's default action to `thread`, short of actually suspending
/// or terminating it — both of those happen on the target's own stack, at a
/// boundary where it holds nothing.
fn apply_default_action(thread: &Thread, signum: u32) {
    use crate::thread::signal;

    match signal::default_action(signum) {
        signal::DefaultAction::Terminate => {
            thread.killed.store(true, Ordering::Release);
            thread
                .exit_code
                .store(128 + signum as i32, Ordering::Release);
        }
        signal::DefaultAction::Stop => {
            thread.stop_requested.store(true, Ordering::Release);
        }
        signal::DefaultAction::Continue => {
            // Clearing the request is the whole of the resume: the parked
            // thread's park condition reads it, so the wake that follows sends
            // it back to userspace.
            thread.stop_requested.store(false, Ordering::Release);
            // The target clears this too, on its way out of the park, but not
            // until it next runs. A shell that resumes a job and immediately
            // waits on it would otherwise be told it is still stopped and put
            // it straight back in the job list.
            thread.stopped.store(false, Ordering::Release);
        }
        signal::DefaultAction::Ignore => {}
    }
}

/// Act on the pending signals a thread's mask no longer blocks.
///
/// Called after `sigprocmask` widens the deliverable set, on the thread's own
/// behalf. A signal whose disposition is SIG_IGN is discarded; one whose
/// default action is Terminate marks the thread killed, and it dies at the
/// syscall return boundary in `exit_if_killed`.
///
/// The action itself goes through `apply_default_action`, the same function the
/// send path uses: two copies of the Continue arm drifted apart once, leaving a
/// thread resumed this way still reporting Stopped to `ps` and to an untraced
/// `waitpid`.
pub fn deliver_unblocked_signals(thread: &Thread) {
    use crate::thread::signal;

    let deliverable = thread.signal.take_deliverable();
    for signum in 1..32u32 {
        if deliverable & (1 << signum) == 0 || thread.signal.get_handler(signum) == signal::SIG_IGN
        {
            continue;
        }
        // A handled signal is put back: it belongs to the handler path, which
        // runs on the thread's own stack at its next syscall return.
        if thread.signal.has_user_handler(signum) {
            thread.signal.send(signum);
            continue;
        }
        apply_default_action(thread, signum);
    }
}
