use volatile::VolatilePtr;
use x86_64::{PhysAddr, VirtAddr, structures::paging::PageTableFlags};

use crate::{
    drivers::ahci::{
        AhciError,
        structures::{
            CommandHeader, CommandTable, HbaFis, HbaPort, HbaPortVolatileFieldAccess, PORT_CMD_CR,
            PORT_CMD_FR, PORT_CMD_FRE, PORT_CMD_ST,
        },
    },
    memory::{DMA_REGION_START, mapper::memory_mapper},
    println,
};

const AHCI_CMD_SLOTS: usize = 32;

#[derive(Debug)]
pub struct AhciPort {
    pub port_idx: usize,
    pub port_regs: VolatilePtr<'static, HbaPort>,

    // DMA regions
    pub command_list: DmaRegion<[CommandHeader; AHCI_CMD_SLOTS]>,
    pub fis_area: DmaRegion<HbaFis>,
    pub command_tables: [Option<DmaRegion<CommandTable>>; AHCI_CMD_SLOTS],

    // Command slot tracking
    pub free_slots: u32, // Bitmap of free command slots
}

#[derive(Debug)]
pub struct DmaRegion<T: 'static> {
    pub virt_addr: VirtAddr,
    pub data: &'static mut T,
}

impl<T> DmaRegion<T> {
    pub fn allocate() -> Result<Self, AhciError> {
        let size = core::mem::size_of::<T>() as u64;
        let aligned_size = (size + 0xfff) & !0xfff; // Round up to page boundary

        // Find a free virtual address (we'd need a virtual address allocator)
        // For now, let's use a simple approach with a static counter
        static NEXT_DMA_ADDR: core::sync::atomic::AtomicU64 =
            core::sync::atomic::AtomicU64::new(DMA_REGION_START.as_u64());

        let virt_addr = VirtAddr::new(
            NEXT_DMA_ADDR.fetch_add(aligned_size, core::sync::atomic::Ordering::Relaxed),
        );

        let mut mapper = memory_mapper();
        mapper
            .map_memory(
                virt_addr,
                aligned_size,
                PageTableFlags::WRITABLE | PageTableFlags::NO_CACHE,
            )
            .map_err(|_| AhciError::DmaAllocationFailed)?;

        let data = unsafe { &mut *(virt_addr.as_mut_ptr::<T>()) };

        // Zero the memory
        unsafe {
            core::ptr::write_bytes(data as *mut T, 0, core::mem::size_of::<T>());
        }

        Ok(Self { virt_addr, data })
    }

    pub fn phys_addr(&self) -> PhysAddr {
        let mapper = memory_mapper();
        match mapper.translate(self.virt_addr) {
            x86_64::structures::paging::mapper::TranslateResult::Mapped {
                frame, offset, ..
            } => frame.start_address() + offset,
            _ => panic!("DMA region not mapped!"),
        }
    }
}

impl AhciPort {
    pub fn new(
        port_idx: usize,
        port_regs: VolatilePtr<'static, HbaPort>,
    ) -> Result<Self, AhciError> {
        println!("Initializing AHCI port {}", port_idx);

        // Stop the port first
        Self::stop_port(port_regs)?;

        // Allocate DMA regions
        let command_list = DmaRegion::allocate()?;
        let fis_area = DmaRegion::allocate()?;

        // Set up the port registers
        port_regs
            .clb()
            .write(command_list.phys_addr().as_u64() as u32);
        port_regs
            .clbu()
            .write((command_list.phys_addr().as_u64() >> 32) as u32);
        port_regs.fb().write(fis_area.phys_addr().as_u64() as u32);
        port_regs
            .fbu()
            .write((fis_area.phys_addr().as_u64() >> 32) as u32);

        // Clear interrupt status
        port_regs.is().write(0xFFFFFFFF);

        // Enable FIS receive
        let mut cmd = port_regs.read().cmd;
        cmd |= PORT_CMD_FRE;
        port_regs.cmd().write(cmd);

        // Start the port
        cmd |= PORT_CMD_ST;
        port_regs.cmd().write(cmd);

        println!("Port {} initialized successfully", port_idx);

        Ok(Self {
            port_idx,
            port_regs,
            command_list,
            fis_area,
            command_tables: [const { None }; AHCI_CMD_SLOTS],
            free_slots: 0xFFFFFFFF, // All slots initially free
        })
    }

    fn stop_port(port_regs: VolatilePtr<'static, HbaPort>) -> Result<(), AhciError> {
        // Clear ST (start) bit
        let mut cmd = port_regs.cmd().read();
        cmd &= !PORT_CMD_ST;
        port_regs.cmd().write(cmd);

        // Wait for CR (command list running) to clear
        let start = crate::timer::Instant::now();
        while port_regs.read().cmd & PORT_CMD_CR != 0 {
            if start.elapsed().as_millis() > 500 {
                return Err(AhciError::CommandTimeout);
            }
            x86_64::instructions::hlt();
        }

        // Clear FRE (FIS receive enable)
        cmd = port_regs.cmd().read();
        cmd &= !PORT_CMD_FRE;
        port_regs.cmd().write(cmd);

        // Wait for FR (FIS receive running) to clear
        let start = crate::timer::Instant::now();
        while port_regs.cmd().read() & PORT_CMD_FR != 0 {
            if start.elapsed().as_millis() > 500 {
                return Err(AhciError::CommandTimeout);
            }
            x86_64::instructions::hlt();
        }

        Ok(())
    }

    pub fn allocate_command_slot(&mut self) -> Option<usize> {
        if self.free_slots == 0 {
            return None;
        }

        let slot = self.free_slots.trailing_zeros() as usize;
        self.free_slots &= !(1 << slot);
        Some(slot)
    }

    pub fn free_command_slot(&mut self, slot: usize) {
        if slot < AHCI_CMD_SLOTS {
            self.free_slots |= 1 << slot;
        }
    }

    pub fn handle_interrupt(&mut self) {
        let is = self.port_regs.is().read();

        // Clear interrupt status
        self.port_regs.is().write(is);

        // Handle completed commands
        let ci = self.port_regs.read().ci;
        let completed_slots = !ci & (!self.free_slots);

        for slot in 0..AHCI_CMD_SLOTS {
            if completed_slots & (1 << slot) != 0 {
                self.handle_command_completion(slot);
            }
        }

        if is & (1 << 30) != 0 {
            // Task file error
            println!("Port {} task file error", self.port_idx);
        }
    }

    fn handle_command_completion(&mut self, slot: usize) {
        println!("Command slot {} completed on port {}", slot, self.port_idx);

        // Free the command table
        if let Some(cmd_table) = self.command_tables[slot].take() {
            // Command table will be dropped and frame deallocated
        }

        // Mark slot as free
        self.free_command_slot(slot);

        // TODO: Notify waiting threads via broadcast/mailbox
    }
}
