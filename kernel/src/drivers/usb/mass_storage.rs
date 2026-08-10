#![expect(unused)]
//! USB Mass Storage class driver - Bulk-Only Transport (BOT) with SCSI commands.
//!
//! Implements the USB Mass Storage Class specification using the Bulk-Only Transport
//! protocol (BBB, subclass=0x06 SCSI transparent, protocol=0x50 BOT).

use alloc::sync::Arc;

use crate::{
    drivers::dma::{DmaBuffer, dma},
    fs::devfs::{self, DevFsDevice, DevFsError},
    println,
};

use super::xhci::{XhciController, XhciError, rings::TransferRing};

// ---------------------------------------------------------------------------
// BOT constants
// ---------------------------------------------------------------------------

/// Command Block Wrapper signature "USBC" in little-endian.
const CBW_SIGNATURE: u32 = u32::from_le_bytes(*b"USBC");
/// Command Status Wrapper signature "USBS" in little-endian.
const CSW_SIGNATURE: u32 = u32::from_le_bytes(*b"USBS");

// ---------------------------------------------------------------------------
// BOT structures
// ---------------------------------------------------------------------------

/// Command Block Wrapper (31 bytes) - sent on the bulk OUT endpoint.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct Cbw {
    pub d_cbw_signature: u32,
    pub d_cbw_tag: u32,
    pub d_cbw_data_transfer_length: u32,
    /// bit 7: 0 = Data-Out (host-to-device), 1 = Data-In (device-to-host)
    pub bm_cbw_flags: u8,
    pub b_cbw_lun: u8,
    /// Length of the SCSI command in cbwcb (1-16).
    pub b_cbw_cb_length: u8,
    /// The SCSI Command Descriptor Block.
    pub cbwcb: [u8; 16],
}

/// Command Status Wrapper (13 bytes) - received on the bulk IN endpoint.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct Csw {
    pub d_csw_signature: u32,
    pub d_csw_tag: u32,
    pub d_csw_data_residue: u32,
    /// 0 = command passed, 1 = command failed, 2 = phase error.
    pub b_csw_status: u8,
}

// ---------------------------------------------------------------------------
// UsbMassStorage
// ---------------------------------------------------------------------------

/// State for a detected USB mass storage device.
pub struct UsbMassStorage {
    pub slot_id: u8,
    /// xHCI DCI for the bulk IN endpoint.
    pub ep_in_dci: u32,
    /// xHCI DCI for the bulk OUT endpoint.
    pub ep_out_dci: u32,
    /// Logical block size in bytes (filled by READ CAPACITY).
    pub block_size: u32,
    /// Total number of logical blocks (filled by READ CAPACITY).
    pub block_count: u64,
    tag: u32,
    /// Pre-allocated DMA buffer for Command Block Wrappers (31 bytes), reused per transfer.
    cbw_buf: DmaBuffer,
    /// Pre-allocated DMA buffer for Command Status Wrappers (13 bytes), reused per transfer.
    csw_buf: DmaBuffer,
}

impl UsbMassStorage {
    pub fn new(slot_id: u8, ep_in_addr: u8, ep_out_addr: u8) -> Self {
        let ep_in_num = ep_in_addr & 0x0F;
        let ep_out_num = ep_out_addr & 0x0F;
        // xHCI DCI formula: ep_num * 2 + (1 if IN, 0 if OUT)
        let ep_in_dci = (ep_in_num * 2 + 1) as u32;
        let ep_out_dci = (ep_out_num * 2) as u32;
        let cbw_buf = dma()
            .allocate_sized(31)
            .expect("usb-msc: failed to allocate CBW buffer");
        let csw_buf = dma()
            .allocate_sized(13)
            .expect("usb-msc: failed to allocate CSW buffer");
        Self {
            slot_id,
            ep_in_dci,
            ep_out_dci,
            block_size: 512,
            block_count: 0,
            tag: 1,
            cbw_buf,
            csw_buf,
        }
    }

    fn next_tag(&mut self) -> u32 {
        let t = self.tag;
        self.tag = self.tag.wrapping_add(1);
        t
    }

    // -----------------------------------------------------------------------
    // SCSI commands
    // -----------------------------------------------------------------------

    /// Smallest INQUIRY response that carries a meaningful header: peripheral
    /// device type through ADDITIONAL LENGTH (SPC-4 §6.4.2).
    const MIN_INQUIRY_BYTES: usize = 5;

    /// SCSI INQUIRY (opcode 0x12) - identifies the device type and vendor strings.
    ///
    /// Returns the standard INQUIRY data, zero-padded to 36 bytes when the
    /// device answers with fewer than the allocation length.
    pub fn inquiry(
        &mut self,
        controller: &mut XhciController,
        in_ring: &mut TransferRing,
        out_ring: &mut TransferRing,
    ) -> Result<[u8; 36], XhciError> {
        let mut cmd = [0u8; 16];
        cmd[0] = 0x12; // INQUIRY
        cmd[4] = 36; // allocation length

        let data_buf = dma()
            .allocate_sized(36)
            .map_err(|_| XhciError::InvalidDevice)?;
        let data_phys = data_buf.phys_addr().as_u64();

        let transferred = self.bot_transfer(
            controller,
            in_ring,
            out_ring,
            &cmd,
            6,
            Some(data_phys),
            36,
            true,
        )?;

        // Only the bytes the device actually sent are ours; the rest of a
        // pooled buffer still holds whatever its previous owner left there.
        let mut result = [0u8; 36];
        let valid = transferred.min(36);
        unsafe {
            core::ptr::copy_nonoverlapping(data_buf.as_ptr(), result.as_mut_ptr(), valid);
        }
        if let Err(e) = dma().dealloc(data_buf) {
            println!("usb-msc: dma dealloc failed: {e}");
        }
        if valid < Self::MIN_INQUIRY_BYTES {
            println!("usb-msc: short INQUIRY response: {valid} bytes");
            return Err(XhciError::InvalidDevice);
        }
        Ok(result)
    }

    /// SCSI READ CAPACITY(10) (opcode 0x25).
    ///
    /// Returns `(last_lba, block_size)` where the total block count is `last_lba + 1`.
    pub fn read_capacity(
        &mut self,
        controller: &mut XhciController,
        in_ring: &mut TransferRing,
        out_ring: &mut TransferRing,
    ) -> Result<(u32, u32), XhciError> {
        let mut cmd = [0u8; 16];
        cmd[0] = 0x25; // READ CAPACITY(10)

        let data_buf = dma()
            .allocate_sized(8)
            .map_err(|_| XhciError::InvalidDevice)?;
        let data_phys = data_buf.phys_addr().as_u64();

        let transferred = self.bot_transfer(
            controller,
            in_ring,
            out_ring,
            &cmd,
            10,
            Some(data_phys),
            8,
            true,
        )?;

        // Both fields are big-endian 32-bit integers.
        let last_lba = unsafe { u32::from_be(core::ptr::read(data_buf.as_ptr() as *const u32)) };
        let block_size =
            unsafe { u32::from_be(core::ptr::read(data_buf.as_ptr().add(4) as *const u32)) };

        if let Err(e) = dma().dealloc(data_buf) {
            println!("usb-msc: dma dealloc failed: {e}");
        }

        // A short answer leaves part of the parameter data as whatever the
        // previous owner of the pooled buffer wrote, so neither field is ours.
        if transferred < 8 {
            println!("usb-msc: short READ CAPACITY response: {transferred} bytes");
            return Err(XhciError::InvalidDevice);
        }
        // `block_size` divides MAX_TRANSFER_BYTES in read_sectors/write_sectors:
        // zero faults the CPU and anything past the cap makes the chunk size
        // round to zero, so neither can be allowed to reach that arithmetic.
        if block_size == 0 || block_size > Self::MAX_TRANSFER_BYTES {
            println!("usb-msc: implausible block size {block_size}");
            return Err(XhciError::InvalidDevice);
        }
        Ok((last_lba, block_size))
    }

    /// SCSI TEST UNIT READY (opcode 0x00).
    ///
    /// Returns `true` if the device reports ready (command passed), `false` otherwise.
    pub fn test_unit_ready(
        &mut self,
        controller: &mut XhciController,
        in_ring: &mut TransferRing,
        out_ring: &mut TransferRing,
    ) -> Result<bool, XhciError> {
        let cmd = [0u8; 16]; // opcode 0x00
        match self.bot_transfer(controller, in_ring, out_ring, &cmd, 6, None, 0, false) {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// Max bytes per single BOT data phase (xHCI Normal TRB Transfer Length is 17 bits,
    /// but we cap conservatively at 64KB to stay within one TRB).
    const MAX_TRANSFER_BYTES: u32 = 65536;

    /// SCSI READ(10) (opcode 0x28) - read `count` sectors starting at `lba`.
    ///
    /// The caller must provide a DMA-safe physical address `buf_phys` pointing to a
    /// buffer of at least `count * block_size` bytes.
    /// Large reads are automatically split into multiple SCSI commands.
    pub fn read_sectors(
        &mut self,
        controller: &mut XhciController,
        in_ring: &mut TransferRing,
        out_ring: &mut TransferRing,
        lba: u32,
        count: u16,
        buf_phys: u64,
    ) -> Result<(), XhciError> {
        let max_sectors = (Self::MAX_TRANSFER_BYTES / self.block_size) as u16;
        let mut remaining = count;
        let mut cur_lba = lba;
        let mut cur_phys = buf_phys;

        while remaining > 0 {
            let chunk = remaining.min(max_sectors);

            let mut cmd = [0u8; 16];
            cmd[0] = 0x28; // READ(10)
            cmd[2] = (cur_lba >> 24) as u8;
            cmd[3] = (cur_lba >> 16) as u8;
            cmd[4] = (cur_lba >> 8) as u8;
            cmd[5] = cur_lba as u8;
            cmd[7] = (chunk >> 8) as u8;
            cmd[8] = chunk as u8;

            let transfer_len = chunk as u32 * self.block_size;
            let got = self.bot_transfer(
                controller,
                in_ring,
                out_ring,
                &cmd,
                10,
                Some(cur_phys),
                transfer_len,
                true,
            )?;
            // A short read leaves the tail of the caller's buffer holding
            // whatever was there before; reporting success would pass it off
            // as sector data.
            if got < transfer_len as usize {
                println!("usb-msc: short READ(10): {got} of {transfer_len} bytes");
                return Err(XhciError::InvalidDevice);
            }

            remaining -= chunk;
            cur_lba += chunk as u32;
            cur_phys += transfer_len as u64;
        }
        Ok(())
    }

    /// SCSI WRITE(10) (opcode 0x2A) - write `count` sectors starting at `lba`.
    ///
    /// The caller must provide a DMA-safe physical address `buf_phys` containing the
    /// data to write (`count * block_size` bytes).
    /// Large writes are automatically split into multiple SCSI commands.
    pub fn write_sectors(
        &mut self,
        controller: &mut XhciController,
        in_ring: &mut TransferRing,
        out_ring: &mut TransferRing,
        lba: u32,
        count: u16,
        buf_phys: u64,
    ) -> Result<(), XhciError> {
        let max_sectors = (Self::MAX_TRANSFER_BYTES / self.block_size) as u16;
        let mut remaining = count;
        let mut cur_lba = lba;
        let mut cur_phys = buf_phys;

        while remaining > 0 {
            let chunk = remaining.min(max_sectors);

            let mut cmd = [0u8; 16];
            cmd[0] = 0x2A; // WRITE(10)
            cmd[2] = (cur_lba >> 24) as u8;
            cmd[3] = (cur_lba >> 16) as u8;
            cmd[4] = (cur_lba >> 8) as u8;
            cmd[5] = cur_lba as u8;
            cmd[7] = (chunk >> 8) as u8;
            cmd[8] = chunk as u8;

            let transfer_len = chunk as u32 * self.block_size;
            let sent = self.bot_transfer(
                controller,
                in_ring,
                out_ring,
                &cmd,
                10,
                Some(cur_phys),
                transfer_len,
                false,
            )?;
            if sent < transfer_len as usize {
                println!("usb-msc: short WRITE(10): {sent} of {transfer_len} bytes");
                return Err(XhciError::InvalidDevice);
            }

            remaining -= chunk;
            cur_lba += chunk as u32;
            cur_phys += transfer_len as u64;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // BOT core
    // -----------------------------------------------------------------------

    /// Execute one Bulk-Only Transport transaction: CBW -> [data phase] -> CSW.
    ///
    /// - `scsi_cmd`: 16-byte CDB (unused bytes must be zero).
    /// - `cmd_len`: number of significant bytes in `scsi_cmd` (e.g. 6, 10, 12, 16).
    /// - `data_phys`: physical address of the data buffer (or `None` for no data phase).
    /// - `data_len`: byte count for the data phase.
    /// - `direction_in`: `true` = device-to-host (read), `false` = host-to-device (write).
    ///
    /// Returns the number of bytes the data phase actually moved. A caller that
    /// parses the buffer must bound itself by that count: the buffer comes from
    /// a pool that does not zero on reuse, so bytes past it belong to whoever
    /// held it last.
    fn bot_transfer(
        &mut self,
        controller: &mut XhciController,
        in_ring: &mut TransferRing,
        out_ring: &mut TransferRing,
        scsi_cmd: &[u8; 16],
        cmd_len: u8,
        data_phys: Option<u64>,
        data_len: u32,
        direction_in: bool,
    ) -> Result<usize, XhciError> {
        let tag = self.next_tag();

        // 1. Build and send CBW on bulk OUT using the pre-allocated buffer.
        let cbw_phys = self.cbw_buf.phys_addr().as_u64();
        unsafe {
            let cbw = self.cbw_buf.as_ptr() as *mut Cbw;
            core::ptr::write_bytes(cbw as *mut u8, 0, 31);
            (*cbw).d_cbw_signature = CBW_SIGNATURE;
            (*cbw).d_cbw_tag = tag;
            (*cbw).d_cbw_data_transfer_length = data_len;
            (*cbw).bm_cbw_flags = if direction_in { 0x80 } else { 0x00 };
            (*cbw).b_cbw_lun = 0;
            (*cbw).b_cbw_cb_length = cmd_len;
            core::ptr::copy_nonoverlapping(scsi_cmd.as_ptr(), (*cbw).cbwcb.as_mut_ptr(), 16);
        }

        controller.bulk_transfer(self.slot_id, out_ring, self.ep_out_dci, cbw_phys, 31, false)?;

        // 2. Data phase (optional).
        let mut transferred = 0usize;
        if data_len > 0 {
            if let Some(phys) = data_phys {
                transferred = if direction_in {
                    controller.bulk_transfer(
                        self.slot_id,
                        in_ring,
                        self.ep_in_dci,
                        phys,
                        data_len,
                        true,
                    )?
                } else {
                    controller.bulk_transfer(
                        self.slot_id,
                        out_ring,
                        self.ep_out_dci,
                        phys,
                        data_len,
                        false,
                    )?
                };
            }
        }

        // 3. Read CSW on bulk IN using the pre-allocated buffer.
        let csw_phys = self.csw_buf.phys_addr().as_u64();
        unsafe {
            core::ptr::write_bytes(self.csw_buf.as_ptr() as *mut u8, 0, 13);
        }
        controller.bulk_transfer(self.slot_id, in_ring, self.ep_in_dci, csw_phys, 13, true)?;

        // Validate the CSW.
        let csw = unsafe { core::ptr::read(self.csw_buf.as_ptr() as *const Csw) };
        // Read packed fields into locals before use.
        let sig = csw.d_csw_signature;
        let csw_tag = csw.d_csw_tag;
        let residue = csw.d_csw_data_residue;
        let status = csw.b_csw_status;

        if sig != CSW_SIGNATURE {
            println!("usb-msc: invalid CSW signature {:#010x}", sig);
            return Err(XhciError::InvalidDevice);
        }
        if csw_tag != tag {
            println!(
                "usb-msc: CSW tag mismatch: got {} expected {}",
                csw_tag, tag
            );
            return Err(XhciError::InvalidDevice);
        }
        if status != 0 {
            println!("usb-msc: SCSI command failed, CSW status={}", status);
            return Err(XhciError::TransferError(status));
        }

        // Two independent counts cover the data phase: the transfer event's
        // residual, reported by the host controller, and the CSW residue,
        // reported by the device. Trust whichever claims less arrived.
        Ok(transferred.min(data_len.saturating_sub(residue) as usize))
    }
}

// ---------------------------------------------------------------------------
// DevFS stub device
// ---------------------------------------------------------------------------

/// A devfs device node that represents a detected USB mass storage device.
///
/// Read and write operations are not yet wired to the xHCI driver thread
/// because BOT transfers require mutable access to the controller and transfer
/// rings owned by that thread. The node's presence at `/dev/usbN` confirms
/// the device was detected and identified successfully.
pub struct UsbStorageDevFsNode {
    pub slot_id: u8,
    pub block_size: u32,
    pub block_count: u64,
    pub vendor: [u8; 8],
    pub product: [u8; 16],
}

impl UsbStorageDevFsNode {
    pub fn new(slot_id: u8, block_size: u32, block_count: u64, inquiry_data: &[u8; 36]) -> Self {
        let mut vendor = [0u8; 8];
        let mut product = [0u8; 16];
        // INQUIRY response: bytes 8-15 = vendor, bytes 16-31 = product
        if inquiry_data.len() >= 16 {
            vendor.copy_from_slice(&inquiry_data[8..16]);
        }
        if inquiry_data.len() >= 32 {
            product.copy_from_slice(&inquiry_data[16..32]);
        }
        Self {
            slot_id,
            block_size,
            block_count,
            vendor,
            product,
        }
    }
}

impl DevFsDevice for UsbStorageDevFsNode {
    fn size(&self) -> u64 {
        self.block_count * self.block_size as u64
    }

    fn read(&self, _offset: usize, _count: usize) -> Result<alloc::vec::Vec<u8>, DevFsError> {
        Err(DevFsError::Unsupported)
    }

    fn write(&self, _offset: usize, _data: &[u8]) -> Result<usize, DevFsError> {
        Err(DevFsError::Unsupported)
    }
}

// ---------------------------------------------------------------------------
// Registration helper
// ---------------------------------------------------------------------------

/// Register a detected USB storage device in devfs as `/dev/usbN`.
///
/// `index` distinguishes multiple storage devices (0 for the first one found).
pub fn register_usb_storage(
    index: usize,
    slot_id: u8,
    block_size: u32,
    block_count: u64,
    inquiry_data: &[u8; 36],
) {
    let node = Arc::new(UsbStorageDevFsNode::new(
        slot_id,
        block_size,
        block_count,
        inquiry_data,
    ));

    let path = alloc::format!("/usb{}", index);
    match devfs::register_device_str(&path, node) {
        Ok(()) => println!("usb-msc: registered /dev{}", path),
        Err(e) => println!("usb-msc: failed to register /dev{}: {:?}", path, e),
    }
}
