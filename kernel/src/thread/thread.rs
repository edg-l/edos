use core::{
    cell::UnsafeCell,
    ops::Deref,
    sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use alloc::{collections::btree_map::BTreeMap, string::String, sync::Arc, vec::Vec};
use spin::{Mutex, RwLock};
use x86_64::{
    VirtAddr,
    instructions::interrupts::without_interrupts,
    registers::control::Cr3,
    structures::paging::{OffsetPageTable, PageTableFlags},
};

use intrusive_list::Link;

use crate::{
    boot::boot_info,
    drivers::{fpu::FpuState, hpet::driver::get_hpet_timer},
    fs::{self, path::Path},
    loader::{ElfLoadError, TlsTemplate, load_elf},
    memory::{
        USER_STACK_SIZE, USER_STACK_TOP,
        frame_allocator::frame_allocator,
        mapper::{MemoryManager, get_level_4_table},
        shared::SharedMemory,
        vma::{Vma, VmaBacking, VmaFlags, VmaProt, VmaSet},
    },
    println,
    syscalls::Errno,
    thread::{
        UserThread, UserThreadInfo, UserThreadTls,
        context::CpuContext,
        fd::FileDescriptorTable,
        irqlock::IrqSpinlock,
        mutex::BlockingMutex,
        paging::allocate_process_pml4,
        runqueue::{DEFAULT_PRIORITY, PRIORITY_LEVELS},
        scheduler::switch_to_kernel_page,
        setup_user_stack,
        signal::SignalState,
        util::{kthread_stack_alloc, kthread_stack_free, thread_stack_alloc, thread_stack_free},
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
///   Ready   -> Running  (scheduler picks thread)
///   Running -> Ready    (preempted, re-enqueued)
///   Running -> Parked   (thread_park / thread_park_while)
///   Running -> Sleeping (thread_sleep)
///   Running -> Dying    (thread_exit)
///   Parked  -> Waking   (try_wake)
///   Parked  -> Running  (thread_park_while abort)
///   Sleeping-> Waking   (try_wake)
///   Waking  -> Ready    (complete_wake)
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

    // deadline as Instant counter value
    pub slice_deadline: AtomicU64,

    // Sleep data split to avoid locking:
    pub sleep_deadline: AtomicU64, // instant counter value

    // CPU time accounting (nanoseconds + current run start tick)
    pub cpu_time_ns: AtomicU64,
    pub run_start_tick: AtomicU64,

    pub tls_base: AtomicU64,

    pub exit_code: AtomicI32,
    /// Set when the process has been killed (e.g. by Ctrl+C). Sleeping syscalls
    /// check this flag on wakeup and return EINTR to unblock the process.
    pub killed: AtomicBool,
    /// Signal state: pending bitmask and per-signal disposition.
    pub signal: SignalState,

    pub user: Option<Arc<RwLock<UserThread>>>,

    // Context and kernel stack pointer:
    pub ctx: Mutex<CpuContext>, // only scheduler touches ctx under lock
    pub kstack_top: u64,

    // Intrusive runqueue link — only touched while the runqueue lock is held.
    pub rq_link: Link,
    pub rq_boosted: AtomicBool,

    // Set after save_current_thread writes valid ctx, cleared when the
    // thread starts running via context_switch_to. Work-stealing skips
    // threads where this is false to avoid loading stale register state.
    pub context_saved: AtomicBool,

    // FPU state — only the running CPU touches this during context switch.
    // UnsafeCell because it's accessed without a lock (only current CPU writes).
    pub fpu: UnsafeCell<FpuState>,
    pub fpu_init: AtomicBool,
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
    thread_id: ThreadId,
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
    let slot_offset = thread_id
        .0
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
    pub fn switch_to_page(&self) {
        if let Some(user) = &self.user {
            let user = user.read();
            if Cr3::read().0.start_address() != user.cr3.0.start_address() {
                unsafe { Cr3::write(user.cr3.0, user.cr3.1) };
            }
            //tlb_flush_all_including_global();
        } else {
            switch_to_kernel_page();
        }
    }

    pub fn state(&self) -> State {
        State::from(self.state.load(Ordering::Acquire))
    }

    pub fn mark_need_resched(&self) {
        self.flags
            .fetch_or(Flags::NEED_RESCHED.bits(), Ordering::AcqRel);
    }

    pub fn set_priority(&self, prio: u8) {
        self.priority
            .store(prio.min((PRIORITY_LEVELS - 1) as u8), Ordering::Release);
        self.mark_need_resched();
    }

    pub fn priority(&self) -> u8 {
        self.priority.load(Ordering::Acquire)
    }

    #[expect(unused)]
    pub fn set_affinity_mask(&self, mask: u32) {
        self.cpu_affinity.store(mask, Ordering::Release);
        self.mark_need_resched();
    }

    pub fn begin_run(&self, start_tick: u64) {
        self.run_start_tick.store(start_tick, Ordering::Release);
    }

    pub fn end_run(&self, end_tick: u64) {
        let start_tick = self.run_start_tick.swap(0, Ordering::AcqRel);
        if start_tick == 0 {
            return;
        }

        let elapsed_ticks = end_tick.saturating_sub(start_tick);
        if elapsed_ticks == 0 {
            return;
        }

        if let Some(timer) = get_hpet_timer() {
            let elapsed_ns = timer.ticks_to_nanos(elapsed_ticks);
            if elapsed_ns != 0 {
                self.cpu_time_ns.fetch_add(elapsed_ns, Ordering::AcqRel);
            }
        }
    }

    pub fn cpu_time_ns(&self) -> u64 {
        let accumulated = self.cpu_time_ns.load(Ordering::Acquire);
        let start_tick = self.run_start_tick.load(Ordering::Acquire);

        if start_tick == 0 {
            return accumulated;
        }

        // Only report in-flight runtime while the scheduler still considers us running.
        if !matches!(self.state(), State::Running) {
            return accumulated;
        }

        if let Some(timer) = get_hpet_timer() {
            let now_tick = Instant::now().tick();
            let extra_ticks = now_tick.saturating_sub(start_tick);
            let extra_ns = timer.ticks_to_nanos(extra_ticks);
            return accumulated.saturating_add(extra_ns);
        }

        accumulated
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
            cpu_affinity: AtomicU32::new(0),
            flags: AtomicU32::new(0),
            slice_deadline: AtomicU64::new(0),
            priority: AtomicU8::new(DEFAULT_PRIORITY),
            sleep_deadline: AtomicU64::new(0),
            cpu_time_ns: AtomicU64::new(0),
            run_start_tick: AtomicU64::new(0),
            tls_base: AtomicU64::new(0),
            cpu: AtomicU32::new(0),
            exit_code: AtomicI32::new(0),
            killed: AtomicBool::new(false),
            signal: SignalState::new(),
            rq_link: Link::new(),
            rq_boosted: AtomicBool::new(false),
            context_saved: AtomicBool::new(true),
            fpu: UnsafeCell::new(FpuState::default()),
            fpu_init: AtomicBool::new(false),
        });

        THREADS.insert(thread.clone());

        thread
    }

    /// Must provide entry point and cr3 page table.
    pub fn new_user(
        elf_data: Arc<Vec<u8>>,
        name: Option<String>,
        argv: &[&[u8]],
        envp: &[&[u8]],
        user: u32,
        group: u32,
        cwd: Path,
    ) -> Result<Arc<Self>, ElfLoadError> {
        let kernel_stack_top = kthread_stack_alloc();

        let kernel_pml4 = boot_info().cr3;
        let physical_memory_offset = boot_info().physical_memory_offset;
        let kernel_table = unsafe { get_level_4_table(kernel_pml4) };
        let page = unsafe { allocate_process_pml4(kernel_table) };

        // Build child's page table manager via HHDM (no CR3 switch needed).
        let child_page_table = unsafe { get_level_4_table((page, kernel_pml4.1)) };
        let table = unsafe { OffsetPageTable::new(child_page_table, physical_memory_offset) };

        let mut process_memory_manager = MemoryManager::new(table);

        let stack_top = thread_stack_alloc(&mut process_memory_manager);

        let (user_stack_pointer, argv_ptr, argc, envp_ptr) =
            setup_user_stack(stack_top, argv, envp, &process_memory_manager)
                .map_err(|_| ElfLoadError::MappingFailed)?;

        let mut load_info = load_elf(elf_data.clone(), &mut process_memory_manager)?;

        let id = ThreadId(THREAD_ID_NEXT_ID.fetch_add(1, Ordering::Relaxed));

        let mut tls_runtime: Option<UserThreadTls> = None;
        let mut tls_region: Option<Vma> = None;
        let mut tls_fs_base = 0u64;

        if let Some(template) = load_info.tls_template.take() {
            let template = Arc::new(template);
            let allocation = allocate_tls_region(&template, id, &mut process_memory_manager)?;
            tls_fs_base = allocation.fs_base;
            tls_region = Some(allocation.vma);
            tls_runtime = Some(allocation.runtime);
        }

        let entry_point = load_info.entry_point;
        let heap_break = load_info.heap_break;

        // Build initial VmaSet from ELF segments + TLS + stack
        let mut vma_set = VmaSet::new();
        for vma in load_info.memory_regions {
            vma_set.insert(vma);
        }
        if let Some(tls_vma) = tls_region {
            vma_set.insert(tls_vma);
        }
        let stack_bottom = VirtAddr::new(stack_top - USER_STACK_SIZE);
        vma_set.insert(Vma {
            start: stack_bottom,
            end: VirtAddr::new(stack_top),
            prot: VmaProt::READ | VmaProt::WRITE,
            flags: VmaFlags::PRIVATE | VmaFlags::GROWSDOWN,
            backing: VmaBacking::Stack,
        });

        let mut context = CpuContext::new_user_thread(entry_point.as_u64(), user_stack_pointer);
        context.rdi = argc as u64;
        context.rsi = argv_ptr;
        context.rdx = envp_ptr;

        let name = Arc::new(name.unwrap_or_default());

        let mm = Arc::new(Mutex::new(process_memory_manager));

        let address_space_refs = Arc::new(AtomicUsize::new(1));
        let process_stack_top = Arc::new(AtomicU64::new(stack_top));

        let user_state = Arc::new(RwLock::new(UserThread {
            pid: id.0,
            cr3: (page, kernel_pml4.1),
            memory_manager: mm.clone(),
            vmas: Arc::new(spin::Mutex::new(vma_set)),
            tls: tls_runtime,
            heap_break,
            address_space_refs,
            process_stack_top,
        }));

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
            run_start_tick: AtomicU64::new(0),
            tls_base: AtomicU64::new(tls_fs_base),
            cpu: AtomicU32::new(0),
            exit_code: AtomicI32::new(0),
            killed: AtomicBool::new(false),
            signal: SignalState::new(),
            user: Some(user_state),
            rq_link: Link::new(),
            rq_boosted: AtomicBool::new(false),
            context_saved: AtomicBool::new(true),
            fpu: UnsafeCell::new(FpuState::default()),
            fpu_init: AtomicBool::new(false),
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
        // Get file info to know the size
        let file_info = fs::api::file_info(path).map_err(|e| {
            println!("Failed to get file info for {:?}: {:?}", path, e);
            ElfLoadError::MappingFailed
        })?;

        let file_size = file_info.size as usize;
        if file_size == 0 {
            println!("File {:?} is empty", path);
            return Err(ElfLoadError::MissingSegments);
        }

        // Read the entire file, wrap in Arc for lazy ELF loading
        let elf_data = Arc::new(fs::api::read_bytes(path, 0, file_size).map_err(|e| {
            println!("Failed to read file {:?}: {:?}", path, e);
            ElfLoadError::MappingFailed
        })?);

        Self::new_user(elf_data, name, argv, envp, user, group, cwd)
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
        kthread_stack_free(self.kstack_top);

        let Some(user_lock) = &self.user else {
            return;
        };

        let user = user_lock.write();
        let remaining = user.address_space_refs.fetch_sub(1, Ordering::AcqRel);
        let is_last_thread = remaining == 1;

        let mut memory_manager = user.memory_manager.lock();

        if is_last_thread {
            // Close all file descriptors (pipes need proper shutdown for EOF)
            if let Some(info) = THREADS.get_info(self.id) {
                let fds: alloc::vec::Vec<(u64, super::pipe::FileDescriptor)> =
                    info.lock().fd_table.lock().drain_all();
                for (_fd_num, descriptor) in fds {
                    match descriptor {
                        super::pipe::FileDescriptor::PipeRead(pipe) => {
                            let notif = pipe.lock().close_reader();
                            notif.flush();
                        }
                        super::pipe::FileDescriptor::PipeWrite(pipe) => {
                            let notif = pipe.lock().close_writer();
                            notif.flush();
                        }
                        super::pipe::FileDescriptor::PtySlave(pty) => {
                            let mut guard = pty.lock();
                            if guard.foreground_pid == Some(self.id.0) {
                                guard.foreground_pid = None;
                            }
                            let notif = guard.close_slave();
                            drop(guard);
                            notif.flush();
                        }
                        super::pipe::FileDescriptor::PtyMaster(pty) => {
                            let notif = pty.lock().close_master();
                            notif.flush();
                        }
                        super::pipe::FileDescriptor::Socket(sock) => {
                            let mut s = sock.lock();
                            s.refcount = s.refcount.saturating_sub(1);
                            if s.refcount > 0 {
                                continue; // Other fds still reference this socket
                            }
                            s.closed = true;
                            s.rx_wq.wake_all();
                            if let Some(addr) = s.local_addr {
                                let proto = if s.sock_type == crate::net::socket::SOCK_DGRAM {
                                    17u8
                                } else {
                                    6u8
                                };
                                crate::net::socket::port_table()
                                    .lock()
                                    .remove(&(proto, addr.port));
                            }
                            let tcp_conn = s.tcp_conn.clone();
                            drop(s);
                            // For TCP sockets, send FIN to initiate graceful close
                            if let Some(conn) = tcp_conn {
                                let fin = conn.lock().build_fin();
                                if let Some(fin_seg) = fin {
                                    let remote_ip = conn.lock().remote_ip;
                                    if let Some(stack_mutex) = crate::net::stack::NET_STACK.get() {
                                        let mut stack = stack_mutex.lock();
                                        let _ = stack.send_ip(
                                            remote_ip,
                                            crate::net::ipv4::IpProtocol::Tcp,
                                            &fin_seg,
                                        );
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            // Clean up windows owned by this process
            window::cleanup_process_windows(user.pid);

            // Unmap all VMAs, skipping Stack (handled by thread_stack_free below)
            let vmas = user.vmas.lock().clone();
            for vma in vmas.iter() {
                match &vma.backing {
                    VmaBacking::Anonymous | VmaBacking::Tls => {
                        let _ = memory_manager.unmap_memory(vma.start, vma.size());
                    }
                    VmaBacking::ElfSegment { .. } => {
                        // Pages may be lazily faulted -- only unmap present ones
                        use x86_64::structures::paging::{Mapper, Page, Size4KiB};
                        let page_count = (vma.size() + 0xFFF) / 4096;
                        for i in 0..page_count {
                            let virt = VirtAddr::new(vma.start.as_u64() + i * 4096);
                            let page: Page<Size4KiB> = Page::containing_address(virt);
                            if let Ok(phys) = memory_manager.mapper.translate_page(page) {
                                if let Ok((_, flush)) = memory_manager.mapper.unmap(page) {
                                    flush.ignore();
                                }
                                unsafe { frame_allocator().deallocate_frame(phys) };
                            }
                        }
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
                }
            }

            let stack_top = user.process_stack_top.load(Ordering::Acquire);
            thread_stack_free(&mut memory_manager, stack_top);

            // No TLB shootdown needed: each process has its own address space
            // (address_space_refs starts at 1, COW fork creates a new PML4).
            // The dying thread is the only one with this CR3 loaded, so no other
            // CPU has user-space TLB entries for this address space.
            memory_manager.clean_lower_half();

            switch_to_kernel_page();

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
}

pub struct ThreadRegistry {
    pub(super) map: RwLock<BTreeMap<ThreadId, Arc<Thread>>>,
    infos: RwLock<BTreeMap<ThreadId, Arc<IrqSpinlock<UserThreadInfo>>>>,
}

impl ThreadRegistry {
    pub const fn new() -> Self {
        Self {
            map: RwLock::new(BTreeMap::new()),
            infos: RwLock::new(BTreeMap::new()),
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

pub struct ThreadExitRegistry {
    map: RwLock<BTreeMap<ThreadId, i32>>,
    /// Threads waiting for another thread to exit: child_tid -> waiter_tid.
    waiters: RwLock<BTreeMap<ThreadId, ThreadId>>,
}

impl ThreadExitRegistry {
    pub const fn new() -> Self {
        Self {
            map: RwLock::new(BTreeMap::new()),
            waiters: RwLock::new(BTreeMap::new()),
        }
    }

    pub fn insert(&self, tid: ThreadId, code: i32) {
        without_interrupts(|| {
            self.map.write().insert(tid, code);
        })
    }

    pub fn take(&self, tid: ThreadId) -> Option<i32> {
        without_interrupts(|| self.map.write().remove(&tid))
    }

    /// Check if an exit code exists without consuming it.
    pub fn has_exited(&self, tid: ThreadId) -> bool {
        without_interrupts(|| self.map.read().contains_key(&tid))
    }

    /// Register that `waiter` wants to be woken when `target` exits.
    pub fn register_waiter(&self, target: ThreadId, waiter: ThreadId) {
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
    pub fn wake_waiter(&self, target: ThreadId) {
        let waiter = without_interrupts(|| self.waiters.write().remove(&target));
        if let Some(waiter_tid) = waiter {
            use crate::thread::scheduler::{WakePriority, sched};
            sched().wake_thread_irq(waiter_tid, WakePriority::Normal);
        }
    }
}

pub static EXITED_THREADS: ThreadExitRegistry = ThreadExitRegistry::new();

pub fn record_thread_exit(tid: ThreadId, code: i32) {
    EXITED_THREADS.insert(tid, code);
    EXITED_THREADS.wake_waiter(tid);
}

pub fn take_thread_exit_code(tid: ThreadId) -> Option<i32> {
    EXITED_THREADS.take(tid)
}

// simple wrapper
pub fn get_thread_by_id(tid: ThreadId) -> Option<Arc<Thread>> {
    without_interrupts(|| THREADS.get(tid))
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

/// Mark a process as killed and wake it so it can exit.
///
/// Sends SIGINT to the target process. The target thread will observe
/// `killed == true` after waking from any blocking syscall (e.g. `sys_read`
/// on a PTY slave) and return an error, causing the process to exit.
pub fn kill_process(pid: u64) -> bool {
    kill_process_with_signal(pid, crate::thread::signal::SIGINT)
}

/// Send a signal to a process by PID.
///
/// For signals whose default action is Terminate, also sets the `killed` flag
/// (for backward compatibility with PTY slave read checks) and wakes the thread.
pub fn kill_process_with_signal(pid: u64, signum: u32) -> bool {
    use crate::thread::scheduler::{WakePriority, sched};
    use crate::thread::signal;

    if let Some(thread) = THREADS.get(ThreadId(pid)) {
        // Check if signal is ignored (SIG_IGN)
        if signum != signal::SIGKILL && thread.signal.get_handler(signum) == signal::SIG_IGN {
            return true; // Signal was "sent" but ignored
        }

        // Set pending signal
        thread.signal.send(signum);

        // For default-terminate signals, set the killed flag and exit code
        match signal::default_action(signum) {
            signal::DefaultAction::Terminate => {
                // Also set the old killed flag for backward compatibility
                // (PTY slave read checks it)
                thread.killed.store(true, Ordering::Release);
                thread
                    .exit_code
                    .store(128 + signum as i32, Ordering::Release);
            }
            signal::DefaultAction::Ignore => {
                // SIG_DFL for this signal is ignore (e.g. SIGCHLD)
                return true;
            }
        }

        // Wake the thread so it can observe the signal
        sched().wake_thread(ThreadId(pid), WakePriority::Normal);
        true
    } else {
        false
    }
}
