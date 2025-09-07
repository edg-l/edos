use alloc::vec::Vec;
use core::{marker::PhantomData, ptr};
use x86_64::{
    PhysAddr, VirtAddr, instructions::interrupts::without_interrupts, registers::control::Cr3,
    structures::paging::PageTableFlags,
};

use crate::{
    boot::boot_info,
    drivers::ahci::{
        AhciError,
        fis::FisRegH2D,
        structures::{
            CommandHeader, CommandTable, DeviceIdentifyInfo, HbaFis, HbaPort, PORT_CMD_CR,
            PORT_CMD_FR, PORT_CMD_FRE, PORT_CMD_ST, PORT_IS_TFES, PrdtEntry,
        },
    },
    memory::{DMA_REGION_START, mapper::memory_mapper},
    println,
};

const AHCI_CMD_SLOTS: usize = 32;

#[derive(Debug)]
pub struct AhciPort {
    pub port_idx: usize,
    pub port_regs: *mut HbaPort,

    // DMA regions
    pub command_list: DmaRegion<[CommandHeader; AHCI_CMD_SLOTS]>,
    pub fis_area: DmaRegion<HbaFis>,
    pub command_tables: [Option<DmaRegion<CommandTable>>; AHCI_CMD_SLOTS],

    // Command slot tracking
    pub free_slots: u32, // Bitmap of free command slots
}

#[derive(Debug, Clone, Copy)]
pub struct DmaRegion<T: 'static> {
    pub virt_addr: VirtAddr,
    _phantom: PhantomData<T>,
}

impl<T> DmaRegion<T> {
    pub fn get(&self) -> *mut T {
        self.virt_addr.as_mut_ptr()
    }

    pub fn allocate() -> Result<Self, AhciError> {
        let size = core::mem::size_of::<T>() as u64 * 2;
        let aligned_size = (size + 0xfff) & !0xfff; // Round up to page boundary

        // Find a free virtual address (we'd need a virtual address allocator)
        // For now, let's use a simple approach with a static counter
        static NEXT_DMA_ADDR: core::sync::atomic::AtomicU64 =
            core::sync::atomic::AtomicU64::new(DMA_REGION_START.as_u64());

        let virt_addr = VirtAddr::new(
            NEXT_DMA_ADDR.fetch_add(aligned_size, core::sync::atomic::Ordering::Relaxed),
        );

        {
            let mut mapper = memory_mapper();
            without_interrupts(|| {
                let kernel_cr3 = boot_info().cr3;
                unsafe { Cr3::write(kernel_cr3.0, kernel_cr3.1) };

                mapper
                    .map_memory(
                        virt_addr,
                        aligned_size,
                        PageTableFlags::WRITABLE
                            | PageTableFlags::NO_CACHE
                            | PageTableFlags::GLOBAL,
                    )
                    .map_err(|_| AhciError::DmaAllocationFailed)
            })?;
        }

        // Zero the memory
        unsafe {
            ptr::write_bytes(virt_addr.as_mut_ptr::<T>(), 0, 1);
        }

        Ok(Self {
            virt_addr,
            _phantom: PhantomData,
        })
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
    pub fn new(port_idx: usize, port_regs: *mut HbaPort) -> Result<Self, AhciError> {
        println!("Initializing AHCI port {}", port_idx);

        // Stop the port first
        Self::stop_port(port_regs)?;

        // Allocate DMA regions
        let command_list = DmaRegion::allocate()?;
        let fis_area = DmaRegion::allocate()?;

        // Set up the port registers
        unsafe {
            ptr::write_volatile(
                &raw mut (*port_regs).clb,
                command_list.phys_addr().as_u64() as u32,
            );
            ptr::write_volatile(
                &raw mut (*port_regs).clbu,
                (command_list.phys_addr().as_u64() >> 32) as u32,
            );
            ptr::write_volatile(
                &raw mut (*port_regs).fb,
                fis_area.phys_addr().as_u64() as u32,
            );
            ptr::write_volatile(
                &raw mut (*port_regs).fbu,
                (fis_area.phys_addr().as_u64() >> 32) as u32,
            );

            // Clear interrupt status
            ptr::write_volatile(&raw mut (*port_regs).is, 0xFFFFFFFF);

            // Enable FIS receive
            let mut cmd = ptr::read_volatile(&raw const (*port_regs).cmd);
            cmd |= PORT_CMD_FRE;
            ptr::write_volatile(&raw mut (*port_regs).cmd, cmd);

            // Start the port
            cmd |= PORT_CMD_ST;
            ptr::write_volatile(&raw mut (*port_regs).cmd, cmd);
        }

        unsafe {
            // Enable specific interrupts we care about
            let ie = (1 << 0) |  // DHRS - Device to Host Register FIS
             (1 << 2) |  // DSS - DMA Setup FIS
             (1 << 5) |  // DPS - Descriptor Processed
             (1 << 30); // TFES - Task File Error

            ptr::write_volatile(&raw mut (*port_regs).ie, ie);
        }

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

    fn stop_port(port_regs: *mut HbaPort) -> Result<(), AhciError> {
        unsafe {
            // Clear ST (start) bit
            let mut cmd = ptr::read_volatile(&(*port_regs).cmd);
            cmd &= !PORT_CMD_ST;
            ptr::write_volatile(&raw mut (*port_regs).cmd, cmd);

            // Wait for CR (command list running) to clear
            let start = crate::timer::Instant::now();
            while ptr::read_volatile(&raw const (*port_regs).cmd) & PORT_CMD_CR != 0 {
                if start.elapsed().as_millis() > 500 {
                    return Err(AhciError::CommandTimeout);
                }
                x86_64::instructions::hlt();
            }

            // Clear FRE (FIS receive enable)
            cmd = ptr::read_volatile(&raw const (*port_regs).cmd);
            cmd &= !PORT_CMD_FRE;
            ptr::write_volatile(&raw mut (*port_regs).cmd, cmd);

            // Wait for FR (FIS receive running) to clear
            let start = crate::timer::Instant::now();
            while ptr::read_volatile(&raw const (*port_regs).cmd) & PORT_CMD_FR != 0 {
                if start.elapsed().as_millis() > 500 {
                    return Err(AhciError::CommandTimeout);
                }
                x86_64::instructions::hlt();
            }
        }

        println!("Port stopped");

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

    /// Issue IDENTIFY command to get device information
    pub fn identify_device(&mut self) -> Result<DeviceIdentifyInfo, AhciError> {
        // Allocate command slot
        let slot = self
            .allocate_command_slot()
            .ok_or(AhciError::PortNotReady)?;

        // Allocate DMA buffer for identify data (512 bytes)
        let data_buffer = DmaRegion::<[u8; 512]>::allocate()?;

        // Allocate command table for this slot
        let cmd_table = DmaRegion::<CommandTable>::allocate()?;
        self.command_tables[slot] = Some(cmd_table);

        // Setup command table
        unsafe {
            let table = self.command_tables[slot].as_ref().unwrap().get();

            // Zero the command table
            table.write(core::mem::zeroed());

            let table = &mut *table;

            // Setup Command FIS (Host to Device Register FIS)
            let fis = FisRegH2D::new_identify();
            let fis_bytes = bytemuck::bytes_of(&fis);
            table.cfis[..fis_bytes.len()].copy_from_slice(fis_bytes);

            // Setup PRDT (Physical Region Descriptor Table) - immediately after CommandTable
            let prdt_entry = PrdtEntry {
                dba: data_buffer.phys_addr().as_u64() as u32,
                dbau: (data_buffer.phys_addr().as_u64() >> 32) as u32,
                reserved: 0,
                dbc: 512 - 1, // Byte count - 1 (0-based)
            };

            // Write PRDT entry right after the CommandTable
            let prdt_ptr = (table as *mut CommandTable as *mut u8)
                .add(core::mem::size_of::<CommandTable>())
                as *mut PrdtEntry;
            ptr::write_volatile(prdt_ptr, prdt_entry);
        }

        // Setup command header in command list
        unsafe {
            let cmd_list = self.command_list.get().as_mut().unwrap();
            let cmd_header = &mut cmd_list[slot];
            *cmd_header = CommandHeader {
                flags: 5, // FIS length = 5 DWORDs (20 bytes)
                prdtl: 1, // One PRDT entry
                prdbc: 0, // Will be updated by hardware
                ctba: self.command_tables[slot]
                    .as_ref()
                    .unwrap()
                    .phys_addr()
                    .as_u64() as u32,
                ctbau: (self.command_tables[slot]
                    .as_ref()
                    .unwrap()
                    .phys_addr()
                    .as_u64()
                    >> 32) as u32,
                reserved: [0; 4],
            };
        }

        // Issue command by setting bit in Command Issue register
        unsafe {
            ptr::write_volatile(&raw mut (*self.port_regs).ci, 1 << slot);
        }

        // Wait for command completion - interrupt will wake the driver thread
        let timeout = core::time::Duration::from_secs(5);
        let start_time = crate::timer::Instant::now();

        loop {
            // Check if command completed (slot bit cleared in CI register)
            let ci = unsafe { ptr::read_volatile(&raw const (*self.port_regs).ci) };
            if ci & (1 << slot) == 0 {
                break;
            }

            // Check for errors before waiting
            let is = unsafe { ptr::read_volatile(&raw const (*self.port_regs).is) };
            if is & PORT_IS_TFES != 0 {
                println!("IDENTIFY command failed with task file error");
                unsafe { ptr::write_volatile(&raw mut (*self.port_regs).is, is) };
                self.free_command_slot(slot);
                return Err(AhciError::IoError);
            }

            // Check if we've exceeded our total timeout
            if start_time.elapsed() >= timeout {
                println!("IDENTIFY command timed out");
                self.free_command_slot(slot);
                return Err(AhciError::CommandTimeout);
            }

            // Park the driver thread - interrupt will wake us when command completes
            // Use shorter waits so we can check timeout more frequently
            crate::thread::scheduler::sched()
                .thread_wait_timeout(core::time::Duration::from_millis(100));
        }

        // Copy the identify data
        let result = unsafe { *data_buffer.get() };

        // Clean up
        self.free_command_slot(slot);

        Ok(DeviceIdentifyInfo::from_identify_data(&result))
    }
}
