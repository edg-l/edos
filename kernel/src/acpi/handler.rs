use core::ptr::NonNull;

use acpi::{Handle, Handler, PhysicalMapping};
use alloc::sync::Arc;
use spin::mutex::Mutex;
use x86_64::{
    PhysAddr, VirtAddr, align_up, instructions::port::Port, structures::paging::PageTableFlags,
};

use crate::{
    memory::{ACPI_MAPPINGS, mapper::memory_mapper},
    serial_println,
};

#[derive(Debug, Clone)]
pub struct AcpiHandler {
    current_addr: Arc<Mutex<VirtAddr>>,
}

impl AcpiHandler {
    pub fn new() -> Self {
        Self {
            current_addr: Arc::new(Mutex::new(ACPI_MAPPINGS)),
        }
    }
}

// ACPI handler manages its own "virtual address space", this is why using the physical address mapping isnt correct here, and we need to actually map
#[expect(unused)]
impl Handler for AcpiHandler {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> acpi::PhysicalMapping<Self, T> {
        let mut mapper = memory_mapper();
        let mut current_virt = self.current_addr.lock();
        let virt_start = *current_virt;

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
                serial_println!("ACPI: Mapping failed: {:?}", e);
                panic!("ACPI mapping failed");
            }
        }

        *current_virt += align_up(size as u64, 4096);

        PhysicalMapping {
            handler: self.clone(),
            mapped_length: size,
            physical_start: physical_address,
            region_length: size,
            virtual_start: NonNull::new(virt_start.as_mut_ptr()).unwrap(),
        }
    }

    fn unmap_physical_region<T>(_region: &acpi::PhysicalMapping<Self, T>) {}

    fn read_u8(&self, address: usize) -> u8 {
        serial_println!("reading {address:x}");
        unsafe { *(address as *const u8) }
    }

    fn read_u16(&self, address: usize) -> u16 {
        serial_println!("reading {address:x}");
        unsafe { *(address as *const u16) }
    }

    fn read_u32(&self, address: usize) -> u32 {
        serial_println!("reading {address:x}");
        unsafe { *(address as *const u32) }
    }

    fn read_u64(&self, address: usize) -> u64 {
        serial_println!("reading {address:x}");
        unsafe { *(address as *const u64) }
    }

    fn write_u8(&self, address: usize, value: u8) {
        serial_println!("writing {address:x}");
        unsafe {
            *(address as *mut u8) = value;
        }
    }

    fn write_u16(&self, address: usize, value: u16) {
        serial_println!("writing {address:x}");
        unsafe {
            *(address as *mut u16) = value;
        }
    }

    fn write_u32(&self, address: usize, value: u32) {
        serial_println!("writing {address:x}");
        unsafe {
            *(address as *mut u32) = value;
        }
    }

    fn write_u64(&self, address: usize, value: u64) {
        serial_println!("writing {address:x}");
        unsafe {
            *(address as *mut u64) = value;
        }
    }

    fn read_io_u8(&self, port: u16) -> u8 {
        serial_println!("read port {port}");
        unsafe { Port::new(port).read() }
    }

    fn read_io_u16(&self, port: u16) -> u16 {
        serial_println!("read port {port}");
        unsafe { Port::new(port).read() }
    }

    fn read_io_u32(&self, port: u16) -> u32 {
        serial_println!("read port {port}");
        unsafe { Port::new(port).read() }
    }

    fn write_io_u8(&self, port: u16, value: u8) {
        serial_println!("write port {port}");
        unsafe { Port::new(port).write(value) }
    }

    fn write_io_u16(&self, port: u16, value: u16) {
        serial_println!("write port {port}");
        unsafe { Port::new(port).write(value) }
    }

    fn write_io_u32(&self, port: u16, value: u32) {
        serial_println!("write port {port}");
        unsafe { Port::new(port).write(value) }
    }

    fn read_pci_u8(&self, address: acpi::PciAddress, offset: u16) -> u8 {
        serial_println!("called read pci 8");
        unimplemented!()
    }

    fn read_pci_u16(&self, address: acpi::PciAddress, offset: u16) -> u16 {
        serial_println!("called read_pci_u16");
        unimplemented!()
    }

    fn read_pci_u32(&self, address: acpi::PciAddress, offset: u16) -> u32 {
        serial_println!("called read_pci_u32");
        unimplemented!()
    }

    fn write_pci_u8(&self, address: acpi::PciAddress, offset: u16, value: u8) {
        serial_println!("called write_pci_u8");
        unimplemented!()
    }

    fn write_pci_u16(&self, address: acpi::PciAddress, offset: u16, value: u16) {
        serial_println!("called write_pci_u16");
        unimplemented!()
    }

    fn write_pci_u32(&self, address: acpi::PciAddress, offset: u16, value: u32) {
        serial_println!("called write_pci_u32");
        unimplemented!()
    }

    fn nanos_since_boot(&self) -> u64 {
        serial_println!("called nanos_since_boot");
        unimplemented!()
    }

    fn stall(&self, microseconds: u64) {
        serial_println!("called stall");
        unimplemented!()
    }

    fn sleep(&self, milliseconds: u64) {
        serial_println!("calledsleep");
        unimplemented!()
    }

    fn create_mutex(&self) -> Handle {
        serial_println!("called create_mutex");
        unimplemented!()
    }

    fn acquire(&self, mutex: Handle, timeout: u16) -> Result<(), acpi::aml::AmlError> {
        serial_println!("called acquire");
        unimplemented!()
    }

    fn release(&self, mutex: Handle) {
        serial_println!("called release");
        unimplemented!()
    }
}
