use core::ptr::NonNull;

use acpi::{Handle, Handler, PhysicalMapping};
use alloc::sync::Arc;
use spin::mutex::Mutex;
use x86_64::{
    PhysAddr, VirtAddr, align_up, instructions::port::Port, structures::paging::PageTableFlags,
};

use crate::{
    memory::{
        mapper::memory_mapper,
        valloc::{vfree, vmalloc},
    },
    println,
};

#[derive(Debug, Clone)]
pub struct AcpiHandler;

// ACPI handler manages its own "virtual address space", this is why using the physical address mapping isnt correct here, and we need to actually map
#[expect(unused)]
impl Handler for AcpiHandler {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> acpi::PhysicalMapping<Self, T> {
        let mut mapper = memory_mapper();
        let virt_start = vmalloc(size as u64);

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
        println!("reading {address:x}");
        unsafe { *(address as *const u8) }
    }

    fn read_u16(&self, address: usize) -> u16 {
        println!("reading {address:x}");
        unsafe { *(address as *const u16) }
    }

    fn read_u32(&self, address: usize) -> u32 {
        println!("reading {address:x}");
        unsafe { *(address as *const u32) }
    }

    fn read_u64(&self, address: usize) -> u64 {
        println!("reading {address:x}");
        unsafe { *(address as *const u64) }
    }

    fn write_u8(&self, address: usize, value: u8) {
        println!("writing {address:x}");
        unsafe {
            *(address as *mut u8) = value;
        }
    }

    fn write_u16(&self, address: usize, value: u16) {
        println!("writing {address:x}");
        unsafe {
            *(address as *mut u16) = value;
        }
    }

    fn write_u32(&self, address: usize, value: u32) {
        println!("writing {address:x}");
        unsafe {
            *(address as *mut u32) = value;
        }
    }

    fn write_u64(&self, address: usize, value: u64) {
        println!("writing {address:x}");
        unsafe {
            *(address as *mut u64) = value;
        }
    }

    fn read_io_u8(&self, port: u16) -> u8 {
        println!("read port {port}");
        unsafe { Port::new(port).read() }
    }

    fn read_io_u16(&self, port: u16) -> u16 {
        println!("read port {port}");
        unsafe { Port::new(port).read() }
    }

    fn read_io_u32(&self, port: u16) -> u32 {
        println!("read port {port}");
        unsafe { Port::new(port).read() }
    }

    fn write_io_u8(&self, port: u16, value: u8) {
        println!("write port {port}");
        unsafe { Port::new(port).write(value) }
    }

    fn write_io_u16(&self, port: u16, value: u16) {
        println!("write port {port}");
        unsafe { Port::new(port).write(value) }
    }

    fn write_io_u32(&self, port: u16, value: u32) {
        println!("write port {port}");
        unsafe { Port::new(port).write(value) }
    }

    fn read_pci_u8(&self, address: acpi::PciAddress, offset: u16) -> u8 {
        println!("called read pci 8");
        unimplemented!()
    }

    fn read_pci_u16(&self, address: acpi::PciAddress, offset: u16) -> u16 {
        println!("called read_pci_u16");
        unimplemented!()
    }

    fn read_pci_u32(&self, address: acpi::PciAddress, offset: u16) -> u32 {
        println!("called read_pci_u32");
        unimplemented!()
    }

    fn write_pci_u8(&self, address: acpi::PciAddress, offset: u16, value: u8) {
        println!("called write_pci_u8");
        unimplemented!()
    }

    fn write_pci_u16(&self, address: acpi::PciAddress, offset: u16, value: u16) {
        println!("called write_pci_u16");
        unimplemented!()
    }

    fn write_pci_u32(&self, address: acpi::PciAddress, offset: u16, value: u32) {
        println!("called write_pci_u32");
        unimplemented!()
    }

    fn nanos_since_boot(&self) -> u64 {
        println!("called nanos_since_boot");
        unimplemented!()
    }

    fn stall(&self, microseconds: u64) {
        println!("called stall");
        unimplemented!()
    }

    fn sleep(&self, milliseconds: u64) {
        println!("calledsleep");
        unimplemented!()
    }

    fn create_mutex(&self) -> Handle {
        println!("called create_mutex");
        unimplemented!()
    }

    fn acquire(&self, mutex: Handle, timeout: u16) -> Result<(), acpi::aml::AmlError> {
        println!("called acquire");
        unimplemented!()
    }

    fn release(&self, mutex: Handle) {
        println!("called release");
        unimplemented!()
    }
}
