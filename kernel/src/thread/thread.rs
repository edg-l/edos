use core::{
    ops::Deref,
    sync::atomic::{AtomicI32, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use alloc::{collections::btree_map::BTreeMap, string::String, sync::Arc, vec::Vec};
use spin::{Mutex, RwLock};
use x86_64::{
    VirtAddr,
    instructions::interrupts::without_interrupts,
    registers::control::Cr3,
    structures::paging::{OffsetPageTable, PageTableFlags},
};

use crate::{
    boot::boot_info,
    drivers::{fpu::FpuState, hpet::driver::get_hpet_timer},
    fs::path::Path,
    loader::{ElfLoadError, TlsTemplate, load_elf},
    memory::{
        USER_STACK_SIZE, USER_STACK_TOP,
        frame_allocator::frame_allocator,
        mapper::{MemoryManager, active_level_4_table, get_level_4_table},
        shared::SharedMemory,
    },
    println,
    syscalls::Errno,
    thread::{
        MappingType, MemoryRegion, MemoryRegionType, UserThread, UserThreadInfo, UserThreadTls,
        context::CpuContext,
        fd::FileDescriptorTable,
        irqlock::IrqSpinlock,
        mutex::BlockingMutex,
        paging::allocate_process_pml4,
        runqueue::{DEFAULT_PRIORITY, PRIORITY_LEVELS},
        scheduler::switch_to_kernel_page,
        setup_user_stack,
        util::{kthread_stack_alloc, kthread_stack_free, thread_stack_alloc, thread_stack_free},
    },
    timer::Instant,
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

impl From<u8> for State {
    fn from(v: u8) -> Self {
        match v {
            1 => State::Running,
            2 => State::Sleeping,
            3 => State::Parked,
            4 => State::Waking,
            5 => State::Dying,
            _ => State::Ready,
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

    pub user: Option<Arc<RwLock<UserThread>>>,

    // Context and kernel stack pointer:
    pub ctx: Mutex<CpuContext>, // only scheduler touches ctx under lock
    pub kstack_top: u64,
}

// For now kernel threads and user share id
static THREAD_ID_NEXT_ID: AtomicU64 = AtomicU64::new(1);

const TLS_REGION_STRIDE: u64 = 0x20000; // 128 KiB per-thread slot
const TLS_GUARD_GAP: u64 = 0x20000; // Keep a gap below the user stack
const TLS_TCB_MIN_SIZE: u64 = 64;
const PAGE_SIZE: u64 = 4096;

pub(crate) struct TlsAllocation {
    pub runtime: UserThreadTls,
    pub region: MemoryRegion,
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

    unsafe {
        core::ptr::write_bytes(mapping_base.as_u64() as *mut u8, 0, map_size as usize);

        if !template.init_data.is_empty() {
            core::ptr::copy_nonoverlapping(
                template.init_data.as_ptr(),
                tls_data_base.as_u64() as *mut u8,
                template.init_data.len(),
            );
        }

        let tcb_ptr = tcb_base.as_u64() as *mut u64;
        tcb_ptr.write(tcb_base.as_u64());
    }

    let runtime = UserThreadTls {
        template: Arc::clone(template),
        data_base: tls_data_base,
        data_size,
        tcb_base,
        tcb_size: tls_region_top.as_u64() - tcb_base.as_u64(),
        mapping_base,
        mapping_size: map_size,
    };

    let region = MemoryRegion {
        start: mapping_base,
        size: map_size,
        flags,
        region_type: MemoryRegionType::Tls,
    };

    Ok(TlsAllocation {
        runtime,
        region,
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

        let name = Arc::new(name);

        let thread = Arc::new(Self {
            id,
            kstack_top: initial_kstack_top,
            ctx: Mutex::new(context),
            state: AtomicU8::new(State::Ready as u8),
            name: Arc::new(name.as_ref().clone().unwrap_or(String::new())),
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
        });

        THREADS.insert(thread.clone());

        thread
    }

    /// Must provide entry point and cr3 page table.
    ///
    /// Note: This function switches to kernel page, should be called without interrupts
    pub fn new_user(
        elf_data: &[u8],
        name: Option<String>,
        argv: &[&[u8]],
        user: u32,
        group: u32,
        cwd: Path,
    ) -> Result<Arc<Self>, ElfLoadError> {
        switch_to_kernel_page();
        // allocate kernel stack before creating page
        let kernel_stack_top = kthread_stack_alloc();

        // Create user page.
        let kernel_pml4 = boot_info().cr3;
        let physical_memory_offset = boot_info().physical_memory_offset;
        let kernel_table = unsafe { get_level_4_table(kernel_pml4) };
        let page = unsafe { allocate_process_pml4(kernel_table) };

        // Use process page to set mappings
        unsafe { Cr3::write(page, kernel_pml4.1) };
        println!("Switched to new process page");

        let page_table = unsafe { active_level_4_table(physical_memory_offset) };
        let table = unsafe { OffsetPageTable::new(page_table, physical_memory_offset) };

        let mut process_memory_manager = MemoryManager::new(table);

        // call align
        let stack_top = thread_stack_alloc(&mut process_memory_manager);

        let (user_stack_pointer, argv_ptr, argc) =
            setup_user_stack(stack_top, argv).map_err(|_| ElfLoadError::MappingFailed)?;

        let mut load_info = load_elf(elf_data, &mut process_memory_manager)?;

        let id = ThreadId(THREAD_ID_NEXT_ID.fetch_add(1, Ordering::Relaxed));

        let mut tls_runtime: Option<UserThreadTls> = None;
        let mut tls_region: Option<MemoryRegion> = None;
        let mut tls_fs_base = 0u64;

        if let Some(template) = load_info.tls_template.take() {
            let template = Arc::new(template);
            let allocation = allocate_tls_region(&template, id, &mut process_memory_manager)?;
            tls_fs_base = allocation.fs_base;
            tls_region = Some(allocation.region);
            tls_runtime = Some(allocation.runtime);
        }

        let entry_point = load_info.entry_point;
        let heap_break = load_info.heap_break;
        let mut owned_regions = Vec::new();
        if let Some(region) = tls_region {
            owned_regions.push(region);
        }
        let process_regions = Arc::new(load_info.memory_regions);

        println!("loaded elf, back to kernel page");

        // Back to kernel page
        unsafe { Cr3::write(kernel_pml4.0, kernel_pml4.1) };

        println!(
            "Creating CpuContext with entry_point: {:p}, stack_top: {:p}",
            entry_point.as_u64() as *const u8,
            user_stack_pointer as *const u8
        );

        let mut context = CpuContext::new_user_thread(entry_point.as_u64(), user_stack_pointer);
        context.rdi = argc as u64;
        context.rsi = argv_ptr;
        context.rdx = 0;

        let name = Arc::new(name);

        let mm = Arc::new(Mutex::new(process_memory_manager));

        let address_space_refs = Arc::new(AtomicUsize::new(1));
        let process_stack_top = Arc::new(AtomicU64::new(stack_top));

        let user_state = Arc::new(RwLock::new(UserThread {
            pid: id.0,
            cr3: (page, kernel_pml4.1),
            memory_manager: mm.clone(),
            memory_regions: Arc::clone(&process_regions),
            owned_regions,
            tls: tls_runtime,
            fpu_init: false,
            fpu: FpuState::default(),
            heap_break,
            address_space_refs,
            process_stack_top,
        }));

        let thread = Arc::new(Thread {
            id,
            kstack_top: kernel_stack_top,
            ctx: Mutex::new(context),
            state: AtomicU8::new(State::Ready as u8),
            name: Arc::new(name.as_ref().clone().unwrap_or(String::new())),
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
            user: Some(user_state),
        });

        THREADS.insert(thread.clone());
        THREADS.insert_info(
            id,
            Arc::new(IrqSpinlock::new(UserThreadInfo {
                pid: id.0,
                errno: Errno::Clear,
                fd_table: Arc::new(BlockingMutex::new(FileDescriptorTable::new())),
                memory_mappings: Arc::new(BlockingMutex::new(BTreeMap::new())),
                next_mmap_addr: Arc::new(AtomicU64::new(heap_break)),
                memory_manager: mm,
                cwd: Arc::new(BlockingMutex::new(cwd)),
                user_id: user,
                group_id: group,
            })),
        );

        Ok(thread)
    }

    pub fn free(&self) {
        kthread_stack_free(self.kstack_top);

        let Some(user_lock) = &self.user else {
            return;
        };

        let user = user_lock.write();
        let remaining = user.address_space_refs.fetch_sub(1, Ordering::AcqRel);
        let is_last_thread = remaining == 1;

        let mut memory_manager = user.memory_manager.lock();

        for region in &user.owned_regions {
            let _ = memory_manager.unmap_memory(region.start, region.size);
        }

        if is_last_thread {
            if let Some(info) = THREADS.get_info(self.id) {
                let mappings = info.lock().memory_mappings.lock().clone();
                for (addr, mapping) in mappings {
                    match mapping.mapping_type {
                        MappingType::Anonymous => {
                            // Anonymous mappings: unmap and deallocate frames
                            let _ = memory_manager.unmap_memory(addr, mapping.size);
                        }
                        MappingType::Shared(shm_id) => {
                            // Shared memory: unmap pages but DON'T deallocate frames
                            // The frames belong to the SharedMemory object
                            use x86_64::structures::paging::{Mapper, Page, Size4KiB};
                            let page_count = (mapping.size + 0xFFF) / 4096;
                            for i in 0..page_count {
                                let virt_addr = VirtAddr::new(addr.as_u64() + i * 4096);
                                let page: Page<Size4KiB> = Page::containing_address(virt_addr);
                                if let Ok((_, flush)) = memory_manager.mapper.unmap(page) {
                                    flush.flush();
                                }
                            }
                            // Decrement the shared memory ref count
                            if let Some(shm) = SharedMemory::get(shm_id) {
                                shm.dec_ref();
                            }
                        }
                    }
                }
            }

            for region in user.memory_regions.iter() {
                let _ = memory_manager.unmap_memory(region.start, region.size);
            }

            let stack_top = user.process_stack_top.load(Ordering::Acquire);
            thread_stack_free(&mut memory_manager, stack_top);

            memory_manager.clean_lower_half();

            switch_to_kernel_page();

            let pml4 = user.cr3.0;
            unsafe { frame_allocator().deallocate_frame(pml4) };
        }
    }

    #[inline]
    pub fn cas_state(&self, from: State, to: State) -> bool {
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
                        return true;
                    } else {
                        continue; // retry if raced
                    }
                }
                x if x == State::Ready as u8 || x == State::Running as u8 => {
                    return false; // already runnable
                }
                _ => return false, // Zombie etc.
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
}

impl ThreadExitRegistry {
    pub const fn new() -> Self {
        Self {
            map: RwLock::new(BTreeMap::new()),
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
}

pub(super) static EXITED_THREADS: ThreadExitRegistry = ThreadExitRegistry::new();

pub fn record_thread_exit(tid: ThreadId, code: i32) {
    EXITED_THREADS.insert(tid, code);
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
