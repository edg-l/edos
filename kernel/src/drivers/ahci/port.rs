use core::{
    ptr::{self},
    time::Duration,
};

use alloc::vec;

use x86_64::PhysAddr;

use crate::{
    drivers::{
        ahci::{
            AhciError, DeviceType,
            fis::FisRegH2D,
            structures::{
                CMD_HEADER_ATAPI, CMD_HEADER_WRITE, CommandHeader, CommandTable,
                DeviceIdentifyInfo, HbaFis, HbaPort, PORT_CMD_CR, PORT_CMD_FR, PORT_CMD_FRE,
                PORT_CMD_ST, PORT_IS_TFES, PrdtEntry, ScsiInquiry, ScsiRead10, ScsiReadCapacity10,
            },
        },
        dma::{DmaBuffer, DmaRegion, dma},
    },
    log,
    thread::scheduler::sched,
};

const AHCI_CMD_SLOTS: usize = 32;

#[expect(unused)]
#[derive(Debug)]
pub struct AhciPort {
    pub port_idx: usize,
    pub port_regs: *mut HbaPort,
    pub device_type: DeviceType,

    // DMA regions
    pub command_list: DmaRegion<[CommandHeader; AHCI_CMD_SLOTS]>,
    pub fis_area: DmaRegion<HbaFis>,
    pub command_tables: [Option<DmaRegion<CommandTable>>; AHCI_CMD_SLOTS],

    // Command slot tracking
    pub free_slots: u32, // Bitmap of free command slots

    // Reusable DMA buffer for ATA read/write. Allocated on first use, grown as needed.
    io_dma: Option<DmaBuffer>,
}

unsafe impl Send for AhciPort {}

#[expect(unused)]
impl AhciPort {
    pub fn new(
        port_idx: usize,
        port_regs: *mut HbaPort,
        device_type: DeviceType,
    ) -> Result<Self, AhciError> {
        log!("Initializing AHCI port {}", port_idx);

        // Stop the port first
        Self::stop_port(port_regs)?;

        // Allocate DMA regions
        let command_list = dma().allocate()?;
        let fis_area = dma().allocate()?;

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

        log!("Port {} initialized successfully", port_idx);

        Ok(Self {
            port_idx,
            port_regs,
            device_type,
            command_list,
            fis_area,
            command_tables: [const { None }; AHCI_CMD_SLOTS],
            free_slots: 0xFFFFFFFF, // All slots initially free
            io_dma: None,
        })
    }

    fn stop_port(port_regs: *mut HbaPort) -> Result<(), AhciError> {
        unsafe {
            // Clear ST (start) bit if set
            let mut cmd = ptr::read_volatile(&raw const (*port_regs).cmd);
            if cmd & PORT_CMD_ST != 0 {
                cmd &= !PORT_CMD_ST;
                ptr::write_volatile(&raw mut (*port_regs).cmd, cmd);

                // Wait for CR (command list running) to clear
                let start = crate::timer::Instant::now();
                while ptr::read_volatile(&raw const (*port_regs).cmd) & PORT_CMD_CR != 0 {
                    if start.elapsed().as_millis() > 500 {
                        return Err(AhciError::CommandTimeout);
                    }
                    sched().thread_sleep(core::time::Duration::from_millis(1));
                }
            }

            // Clear FRE (FIS receive enable) if set
            cmd = ptr::read_volatile(&raw const (*port_regs).cmd);
            if cmd & PORT_CMD_FRE != 0 {
                cmd &= !PORT_CMD_FRE;
                ptr::write_volatile(&raw mut (*port_regs).cmd, cmd);

                // Wait for FR (FIS receive running) to clear
                let start = crate::timer::Instant::now();
                while ptr::read_volatile(&raw const (*port_regs).cmd) & PORT_CMD_FR != 0 {
                    if start.elapsed().as_millis() > 500 {
                        return Err(AhciError::CommandTimeout);
                    }
                    sched().thread_sleep(core::time::Duration::from_millis(1));
                }
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

    pub fn set_device_type(&mut self, device_type: DeviceType) {
        self.device_type = device_type;
    }

    /// Ensure the reusable I/O DMA buffer is at least `min_size` bytes.
    /// Returns the physical address and a pointer to the buffer.
    fn ensure_io_dma(&mut self, min_size: usize) -> Result<(PhysAddr, *mut u8), AhciError> {
        if self.io_dma.is_none() || self.io_dma.as_ref().unwrap().size < min_size {
            self.io_dma = Some(DmaBuffer::allocate_sized(min_size)?);
        }
        let buf = self.io_dma.as_ref().unwrap();
        Ok((buf.phys_addr(), buf.as_ptr()))
    }

    /// Execute ATAPI command (SCSI packet command)
    fn execute_atapi_command(
        &mut self,
        scsi_cmd: &[u8],
        buffer_addr: PhysAddr,
        buffer_size: usize,
        timeout: Duration,
    ) -> Result<(), AhciError> {
        let slot = self
            .allocate_command_slot()
            .ok_or(AhciError::PortNotReady)?;

        // Setup ATAPI command table
        self.setup_atapi_command_table(slot, scsi_cmd, buffer_addr, buffer_size)?;

        // Issue and wait - ATAPI needs special command header flags
        self.issue_command(slot, CMD_HEADER_ATAPI, if buffer_size > 0 { 1 } else { 0 })?;
        let result = self.wait_for_command_completion(slot, timeout);

        // Always free the slot
        self.free_command_slot(slot);

        result
    }

    /// Setup ATAPI command table with PACKET FIS and SCSI CDB
    fn setup_atapi_command_table(
        &mut self,
        slot: usize,
        scsi_cmd: &[u8],
        buffer_addr: PhysAddr,
        buffer_size: usize,
    ) -> Result<(), AhciError> {
        if slot >= AHCI_CMD_SLOTS {
            return Err(AhciError::InvalidSlot);
        }

        if scsi_cmd.len() > 16 {
            return Err(AhciError::IoError); // ATAPI command too long
        }

        // Allocate command table if needed
        if self.command_tables[slot].is_none() {
            self.command_tables[slot] = Some(dma().allocate()?);
        }

        unsafe {
            let table = self.command_tables[slot]
                .as_ref()
                .expect("failed to get slot table")
                .get();
            table.write(core::mem::zeroed());
            let table = &mut *table;

            // For ATAPI, we need both CFIS (PACKET command) and ACMD (SCSI command)

            // 1. Setup Command FIS with PACKET command
            let packet_fis = FisRegH2D::new_atapi_packet(buffer_size as u16);
            let fis_bytes = bytemuck::bytes_of(&packet_fis);
            table.cfis[..fis_bytes.len()].copy_from_slice(fis_bytes);

            // 2. Setup ATAPI Command (SCSI CDB) in ACMD field
            table.acmd[..scsi_cmd.len()].copy_from_slice(scsi_cmd);

            // 3. Setup PRDT (Physical Region Descriptor Table) if we have data transfer
            if buffer_size > 0 {
                let prdt_entry = PrdtEntry {
                    dba: buffer_addr.as_u64() as u32,
                    dbau: (buffer_addr.as_u64() >> 32) as u32,
                    reserved: 0,
                    dbc: (buffer_size - 1) as u32, // Byte count - 1 (0-based)
                };

                ptr::write_volatile(&raw mut (*table).prdt[0], prdt_entry);
            }
        }

        Ok(())
    }

    /// Execute SCSI INQUIRY command and convert to DeviceIdentifyInfo
    fn execute_atapi_inquiry_as_identify(&mut self) -> Result<DeviceIdentifyInfo, AhciError> {
        let inquiry_cmd = ScsiInquiry::new();
        let data_buffer = dma().allocate_sized(96)?;

        let scsi_cmd_bytes = bytemuck::bytes_of(&inquiry_cmd);
        self.execute_atapi_command(
            scsi_cmd_bytes,
            data_buffer.phys_addr(),
            96,
            Duration::from_secs(5),
        )?;

        // Convert SCSI INQUIRY data to DeviceIdentifyInfo
        let inquiry_data = unsafe { core::slice::from_raw_parts(data_buffer.as_ptr(), 96) };
        let mut device_info = DeviceIdentifyInfo::from_scsi_inquiry(inquiry_data);

        // Try to get capacity information via READ_CAPACITY
        if let Ok(capacity) = self.execute_atapi_read_capacity() {
            device_info.sectors = capacity.0;
            device_info.capacity_mb = (capacity.0 * capacity.1 as u64) / (1024 * 1024);
            device_info.capacity_gb = device_info.capacity_mb / 1024;
        }

        dma().dealloc(data_buffer);
        Ok(device_info)
    }

    /// Execute SCSI READ_CAPACITY_10 to get device size
    fn execute_atapi_read_capacity(&mut self) -> Result<(u64, u32), AhciError> {
        let capacity_cmd = ScsiReadCapacity10::new();
        let data_buffer = dma().allocate_sized(8)?;

        let scsi_cmd_bytes = bytemuck::bytes_of(&capacity_cmd);
        self.execute_atapi_command(
            scsi_cmd_bytes,
            data_buffer.phys_addr(),
            8,
            Duration::from_secs(5),
        )?;

        // Parse READ_CAPACITY response: [last_lba(4), block_size(4)]
        let capacity_data = unsafe { core::slice::from_raw_parts(data_buffer.as_ptr(), 8) };

        let last_lba = u32::from_be_bytes([
            capacity_data[0],
            capacity_data[1],
            capacity_data[2],
            capacity_data[3],
        ]);
        let block_size = u32::from_be_bytes([
            capacity_data[4],
            capacity_data[5],
            capacity_data[6],
            capacity_data[7],
        ]);

        dma().dealloc(data_buffer);

        // Total sectors = last_lba + 1 (last_lba is 0-based)
        Ok(((last_lba as u64) + 1, block_size))
    }

    /// Execute SCSI READ_10 command for sector reading
    fn execute_atapi_read_as_sectors(
        &mut self,
        lba: u64,
        buffer: &mut [u8],
        sectors: u16,
    ) -> Result<(), AhciError> {
        let expected_size = sectors as usize * 2048; // CD/DVD sectors are 2048 bytes
        if buffer.len() < expected_size {
            return Err(AhciError::IoError);
        }

        // SCSI READ_10 uses 32-bit LBA
        if lba > u32::MAX as u64 {
            return Err(AhciError::IoError);
        }

        let read_cmd = ScsiRead10::new(lba as u32, sectors);
        let data_buffer = dma().allocate_sized(expected_size)?;

        let scsi_cmd_bytes = bytemuck::bytes_of(&read_cmd);
        self.execute_atapi_command(
            scsi_cmd_bytes,
            data_buffer.phys_addr(),
            expected_size,
            Duration::from_secs(10), // Optical drives can be slower
        )?;

        // Copy data to output buffer
        unsafe {
            ptr::copy_nonoverlapping(data_buffer.as_ptr(), buffer.as_mut_ptr(), expected_size);
        }

        dma().dealloc(data_buffer);
        Ok(())
    }

    /// Issue IDENTIFY command to get device information
    pub fn identify_device(&mut self) -> Result<DeviceIdentifyInfo, AhciError> {
        match self.device_type {
            DeviceType::Ata => self.execute_ata_identify(),
            DeviceType::Atapi => self.execute_atapi_inquiry_as_identify(),
        }
    }

    /// Execute ATA IDENTIFY command
    fn execute_ata_identify(&mut self) -> Result<DeviceIdentifyInfo, AhciError> {
        // Allocate DMA buffer for identify data (512 bytes)
        let data_buffer = dma().allocate_sized(512)?;

        self.execute_command(
            &FisRegH2D::new_identify(),
            data_buffer.phys_addr(),
            512,
            0,
            Duration::from_secs(5),
        )?;
        // Copy the identify data
        let result = unsafe { &*data_buffer.as_ptr().cast::<[u8; 512]>() };

        let info = DeviceIdentifyInfo::from_identify_data(result);

        dma().dealloc(data_buffer);

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
        match self.device_type {
            DeviceType::Ata => self.execute_ata_read(lba, buffer, sectors),
            DeviceType::Atapi => {
                // For ATAPI, need to handle different sector size (2048 vs 512)
                // For now, translate request to ATAPI format
                if sectors == 0 {
                    return Ok(());
                }

                // ATAPI sectors are typically 2048 bytes, but the API expects 512-byte sectors
                // We need to handle this translation
                let atapi_sectors = sectors.div_ceil(4); // Round up: 4 x 512-byte = 1 x 2048-byte
                let atapi_lba = lba / 4; // Each ATAPI sector contains 4 ATA sectors

                // Allocate larger buffer for ATAPI
                let atapi_buffer_size = atapi_sectors as usize * 2048;
                let mut temp_buffer = vec![0u8; atapi_buffer_size];

                self.execute_atapi_read_as_sectors(atapi_lba, &mut temp_buffer, atapi_sectors)?;

                // Copy the relevant portion to output buffer
                let start_offset = (lba % 4) as usize * 512;
                let copy_size = (sectors as usize * 512).min(buffer.len());
                let available_data = temp_buffer.len().saturating_sub(start_offset);
                let actual_copy = copy_size.min(available_data);

                buffer[..actual_copy]
                    .copy_from_slice(&temp_buffer[start_offset..start_offset + actual_copy]);
                Ok(())
            }
        }
    }

    /// Execute ATA read command
    fn execute_ata_read(
        &mut self,
        lba: u64,
        buffer: &mut [u8],
        sectors: u16,
    ) -> Result<(), AhciError> {
        let expected_size = sectors as usize * 512;
        if buffer.len() < expected_size {
            return Err(AhciError::IoError);
        }

        let (phys, dma_ptr) = self.ensure_io_dma(expected_size)?;

        self.execute_command(
            &FisRegH2D::new_read_dma_ext(lba, sectors),
            phys,
            expected_size,
            0,
            Duration::from_secs(5),
        )?;

        unsafe {
            ptr::copy_nonoverlapping(dma_ptr, buffer.as_mut_ptr(), expected_size);
        }

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
        match self.device_type {
            DeviceType::Ata => self.execute_ata_write(lba, buffer, sectors),
            DeviceType::Atapi => Err(AhciError::ReadOnly), // ATAPI devices don't support sector writes
        }
    }

    /// Execute ATA write command
    fn execute_ata_write(
        &mut self,
        lba: u64,
        buffer: &[u8],
        sectors: u16,
    ) -> Result<(), AhciError> {
        let expected_size = sectors as usize * 512;
        if buffer.len() < expected_size {
            return Err(AhciError::IoError);
        }

        let (phys, dma_ptr) = self.ensure_io_dma(expected_size)?;

        unsafe {
            ptr::copy_nonoverlapping(buffer.as_ptr(), dma_ptr, expected_size);
        }

        self.execute_command(
            &FisRegH2D::new_write_dma_ext(lba, sectors),
            phys,
            expected_size,
            CMD_HEADER_WRITE,
            Duration::from_secs(5),
        )?;

        Ok(())
    }

    pub fn flush_cache(&mut self) -> Result<(), AhciError> {
        match self.device_type {
            DeviceType::Ata => {
                self.execute_command(
                    &FisRegH2D::new_flush_cache(),
                    PhysAddr::zero(),
                    0,
                    0,
                    Duration::from_secs(5),
                )?;
                Ok(())
            }
            DeviceType::Atapi => {
                // ATAPI devices typically don't need explicit cache flushing
                // Most optical drives handle this automatically
                Ok(())
            }
        }
    }

    /// Issue a command to the hardware
    ///
    /// # Arguments
    /// * `slot` - Command slot number (0-31)
    /// * `flags` - Additional command header flags (write direction, etc.)
    pub fn issue_command(&mut self, slot: usize, flags: u16, prdtl: u16) -> Result<(), AhciError> {
        if slot >= AHCI_CMD_SLOTS {
            return Err(AhciError::InvalidSlot);
        }

        // Ensure command table is allocated for this slot
        if self.command_tables[slot].is_none() {
            return Err(AhciError::InvalidSlot);
        }

        // Setup command header in command list
        unsafe {
            let cmd_list = self
                .command_list
                .get()
                .as_mut()
                .expect("failed to get cmdlist");
            let cmd_header = &mut cmd_list[slot];

            cmd_header.flags = 5 | flags; // FIS length = 5 DWORDs + additional flags
            cmd_header.prdtl = prdtl;
            cmd_header.prdbc = 0; // Will be updated by hardware
            cmd_header.ctba = self.command_tables[slot]
                .as_ref()
                .expect("failed to get table in ctba")
                .phys_addr()
                .as_u64() as u32;
            cmd_header.ctbau = (self.command_tables[slot]
                .as_ref()
                .expect("failed to get table in ctbau")
                .phys_addr()
                .as_u64()
                >> 32) as u32;
            cmd_header.reserved = [0; 4];
        }

        // Issue command by setting bit in Command Issue register (read-modify-write
        // to preserve other in-flight command bits).
        unsafe {
            let ci = ptr::read_volatile(&raw const (*self.port_regs).ci);
            ptr::write_volatile(&raw mut (*self.port_regs).ci, ci | (1 << slot));
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
        let port_regs = self.port_regs;

        loop {
            // Check if command completed (slot bit cleared in CI register)
            let ci = unsafe { ptr::read_volatile(&raw const (*port_regs).ci) };
            if ci & (1 << slot) == 0 {
                return Ok(());
            }

            // Check for errors
            let is = unsafe { ptr::read_volatile(&raw const (*port_regs).is) };
            if is & PORT_IS_TFES != 0 {
                unsafe { ptr::write_volatile(&raw mut (*self.port_regs).is, PORT_IS_TFES) };
                let tfd = unsafe { ptr::read_volatile(&raw const (*port_regs).tfd) };
                let status = (tfd & 0xFF) as u8;
                let error = ((tfd >> 8) & 0xFF) as u8;
                log!(
                    "AHCI port {}: Command error - Status: {:#x}, Error: {:#x}",
                    self.port_idx,
                    status,
                    error
                );
                return Err(AhciError::IoError);
            }

            if start_time.elapsed() >= timeout {
                log!(
                    "AHCI port {}: Command timeout on slot {}",
                    self.port_idx,
                    slot
                );
                return Err(AhciError::CommandTimeout);
            }

            // Park until the AHCI main thread wakes us via wake_thread.
            // The condition re-checks CI/IS/timeout so we don't miss events.
            sched().thread_park_while(|| {
                let ci = unsafe { ptr::read_volatile(&raw const (*port_regs).ci) };
                let is = unsafe { ptr::read_volatile(&raw const (*port_regs).is) };
                ci & (1 << slot) != 0 && is & PORT_IS_TFES == 0 && start_time.elapsed() < timeout
            });
        }
    }

    /// Check if any commands have completed (non-blocking)
    /// Returns a bitmask of completed slots
    pub fn check_completed_commands(&self) -> u32 {
        let ci = unsafe { ptr::read_volatile(&raw const (*self.port_regs).ci) };
        let issued_mask = (!self.free_slots); // Slots that were issued
        // Issued slots that are no longer in CI
        issued_mask & !ci
    }

    /// Check for any port errors (non-blocking)
    pub fn check_errors(&mut self) -> Option<AhciError> {
        let is = unsafe { ptr::read_volatile(&raw const (*self.port_regs).is) };

        if is & PORT_IS_TFES != 0 {
            // Clear only the TFES bit to preserve other pending interrupt status
            unsafe { ptr::write_volatile(&raw mut (*self.port_regs).is, PORT_IS_TFES) };

            let tfd = unsafe { ptr::read_volatile(&raw const (*self.port_regs).tfd) };
            let status = (tfd & 0xFF) as u8;
            let error = ((tfd >> 8) & 0xFF) as u8;

            log!(
                "AHCI port {}: Error detected - Status: {:#x}, Error: {:#x}",
                self.port_idx,
                status,
                error
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
            self.command_tables[slot] = Some(dma().allocate()?);
        }

        let has_data = buffer_size > 0;

        unsafe {
            let table = self.command_tables[slot]
                .as_ref()
                .expect("failed to get slot table")
                .get();
            table.write(core::mem::zeroed());
            let table = &mut *table;

            // Setup Command FIS
            let fis_bytes = bytemuck::bytes_of(fis);
            table.cfis[..fis_bytes.len()].copy_from_slice(fis_bytes);

            // Setup PRDT only when there is a data transfer
            if has_data {
                let prdt_entry = PrdtEntry {
                    dba: buffer_addr.as_u64() as u32,
                    dbau: (buffer_addr.as_u64() >> 32) as u32,
                    reserved: 0,
                    dbc: (buffer_size - 1) as u32, // Byte count - 1 (0-based)
                };

                ptr::write_volatile(&raw mut (*table).prdt[0], prdt_entry);
            }
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
        let prdtl = if buffer_size > 0 { 1 } else { 0 };
        self.issue_command(slot, flags, prdtl)?;
        let result = self.wait_for_command_completion(slot, timeout);

        // Always free the slot
        self.free_command_slot(slot);

        result
    }
}
