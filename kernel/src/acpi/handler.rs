use core::{
    ptr::NonNull,
    sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};

use acpi::{Handle, Handler, PhysicalMapping};
use x86_64::{PhysAddr, VirtAddr, instructions::port::Port, structures::paging::PageTableFlags};

use crate::{
    drivers::pci::{
        config::{
            pci_read_u8, pci_read_u16, pci_read_u32, pci_write_u8, pci_write_u16, pci_write_u32,
        },
        structures::PciAddress,
    },
    memory::{
        mapper::memory_mapper,
        valloc::{vfree, vmalloc},
    },
    println,
    thread::scheduler::{current_thread_id, thread_sleep, thread_yield},
    timer::{Instant, uptime_nanos},
};

#[derive(Debug, Clone)]
pub struct AcpiHandler;

// ACPI handler manages its own "virtual address space", this is why using the physical address mapping isnt correct here, and we need to actually map
impl Handler for AcpiHandler {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> acpi::PhysicalMapping<Self, T> {
        let mut mapper = memory_mapper();
        let virt_start =
            vmalloc(size as u64).expect("vmalloc: no address space for an ACPI mapping");

        // Check if virtual address is canonical
        let addr = virt_start.as_u64();
        if (0x0000800000000000..0xFFFF800000000000).contains(&addr) {
            panic!("Non-canonical virtual address: {:#x}", addr);
        }

        // Try the mapping with more debugging
        match mapper.map_address_range(
            virt_start,
            PhysAddr::new(physical_address as u64),
            size,
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE,
        ) {
            Ok(_) => {}
            Err(e) => {
                println!("ACPI: Mapping failed: {:?}", e);
                panic!("ACPI mapping failed");
            }
        }

        PhysicalMapping {
            handler: self.clone(),
            mapped_length: size,
            physical_start: physical_address,
            region_length: size,
            virtual_start: NonNull::new(virt_start.as_mut_ptr()).unwrap(),
        }
    }

    fn unmap_physical_region<T>(region: &acpi::PhysicalMapping<Self, T>) {
        let mut mapper = memory_mapper();

        let virt_start = VirtAddr::new(region.virtual_start.as_ptr() as u64);
        // An ACPI table lives in firmware memory: unmap the range without
        // returning the frames to the allocator, which never owned them.
        match mapper.unmap_foreign_memory(virt_start, region.mapped_length as u64) {
            Ok(_) => {
                vfree(virt_start, region.mapped_length as u64);
            }
            Err(e) => {
                println!("ACPI: Unamp failed: {:?}", e);
            }
        }
    }

    fn read_u8(&self, address: usize) -> u8 {
        unsafe { *(address as *const u8) }
    }

    fn read_u16(&self, address: usize) -> u16 {
        unsafe { *(address as *const u16) }
    }

    fn read_u32(&self, address: usize) -> u32 {
        unsafe { *(address as *const u32) }
    }

    fn read_u64(&self, address: usize) -> u64 {
        unsafe { *(address as *const u64) }
    }

    fn write_u8(&self, address: usize, value: u8) {
        unsafe {
            *(address as *mut u8) = value;
        }
    }

    fn write_u16(&self, address: usize, value: u16) {
        unsafe {
            *(address as *mut u16) = value;
        }
    }

    fn write_u32(&self, address: usize, value: u32) {
        unsafe {
            *(address as *mut u32) = value;
        }
    }

    fn write_u64(&self, address: usize, value: u64) {
        unsafe {
            *(address as *mut u64) = value;
        }
    }

    fn read_io_u8(&self, port: u16) -> u8 {
        unsafe { Port::new(port).read() }
    }

    fn read_io_u16(&self, port: u16) -> u16 {
        unsafe { Port::new(port).read() }
    }

    fn read_io_u32(&self, port: u16) -> u32 {
        unsafe { Port::new(port).read() }
    }

    fn write_io_u8(&self, port: u16, value: u8) {
        unsafe { Port::new(port).write(value) }
    }

    fn write_io_u16(&self, port: u16, value: u16) {
        unsafe { Port::new(port).write(value) }
    }

    fn write_io_u32(&self, port: u16, value: u32) {
        unsafe { Port::new(port).write(value) }
    }

    fn read_pci_u8(&self, address: acpi::PciAddress, offset: u16) -> u8 {
        match legacy_config(address, offset) {
            Some((addr, offset)) => pci_read_u8(addr, offset),
            None => !0,
        }
    }

    fn read_pci_u16(&self, address: acpi::PciAddress, offset: u16) -> u16 {
        match legacy_config(address, offset) {
            Some((addr, offset)) => pci_read_u16(addr, offset),
            None => !0,
        }
    }

    fn read_pci_u32(&self, address: acpi::PciAddress, offset: u16) -> u32 {
        match legacy_config(address, offset) {
            Some((addr, offset)) => pci_read_u32(addr, offset),
            None => !0,
        }
    }

    fn write_pci_u8(&self, address: acpi::PciAddress, offset: u16, value: u8) {
        if let Some((addr, offset)) = legacy_config(address, offset) {
            pci_write_u8(addr, offset, value);
        }
    }

    fn write_pci_u16(&self, address: acpi::PciAddress, offset: u16, value: u16) {
        if let Some((addr, offset)) = legacy_config(address, offset) {
            pci_write_u16(addr, offset, value);
        }
    }

    fn write_pci_u32(&self, address: acpi::PciAddress, offset: u16, value: u32) {
        if let Some((addr, offset)) = legacy_config(address, offset) {
            pci_write_u32(addr, offset, value);
        }
    }

    fn nanos_since_boot(&self) -> u64 {
        uptime_nanos()
    }

    fn stall(&self, microseconds: u64) {
        // ACPI 6.5 §5.5.2.4.1: a stall must not give up the processor, which
        // is why this spins where `sleep` parks.
        let deadline = Instant::now() + Duration::from_micros(microseconds);
        while Instant::now() < deadline {
            core::hint::spin_loop();
        }
    }

    fn sleep(&self, milliseconds: u64) {
        let dt = Duration::from_millis(milliseconds);
        if current_thread_id().is_some() {
            thread_sleep(dt);
        } else {
            // Firmware can ask for a sleep before the scheduler exists, and
            // `thread_sleep` returns immediately when no thread is running.
            let deadline = Instant::now() + dt;
            while Instant::now() < deadline {
                core::hint::spin_loop();
            }
        }
    }

    fn create_mutex(&self) -> Handle {
        let index = AML_MUTEX_COUNT.fetch_add(1, Ordering::Relaxed);
        if index == AML_MUTEXES.len() {
            println!(
                "ACPI: AML declares more than {} mutexes; the rest cannot be acquired",
                AML_MUTEXES.len()
            );
        }
        // A handle past the table is handed back anyway: an unacquirable mutex
        // fails the AML method that wants it, where a panic here would fail the
        // boot on a firmware table this kernel does not otherwise care about.
        Handle(index as u32)
    }

    fn acquire(&self, mutex: Handle, timeout: u16) -> Result<(), acpi::aml::AmlError> {
        let owner = owner_key();
        let Some(slot) = aml_mutex(mutex) else {
            return Err(acpi::aml::AmlError::MutexAcquireTimeout);
        };

        // ACPI 6.5 §19.6.2: AML mutexes are reentrant for their owner, so a
        // second acquire by the same thread only deepens the count.
        if slot.owner.load(Ordering::Relaxed) == owner {
            slot.depth.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        let deadline = (timeout != INDEFINITE_TIMEOUT)
            .then(|| Instant::now() + Duration::from_millis(timeout as u64));
        loop {
            if slot
                .owner
                .compare_exchange(NO_OWNER, owner, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                slot.depth.store(1, Ordering::Relaxed);
                return Ok(());
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(acpi::aml::AmlError::MutexAcquireTimeout);
            }
            if current_thread_id().is_some() {
                thread_yield();
            } else {
                core::hint::spin_loop();
            }
        }
    }

    fn release(&self, mutex: Handle) {
        let Some(slot) = aml_mutex(mutex) else {
            return;
        };
        // A Release the owner did not pair with an Acquire is ignored rather
        // than decremented: taking the count below zero would wrap it and
        // leave `owner` set, so the mutex would stay held by a thread that has
        // already moved on and no later Acquire could ever take it.
        if slot.owner.load(Ordering::Relaxed) != owner_key() {
            return;
        }
        if slot.depth.fetch_sub(1, Ordering::Relaxed) == 1 {
            slot.owner.store(NO_OWNER, Ordering::Release);
        }
    }
}

/// The PCI address and config-space offset the 0xCF8/0xCFC pair can reach, or
/// `None` for the two things it cannot: a non-zero segment group, and the
/// extended config space above 256 bytes. Both need the MCFG mapping this
/// kernel does not have, so a read answers all-ones — what the bus returns for
/// a device that is not there — and a write is dropped.
fn legacy_config(address: acpi::PciAddress, offset: u16) -> Option<(PciAddress, u8)> {
    if address.segment() != 0 || offset > 0xFF {
        println!(
            "ACPI: PCI segment {} offset {offset:#x} needs MCFG, which this kernel does not map",
            address.segment()
        );
        return None;
    }
    Some((
        PciAddress {
            bus: address.bus(),
            device: address.device(),
            function: address.function(),
        },
        offset as u8,
    ))
}

/// A mutex the AML interpreter created, held by at most one thread at a time.
struct AmlMutex {
    /// The owning thread's id plus one, or [`NO_OWNER`].
    owner: AtomicU64,
    /// How many times the owner acquired it without releasing.
    depth: AtomicU32,
}

const NO_OWNER: u64 = 0;

/// `Handler::acquire`'s "wait forever" timeout.
const INDEFINITE_TIMEOUT: u16 = 0xFFFF;

/// Mutexes are declared by the DSDT and never freed, so they are a fixed table
/// indexed by handle rather than an allocation the interpreter has to track.
/// A DSDT declaring more than this gets handles past the end, which
/// [`aml_mutex`] answers `None` for.
static AML_MUTEXES: [AmlMutex; 128] = [const {
    AmlMutex {
        owner: AtomicU64::new(NO_OWNER),
        depth: AtomicU32::new(0),
    }
}; 128];

static AML_MUTEX_COUNT: AtomicUsize = AtomicUsize::new(0);

/// The slot a handle names, or `None` for a handle past the table.
fn aml_mutex(handle: Handle) -> Option<&'static AmlMutex> {
    AML_MUTEXES.get(handle.0 as usize)
}

/// The current thread's identity as an owner, distinct from [`NO_OWNER`]. Code
/// running before the scheduler exists is one owner, since it is one thread.
fn owner_key() -> u64 {
    current_thread_id().map_or(1, |tid| tid.0 + 2)
}
