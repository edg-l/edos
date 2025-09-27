use core::{
    ops::Deref,
    sync::atomic::{AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering},
};

use alloc::{collections::btree_map::BTreeMap, string::String, sync::Arc};
use spin::{Mutex, RwLock};
use x86_64::{
    VirtAddr, instructions::interrupts::without_interrupts, registers::control::Cr3,
    structures::paging::OffsetPageTable,
};

use crate::{
    boot::boot_info,
    drivers::fpu::FpuState,
    fs::path::Path,
    loader::{ElfLoadError, load_elf},
    memory::mapper::{MemoryManager, active_level_4_table, get_level_4_table},
    println,
    syscalls::Errno,
    thread::{
        UserThread, UserThreadInfo,
        context::CpuContext,
        fd::FileDescriptorTable,
        paging::allocate_process_pml4,
        runqueue::{DEFAULT_PRIORITY, PRIORITY_LEVELS},
        scheduler::switch_to_kernel_page,
        setup_user_stack,
        util::{kthread_stack_alloc, kthread_stack_free, thread_stack_alloc, thread_stack_free},
    },
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

    pub exit_code: AtomicI32,

    pub user: Option<Arc<RwLock<UserThread>>>,

    // Context and kernel stack pointer:
    pub ctx: Mutex<CpuContext>, // only scheduler touches ctx under lock
    pub kstack_top: u64,
}

// For now kernel threads and user share id
static THREAD_ID_NEXT_ID: AtomicU64 = AtomicU64::new(1);

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

    pub fn set_affinity_mask(&self, mask: u32) {
        self.cpu_affinity.store(mask, Ordering::Release);
        self.mark_need_resched();
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

        let load_info = load_elf(elf_data, &mut process_memory_manager)?;

        println!("loaded elf, back to kernel page");

        // Back to kernel page
        unsafe { Cr3::write(kernel_pml4.0, kernel_pml4.1) };

        println!(
            "Creating CpuContext with entry_point: {:p}, stack_top: {:p}",
            load_info.entry_point.as_u64() as *const u8,
            user_stack_pointer as *const u8
        );

        let mut context =
            CpuContext::new_user_thread(load_info.entry_point.as_u64(), user_stack_pointer);
        context.rdi = argc as u64;
        context.rsi = argv_ptr;
        context.rdx = 0;

        let id = ThreadId(THREAD_ID_NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed));

        let name = Arc::new(name);

        let mm = Arc::new(Mutex::new(process_memory_manager));

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
            cpu: AtomicU32::new(0),
            exit_code: AtomicI32::new(0),
            user: Some(Arc::new(RwLock::new(UserThread {
                pid: id.0,
                initial_stack_top: stack_top,
                cr3: (page, kernel_pml4.1),
                memory_manager: mm.clone(),
                memory_regions: load_info.memory_regions,
                heap_break: load_info.heap_break,
                fpu_init: false,
                fpu: FpuState::default(),
            }))),
        });

        THREADS.insert(thread.clone());
        THREADS.insert_info(
            id,
            Arc::new(Mutex::new(UserThreadInfo {
                pid: id.0,
                errno: Errno::Clear,
                fd_table: FileDescriptorTable::new(),
                memory_mappings: BTreeMap::new(),
                next_mmap_addr: VirtAddr::new(load_info.heap_break),
                memory_manager: mm,
                cwd,
                user_id: user,
                group_id: group,
            })),
        );

        Ok(thread)
    }

    pub fn free(&self) {
        kthread_stack_free(self.kstack_top);

        if let Some(user) = &self.user {
            let user = user.write();
            let info = THREADS.get_info(self.id);
            // Unmap all memory mappings
            let mut memory_manager = user.memory_manager.lock();
            if let Some(info) = info {
                for (&addr, mapping) in &info.lock().memory_mappings {
                    let _ = memory_manager.unmap_memory(addr, mapping.size);
                }
            }

            for region in &user.memory_regions {
                let _ = memory_manager.unmap_memory(region.start, region.size);
            }

            thread_stack_free(&mut memory_manager, user.initial_stack_top);

            // clean up all page tables in the lower half of the address space
            memory_manager.clean_lower_half();
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
    infos: RwLock<BTreeMap<ThreadId, Arc<Mutex<UserThreadInfo>>>>,
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

    pub fn remove(&self, tid: ThreadId) {
        without_interrupts(|| {
            self.map.write().remove(&tid);
        })
    }

    pub fn insert_info(&self, tid: ThreadId, t: Arc<Mutex<UserThreadInfo>>) {
        without_interrupts(|| {
            self.infos.write().insert(tid, t);
        })
    }

    pub fn get(&self, tid: ThreadId) -> Option<Arc<Thread>> {
        without_interrupts(|| self.map.read().get(&tid).cloned())
    }

    pub fn get_info(&self, tid: ThreadId) -> Option<Arc<Mutex<UserThreadInfo>>> {
        without_interrupts(|| self.infos.read().get(&tid).cloned())
    }
}

// single global instance
pub(super) static THREADS: ThreadRegistry = ThreadRegistry::new();

// simple wrapper
pub fn get_thread_by_id(tid: ThreadId) -> Option<Arc<Thread>> {
    THREADS.get(tid)
}

pub fn get_thread_info_by_id(tid: ThreadId) -> Option<Arc<Mutex<UserThreadInfo>>> {
    THREADS.get_info(tid)
}
