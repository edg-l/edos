use core::{
    ptr::{self, null_mut},
    time::Duration,
};

use x86_64::PhysAddr;

use crate::{
    drivers::ahci::{
        AhciError,
        dma::{DmaAllocator, DmaRegion},
        fis::FisRegH2D,
        structures::{
            CMD_HEADER_WRITE, CommandHeader, CommandTable, DeviceIdentifyInfo, HbaFis, HbaPort,
            PORT_CMD_CR, PORT_CMD_FR, PORT_CMD_FRE, PORT_CMD_ST, PORT_IS_TFES, PrdtEntry,
        },
    },
    println,
};

const AHCI_CMD_SLOTS: usize = 32;

#[expect(unused)]
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
    pub dma_alloc: DmaAllocator,
}

#[expect(unused)]
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
            dma_alloc: DmaAllocator::default(),
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
        let data_buffer = self.dma_alloc.allocate_sized(512)?;

        self.execute_command(
            &FisRegH2D::new_identify(),
            data_buffer.phys_addr(),
            512,
            0,
            Duration::from_secs(5),
        )?;
        // Copy the identify data
        let result = unsafe { &*data_buffer.as_ptr().cast::<[u8; 512]>() };

        // Clean up
        self.free_command_slot(slot);

        let info = DeviceIdentifyInfo::from_identify_data(result);

        self.dma_alloc.dealloc(data_buffer);

        Ok(info)
    }

    // Read sectors from the device
    ///
    /// # Arguments
    /// * `lba` - Logical block address to start reading from
    /// * `buffer` - Buffer to read data into (must be at least sectors * 512 bytes)
    /// * `sectors` - Number of sectors to read (each sector is 512 bytes)
    pub fn read_sectors(
        &mut self,
        lba: u64,
        buffer: &mut [u8],
        sectors: u16,
    ) -> Result<(), AhciError> {
        let expected_size = sectors as usize * 512;
        if buffer.len() < expected_size {
            return Err(AhciError::IoError);
        }

        // Allocate command slot
        let slot = self
            .allocate_command_slot()
            .ok_or(AhciError::PortNotReady)?;

        // Allocate DMA buffer for read data (sized for multiple sectors)
        let data_buffer = self.dma_alloc.allocate_sized(expected_size)?;

        self.execute_command(
            &FisRegH2D::new_read_dma_ext(lba, sectors),
            data_buffer.phys_addr(),
            expected_size,
            0,
            Duration::from_secs(5),
        )?;

        // Copy the read data to the output buffer
        let bytes_to_copy = expected_size.min(buffer.len());
        unsafe {
            ptr::copy_nonoverlapping(data_buffer.as_ptr(), buffer.as_mut_ptr(), bytes_to_copy);
        }

        self.free_command_slot(slot);

        self.dma_alloc.dealloc(data_buffer);

        Ok(())
    }

    /// Write sectors to the device
    ///
    /// # Arguments
    /// * `lba` - Logical block address to start writing to
    /// * `buffer` - Buffer containing data to write (must be at least sectors * 512 bytes)
    /// * `sectors` - Number of sectors to write (each sector is 512 bytes)
    pub fn write_sectors(
        &mut self,
        lba: u64,
        buffer: &[u8],
        sectors: u16,
    ) -> Result<(), AhciError> {
        let expected_size = sectors as usize * 512;
        if buffer.len() < expected_size {
            return Err(AhciError::IoError);
        }

        // Allocate command slot
        let slot = self
            .allocate_command_slot()
            .ok_or(AhciError::PortNotReady)?;

        // Allocate DMA buffer for write data (sized for multiple sectors)
        let data_buffer = self.dma_alloc.allocate_sized(expected_size)?;

        // Copy input data to DMA buffer
        unsafe {
            ptr::copy_nonoverlapping(buffer.as_ptr(), data_buffer.as_ptr(), expected_size);
        }

        self.execute_command(
            &FisRegH2D::new_write_dma_ext(lba, sectors),
            data_buffer.phys_addr(),
            expected_size,
            CMD_HEADER_WRITE,
            Duration::from_secs(5),
        )?;

        self.free_command_slot(slot);

        self.dma_alloc.dealloc(data_buffer);
        Ok(())
    }

    pub fn flush_cache(&mut self) -> Result<(), AhciError> {
        // Allocate command slot
        let slot = self
            .allocate_command_slot()
            .ok_or(AhciError::PortNotReady)?;

        self.execute_command(
            &FisRegH2D::new_flush_cache(),
            PhysAddr::zero(),
            0,
            0,
            Duration::from_secs(5),
        )?;

        self.free_command_slot(slot);
        Ok(())
    }

    /// Issue a command to the hardware
    ///
    /// # Arguments
    /// * `slot` - Command slot number (0-31)
    /// * `flags` - Additional command header flags (write direction, etc.)
    pub fn issue_command(&mut self, slot: usize, flags: u16) -> Result<(), AhciError> {
        if slot >= AHCI_CMD_SLOTS {
            return Err(AhciError::InvalidSlot);
        }

        // Ensure command table is allocated for this slot
        if self.command_tables[slot].is_none() {
            return Err(AhciError::InvalidSlot);
        }

        // Setup command header in command list
        unsafe {
            let cmd_list = self.command_list.get().as_mut().unwrap();
            let cmd_header = &mut cmd_list[slot];

            cmd_header.flags = 5 | flags; // FIS length = 5 DWORDs + additional flags
            cmd_header.prdtl = 1; // One PRDT entry (could be parameterized)
            cmd_header.prdbc = 0; // Will be updated by hardware
            cmd_header.ctba = self.command_tables[slot]
                .as_ref()
                .unwrap()
                .phys_addr()
                .as_u64() as u32;
            cmd_header.ctbau = (self.command_tables[slot]
                .as_ref()
                .unwrap()
                .phys_addr()
                .as_u64()
                >> 32) as u32;
            cmd_header.reserved = [0; 4];
        }

        // Issue command by setting bit in Command Issue register
        unsafe {
            ptr::write_volatile(&raw mut (*self.port_regs).ci, 1 << slot);
        }

        Ok(())
    }

    /// Wait for command completion with timeout
    ///
    /// # Arguments
    /// * `slot` - Command slot to wait for
    /// * `timeout` - Maximum time to wait
    ///
    /// # Returns
    /// * `Ok(())` - Command completed successfully
    /// * `Err(AhciError)` - Timeout, error, or other failure
    pub fn wait_for_command_completion(
        &mut self,
        slot: usize,
        timeout: core::time::Duration,
    ) -> Result<(), AhciError> {
        if slot >= AHCI_CMD_SLOTS {
            return Err(AhciError::InvalidSlot);
        }

        let start_time = crate::timer::Instant::now();

        loop {
            // Check if command completed (slot bit cleared in CI register)
            let ci = unsafe { ptr::read_volatile(&raw const (*self.port_regs).ci) };
            if ci & (1 << slot) == 0 {
                // Command completed successfully
                break;
            }

            // Check for errors before waiting
            let is = unsafe { ptr::read_volatile(&raw const (*self.port_regs).is) };

            // Task File Error Status - indicates command failed
            if is & PORT_IS_TFES != 0 {
                // Clear the error interrupt
                unsafe { ptr::write_volatile(&raw mut (*self.port_regs).is, is) };

                // Get detailed error information from task file data
                let tfd = unsafe { ptr::read_volatile(&raw const (*self.port_regs).tfd) };
                let status = (tfd & 0xFF) as u8;
                let error = ((tfd >> 8) & 0xFF) as u8;

                println!(
                    "AHCI port {}: Command error - Status: {:#x}, Error: {:#x}",
                    self.port_idx, status, error
                );

                return Err(AhciError::IoError);
            }

            // Check for timeout
            if start_time.elapsed() >= timeout {
                println!(
                    "AHCI port {}: Command timeout on slot {}",
                    self.port_idx, slot
                );
                return Err(AhciError::CommandTimeout);
            }

            // Yield to scheduler while waiting
            // In threaded design, this would be where the thread sleeps
            // and gets woken by interrupt
            crate::thread::scheduler::sched()
                .thread_wait_timeout(core::time::Duration::from_millis(100));
        }

        // Clear any remaining interrupts for this completion
        let is = unsafe { ptr::read_volatile(&raw const (*self.port_regs).is) };
        if is != 0 {
            unsafe { ptr::write_volatile(&raw mut (*self.port_regs).is, is) };
        }

        Ok(())
    }

    /// Check if any commands have completed (non-blocking)
    /// Returns a bitmask of completed slots
    pub fn check_completed_commands(&self) -> u32 {
        let ci = unsafe { ptr::read_volatile(&raw const (*self.port_regs).ci) };
        let issued_mask = (!self.free_slots) & 0xFFFFFFFF; // Slots that were issued
        let completed_mask = issued_mask & !ci; // Issued slots that are no longer in CI
        completed_mask
    }

    /// Check for any port errors (non-blocking)
    pub fn check_errors(&mut self) -> Option<AhciError> {
        let is = unsafe { ptr::read_volatile(&raw const (*self.port_regs).is) };

        if is & PORT_IS_TFES != 0 {
            // Clear the error
            unsafe { ptr::write_volatile(&raw mut (*self.port_regs).is, is) };

            let tfd = unsafe { ptr::read_volatile(&raw const (*self.port_regs).tfd) };
            let status = (tfd & 0xFF) as u8;
            let error = ((tfd >> 8) & 0xFF) as u8;

            println!(
                "AHCI port {}: Error detected - Status: {:#x}, Error: {:#x}",
                self.port_idx, status, error
            );

            Some(AhciError::IoError)
        } else {
            None
        }
    }

    /// Setup a command table with FIS and PRDT
    pub fn setup_command_table(
        &mut self,
        slot: usize,
        fis: &FisRegH2D,
        buffer_addr: x86_64::PhysAddr,
        buffer_size: usize,
    ) -> Result<(), AhciError> {
        if slot >= AHCI_CMD_SLOTS {
            return Err(AhciError::InvalidSlot);
        }

        // Allocate command table if needed
        if self.command_tables[slot].is_none() {
            self.command_tables[slot] = Some(DmaRegion::<CommandTable>::allocate()?);
        }

        unsafe {
            let table = self.command_tables[slot].as_ref().unwrap().get();
            table.write(core::mem::zeroed());
            let table = &mut *table;

            // Setup Command FIS
            let fis_bytes = bytemuck::bytes_of(fis);
            table.cfis[..fis_bytes.len()].copy_from_slice(fis_bytes);

            // Setup PRDT (Physical Region Descriptor Table)
            let prdt_entry = PrdtEntry {
                dba: buffer_addr.as_u64() as u32,
                dbau: (buffer_addr.as_u64() >> 32) as u32,
                reserved: 0,
                dbc: (buffer_size - 1) as u32, // Byte count - 1 (0-based)
            };

            // Write PRDT entry right after the CommandTable
            let prdt_ptr = (table as *mut CommandTable as *mut u8)
                .add(core::mem::size_of::<CommandTable>())
                as *mut PrdtEntry;
            ptr::write_volatile(prdt_ptr, prdt_entry);
        }

        Ok(())
    }

    /// High-level command execution (setup + issue + wait)
    pub fn execute_command(
        &mut self,
        fis: &FisRegH2D,
        buffer_addr: x86_64::PhysAddr,
        buffer_size: usize,
        flags: u16,
        timeout: core::time::Duration,
    ) -> Result<(), AhciError> {
        let slot = self
            .allocate_command_slot()
            .ok_or(AhciError::PortNotReady)?;

        // Setup the command
        self.setup_command_table(slot, fis, buffer_addr, buffer_size)?;

        // Issue and wait
        self.issue_command(slot, flags)?;
        let result = self.wait_for_command_completion(slot, timeout);

        // Always free the slot
        self.free_command_slot(slot);

        result
    }
}
