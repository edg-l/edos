use crate::drivers::dma::{DmaBuffer, dma};
use crate::drivers::virtio::pci::VirtioTransport;
use crate::drivers::virtio::queue::{VIRTQ_DESC_F_WRITE, Virtqueue};
use crate::println;

// -------- Virtio GPU command types --------

const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB: u32 = 0x0108;
const VIRTIO_GPU_CMD_SET_SCANOUT_BLOB: u32 = 0x0109;
const VIRTIO_GPU_CMD_GET_EDID: u32 = 0x010a;

// Cursor commands
const VIRTIO_GPU_CMD_UPDATE_CURSOR: u32 = 0x0300;
const VIRTIO_GPU_CMD_MOVE_CURSOR: u32 = 0x0301;

// -------- Virtio GPU response types --------

const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const VIRTIO_GPU_RESP_OK_EDID: u32 = 0x1104;

// -------- Pixel format --------

// Blob resource constants
const VIRTIO_GPU_BLOB_MEM_GUEST: u32 = 0x0001;
const VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE: u32 = 0x0002;

const VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM: u32 = 1;
const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;

// -------- GPU command/response structures --------

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuCtrlHdr {
    type_: u32,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    ring_idx: u8,
    padding: [u8; 3],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtioGpuRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuDisplayOne {
    rect: VirtioGpuRect,
    enabled: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuRespDisplayInfo {
    hdr: VirtioGpuCtrlHdr,
    pmodes: [VirtioGpuDisplayOne; 16],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuResourceCreate2d {
    hdr: VirtioGpuCtrlHdr,
    resource_id: u32,
    format: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuResourceAttachBacking {
    hdr: VirtioGpuCtrlHdr,
    resource_id: u32,
    nr_entries: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuMemEntry {
    addr: u64,
    length: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuSetScanout {
    hdr: VirtioGpuCtrlHdr,
    rect: VirtioGpuRect,
    scanout_id: u32,
    resource_id: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuTransferToHost2d {
    hdr: VirtioGpuCtrlHdr,
    rect: VirtioGpuRect,
    offset: u64,
    resource_id: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuResourceFlush {
    hdr: VirtioGpuCtrlHdr,
    rect: VirtioGpuRect,
    resource_id: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuResourceCreateBlob {
    hdr: VirtioGpuCtrlHdr,
    resource_id: u32,
    blob_mem: u32,
    blob_flags: u32,
    nr_entries: u32,
    blob_id: u64,
    size: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuSetScanoutBlob {
    hdr: VirtioGpuCtrlHdr,
    rect: VirtioGpuRect,
    scanout_id: u32,
    resource_id: u32,
    width: u32,
    height: u32,
    format: u32,
    padding: u32,
    strides: [u32; 4],
    offsets: [u32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuCmdGetEdid {
    hdr: VirtioGpuCtrlHdr,
    scanout: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuRespEdid {
    hdr: VirtioGpuCtrlHdr,
    size: u32,
    padding: u32,
    edid: [u8; 1024],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuCursorPos {
    scanout_id: u32,
    x: u32,
    y: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuUpdateCursor {
    hdr: VirtioGpuCtrlHdr,
    pos: VirtioGpuCursorPos,
    resource_id: u32,
    hot_x: u32,
    hot_y: u32,
    padding: u32,
}

// -------- VirtioGpu driver --------

pub struct VirtioGpu {
    transport: VirtioTransport,
    control_queue: Virtqueue,
    cursor_queue: Virtqueue,
    /// Scratch DMA buffer for command/response pairs (4096 bytes).
    scratch: DmaBuffer,
    /// Separate DMA buffer for cursor commands (no response needed, fire-and-forget).
    cursor_scratch: DmaBuffer,
    /// True if RESOURCE_BLOB is available (zero-copy display path).
    use_blob: bool,
}

impl VirtioGpu {
    /// Initialise a virtio-gpu device from an already-reset transport.
    ///
    /// The caller must have called `transport.init_device()` before passing it
    /// here.  This function negotiates features, sets up the control queue, and
    /// signals DRIVER_OK.
    pub fn new(transport: VirtioTransport) -> Result<Self, &'static str> {
        // Must acknowledge VIRTIO_F_VERSION_1 (bit 32) for modern devices.
        // Without it QEMU treats the driver as legacy and may not resolve
        // DMA addresses correctly in RESOURCE_ATTACH_BACKING.
        const VIRTIO_GPU_F_EDID: u64 = 1 << 1;
        const VIRTIO_GPU_F_RESOURCE_BLOB: u64 = 1 << 28;
        const VIRTIO_F_VERSION_1: u64 = 1 << 32;

        let device_features = transport.read_device_features();
        let driver_features =
            device_features & (VIRTIO_F_VERSION_1 | VIRTIO_GPU_F_EDID | VIRTIO_GPU_F_RESOURCE_BLOB);
        let use_blob = device_features & VIRTIO_GPU_F_RESOURCE_BLOB != 0;
        println!(
            "virtio-gpu: device features={:#x}, negotiating {:#x}",
            device_features, driver_features
        );
        transport.write_driver_features(driver_features);
        transport.finish_init();

        // Set up control queue (index 0).
        transport.select_queue(0);
        let max_size = transport.queue_size();
        let size = max_size.min(128);
        transport.set_queue_size(size);

        let mut control_queue =
            Virtqueue::new(size).map_err(|_| "virtio-gpu: failed to allocate control queue")?;

        transport.set_queue_desc(control_queue.desc_phys_addr());
        transport.set_queue_avail(control_queue.avail_phys_addr());
        transport.set_queue_used(control_queue.used_phys_addr());

        control_queue.notify_off = transport.queue_notify_off();
        transport.enable_queue();

        // Set up cursor queue (index 1).
        transport.select_queue(1);
        let cursor_max = transport.queue_size();
        let cursor_size = cursor_max.min(16);
        transport.set_queue_size(cursor_size);

        let mut cursor_queue = Virtqueue::new(cursor_size)
            .map_err(|_| "virtio-gpu: failed to allocate cursor queue")?;

        transport.set_queue_desc(cursor_queue.desc_phys_addr());
        transport.set_queue_avail(cursor_queue.avail_phys_addr());
        transport.set_queue_used(cursor_queue.used_phys_addr());

        cursor_queue.notify_off = transport.queue_notify_off();
        transport.enable_queue();

        transport.set_driver_ok();

        let scratch = dma()
            .allocate_sized(4096)
            .map_err(|_| "virtio-gpu: failed to allocate scratch buffer")?;

        let cursor_scratch = dma()
            .allocate_sized(4096)
            .map_err(|_| "virtio-gpu: failed to allocate cursor scratch buffer")?;

        println!(
            "virtio-gpu: initialised, control={}, cursor={}, blob={}",
            size, cursor_size, use_blob
        );

        Ok(Self {
            transport,
            control_queue,
            cursor_queue,
            scratch,
            cursor_scratch,
            use_blob,
        })
    }

    // ---- Low-level command execution ----

    /// Execute a command that is already written into the scratch buffer.
    ///
    /// The command occupies bytes `0..cmd_size` and the response area starts at
    /// `resp_offset` (must be >= cmd_size, typically 8-byte-aligned).
    fn execute_scratch(
        &mut self,
        cmd_size: usize,
        resp_offset: usize,
        resp_size: usize,
    ) -> VirtioGpuCtrlHdr {
        let scratch_phys = self.scratch.phys_addr().as_u64();

        // Zero the response area before submission.
        unsafe {
            core::ptr::write_bytes(self.scratch.as_ptr().add(resp_offset), 0, resp_size);
        }

        let bufs = [
            (scratch_phys, cmd_size as u32, 0u16),
            (
                scratch_phys + resp_offset as u64,
                resp_size as u32,
                VIRTQ_DESC_F_WRITE,
            ),
        ];

        self.control_queue
            .push(&bufs)
            .expect("virtio-gpu: control queue full");

        let notify_off = self.control_queue.notify_off;
        self.transport.notify_queue(0, notify_off);

        for _ in 0..10_000_000u32 {
            if let Some((head, _len)) = self.control_queue.poll_used() {
                self.control_queue.reclaim(head);
                let resp = unsafe {
                    core::ptr::read_volatile(
                        self.scratch.as_ptr().add(resp_offset) as *const VirtioGpuCtrlHdr
                    )
                };
                return resp;
            }
            core::hint::spin_loop();
        }

        panic!("virtio-gpu: command timeout");
    }

    /// Copy `cmd` into the scratch buffer and execute, returning the response header.
    fn send_command<Cmd: Sized>(&mut self, cmd: &Cmd, resp_size: usize) -> VirtioGpuCtrlHdr {
        let cmd_size = core::mem::size_of::<Cmd>();
        unsafe {
            core::ptr::copy_nonoverlapping(
                cmd as *const Cmd as *const u8,
                self.scratch.as_ptr(),
                cmd_size,
            );
        }
        // Align response offset to 8 bytes.
        let resp_offset = (cmd_size + 7) & !7;
        self.execute_scratch(cmd_size, resp_offset, resp_size)
    }

    // ---- Public GPU commands ----

    /// Query the display resolution from the host.
    ///
    /// Returns `(width, height)` of the first enabled scanout, or a 1280x800
    /// fallback if none is reported.
    pub fn get_display_info(&mut self) -> (u32, u32) {
        let mut hdr: VirtioGpuCtrlHdr = unsafe { core::mem::zeroed() };
        hdr.type_ = VIRTIO_GPU_CMD_GET_DISPLAY_INFO;

        let cmd_size = core::mem::size_of::<VirtioGpuCtrlHdr>();
        let resp_size = core::mem::size_of::<VirtioGpuRespDisplayInfo>();

        // Write the command header to scratch.
        unsafe {
            core::ptr::copy_nonoverlapping(
                &hdr as *const VirtioGpuCtrlHdr as *const u8,
                self.scratch.as_ptr(),
                cmd_size,
            );
        }

        let resp_offset = (cmd_size + 7) & !7;
        self.execute_scratch(cmd_size, resp_offset, resp_size);

        // Read the full response (not just the header) from scratch.
        let resp = unsafe {
            core::ptr::read_volatile(
                self.scratch.as_ptr().add(resp_offset) as *const VirtioGpuRespDisplayInfo
            )
        };

        if resp.hdr.type_ != VIRTIO_GPU_RESP_OK_DISPLAY_INFO {
            println!(
                "virtio-gpu: GET_DISPLAY_INFO failed: type={:#x}",
                resp.hdr.type_
            );
            return (1280, 800);
        }

        for pmode in &resp.pmodes {
            if pmode.enabled != 0 {
                return (pmode.rect.width, pmode.rect.height);
            }
        }

        println!("virtio-gpu: no enabled display, using 1280x800 fallback");
        (1280, 800)
    }

    /// Query the EDID data and extract the display refresh rate in Hz.
    /// Returns None if EDID is not supported or the data can't be parsed.
    pub fn get_refresh_rate(&mut self) -> Option<u32> {
        let mut cmd: VirtioGpuCmdGetEdid = unsafe { core::mem::zeroed() };
        cmd.hdr.type_ = VIRTIO_GPU_CMD_GET_EDID;
        cmd.scanout = 0;

        let cmd_size = core::mem::size_of::<VirtioGpuCmdGetEdid>();
        let resp_size = core::mem::size_of::<VirtioGpuRespEdid>();

        unsafe {
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                self.scratch.as_ptr(),
                cmd_size,
            );
        }

        let resp_offset = (cmd_size + 7) & !7;
        self.execute_scratch(cmd_size, resp_offset, resp_size);

        let resp = unsafe {
            core::ptr::read_volatile(
                self.scratch.as_ptr().add(resp_offset) as *const VirtioGpuRespEdid
            )
        };

        if resp.hdr.type_ != VIRTIO_GPU_RESP_OK_EDID || resp.size < 128 {
            return None;
        }

        // Parse first Detailed Timing Descriptor at EDID byte 54.
        // Pixel clock in 10 kHz units (bytes 54-55, little-endian).
        let edid = &resp.edid;
        let pixel_clock_10khz = (edid[55] as u32) << 8 | edid[54] as u32;
        if pixel_clock_10khz == 0 {
            return None;
        }

        // Horizontal: active + blanking
        let h_active = ((edid[58] as u32 & 0xF0) << 4) | edid[56] as u32;
        let h_blanking = ((edid[58] as u32 & 0x0F) << 8) | edid[57] as u32;
        let h_total = h_active + h_blanking;

        // Vertical: active + blanking
        let v_active = ((edid[61] as u32 & 0xF0) << 4) | edid[59] as u32;
        let v_blanking = ((edid[61] as u32 & 0x0F) << 8) | edid[60] as u32;
        let v_total = v_active + v_blanking;

        if h_total == 0 || v_total == 0 {
            return None;
        }

        // refresh = pixel_clock / (h_total * v_total)
        // pixel_clock is in 10 kHz = 10_000 Hz units
        let refresh = (pixel_clock_10khz as u64 * 10_000) / (h_total as u64 * v_total as u64);
        Some(refresh as u32)
    }

    /// Send RESOURCE_CREATE_2D + RESOURCE_ATTACH_BACKING for a single resource.
    ///
    /// `backing_phys` and `backing_len` describe the physical memory region
    /// that backs this resource's pixels.
    fn create_resource(
        &mut self,
        resource_id: u32,
        width: u32,
        height: u32,
        backing_phys: u64,
        backing_len: u32,
    ) {
        self.create_resource_with_format(
            resource_id,
            VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM,
            width,
            height,
            backing_phys,
            backing_len,
        );
    }

    fn create_resource_with_format(
        &mut self,
        resource_id: u32,
        format: u32,
        width: u32,
        height: u32,
        backing_phys: u64,
        backing_len: u32,
    ) {
        // RESOURCE_CREATE_2D
        let mut create: VirtioGpuResourceCreate2d = unsafe { core::mem::zeroed() };
        create.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_CREATE_2D;
        create.resource_id = resource_id;
        create.format = format;
        create.width = width;
        create.height = height;

        let resp = self.send_command(&create, core::mem::size_of::<VirtioGpuCtrlHdr>());
        if resp.type_ != VIRTIO_GPU_RESP_OK_NODATA {
            println!(
                "virtio-gpu: RESOURCE_CREATE_2D resource {} failed: {:#x}",
                resource_id, resp.type_
            );
        } else {
            println!(
                "virtio-gpu: resource {} created ({}x{})",
                resource_id, width, height
            );
        }

        // RESOURCE_ATTACH_BACKING: command header + mem entry packed together.
        let attach_size = core::mem::size_of::<VirtioGpuResourceAttachBacking>();
        let entry_size = core::mem::size_of::<VirtioGpuMemEntry>();

        let mut attach: VirtioGpuResourceAttachBacking = unsafe { core::mem::zeroed() };
        attach.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING;
        attach.resource_id = resource_id;
        attach.nr_entries = 1;

        let mut entry: VirtioGpuMemEntry = unsafe { core::mem::zeroed() };
        entry.addr = backing_phys;
        entry.length = backing_len;

        // Write both structs contiguously into scratch.
        unsafe {
            core::ptr::copy_nonoverlapping(
                &attach as *const _ as *const u8,
                self.scratch.as_ptr(),
                attach_size,
            );
            core::ptr::copy_nonoverlapping(
                &entry as *const _ as *const u8,
                self.scratch.as_ptr().add(attach_size),
                entry_size,
            );
        }

        let total_cmd_size = attach_size + entry_size;
        let resp_offset = (total_cmd_size + 7) & !7;
        let resp = self.execute_scratch(
            total_cmd_size,
            resp_offset,
            core::mem::size_of::<VirtioGpuCtrlHdr>(),
        );

        if resp.type_ != VIRTIO_GPU_RESP_OK_NODATA {
            println!(
                "virtio-gpu: RESOURCE_ATTACH_BACKING resource {} failed: {:#x}",
                resource_id, resp.type_
            );
        }
    }

    /// Set up a double-buffered framebuffer at the given dimensions.
    ///
    /// Allocates a DMA buffer and creates a single GPU resource backed by it.
    /// Sets the resource as the active scanout. No double-buffering at the
    /// virtio level -- TRANSFER_TO_HOST_2D copies a snapshot so tearing is
    /// not an issue.
    ///
    /// Returns the DMA buffer (caller keeps ownership).
    pub fn setup_framebuffer(&mut self, width: u32, height: u32) -> DmaBuffer {
        let buf_bytes = (width * height * 4) as usize;

        let dma_buf = DmaBuffer::allocate_sized(buf_bytes)
            .expect("virtio-gpu: failed to allocate framebuffer backing");

        let phys_base = dma_buf.phys_addr().as_u64();

        if self.use_blob && self.create_resource_blob(1, width, height, phys_base, buf_bytes as u32)
        {
            self.set_scanout_blob(0, 1, width, height);
        } else {
            self.use_blob = false;
            self.create_resource(1, width, height, phys_base, buf_bytes as u32);
            self.set_scanout(0, 1, width, height);
        }

        // Initial transfer + flush so the display shows something immediately.
        self.transfer_and_flush(
            1,
            VirtioGpuRect {
                x: 0,
                y: 0,
                width,
                height,
            },
            width * 4,
        );

        println!("virtio-gpu: framebuffer ready ({}x{})", width, height);

        dma_buf
    }

    /// Transfer pixel data from the backing buffer to the host and flush it to
    /// the physical display.
    ///
    /// With blob resources: only RESOURCE_FLUSH (QEMU reads DMA directly).
    /// Without blob: batched TRANSFER_TO_HOST_2D + RESOURCE_FLUSH.
    pub fn transfer_and_flush(&mut self, resource_id: u32, rect: VirtioGpuRect, stride: u32) {
        if self.use_blob {
            // Zero-copy path: just flush, QEMU reads directly from guest memory.
            let mut flush: VirtioGpuResourceFlush = unsafe { core::mem::zeroed() };
            flush.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_FLUSH;
            flush.rect = rect;
            flush.resource_id = resource_id;
            self.send_command(&flush, core::mem::size_of::<VirtioGpuCtrlHdr>());
            return;
        }

        let hdr_size = core::mem::size_of::<VirtioGpuCtrlHdr>();
        let xfer_size = core::mem::size_of::<VirtioGpuTransferToHost2d>();
        let flush_size = core::mem::size_of::<VirtioGpuResourceFlush>();

        // Layout in scratch buffer:
        //   [xfer cmd | xfer resp | flush cmd | flush resp]
        let xfer_resp_off = (xfer_size + 7) & !7;
        let flush_cmd_off = (xfer_resp_off + hdr_size + 7) & !7;
        let flush_resp_off = (flush_cmd_off + flush_size + 7) & !7;

        // Write TRANSFER_TO_HOST_2D command.
        // offset = byte position in the backing buffer matching rect.y/rect.x,
        // so QEMU reads the correct source pixels for the given rect.
        let mut xfer: VirtioGpuTransferToHost2d = unsafe { core::mem::zeroed() };
        xfer.hdr.type_ = VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D;
        xfer.rect = rect;
        xfer.offset = (rect.y as u64) * (stride as u64) + (rect.x as u64) * 4;
        xfer.resource_id = resource_id;
        unsafe {
            core::ptr::copy_nonoverlapping(
                &xfer as *const _ as *const u8,
                self.scratch.as_ptr(),
                xfer_size,
            );
            // Zero response area
            core::ptr::write_bytes(self.scratch.as_ptr().add(xfer_resp_off), 0, hdr_size);
        }

        // Write RESOURCE_FLUSH command
        let mut flush: VirtioGpuResourceFlush = unsafe { core::mem::zeroed() };
        flush.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_FLUSH;
        flush.rect = rect;
        flush.resource_id = resource_id;
        unsafe {
            core::ptr::copy_nonoverlapping(
                &flush as *const _ as *const u8,
                self.scratch.as_ptr().add(flush_cmd_off),
                flush_size,
            );
            core::ptr::write_bytes(self.scratch.as_ptr().add(flush_resp_off), 0, hdr_size);
        }

        let scratch_phys = self.scratch.phys_addr().as_u64();

        // Push both descriptor chains before notifying
        let xfer_bufs = [
            (scratch_phys, xfer_size as u32, 0u16),
            (
                scratch_phys + xfer_resp_off as u64,
                hdr_size as u32,
                VIRTQ_DESC_F_WRITE,
            ),
        ];
        let flush_bufs = [
            (scratch_phys + flush_cmd_off as u64, flush_size as u32, 0u16),
            (
                scratch_phys + flush_resp_off as u64,
                hdr_size as u32,
                VIRTQ_DESC_F_WRITE,
            ),
        ];

        self.control_queue
            .push(&xfer_bufs)
            .expect("virtio-gpu: queue full");
        self.control_queue
            .push(&flush_bufs)
            .expect("virtio-gpu: queue full");

        // Single notify for both commands
        let notify_off = self.control_queue.notify_off;
        self.transport.notify_queue(0, notify_off);

        // Poll for both completions
        let mut completed = 0u32;
        for _ in 0..10_000_000u32 {
            if let Some((head, _)) = self.control_queue.poll_used() {
                self.control_queue.reclaim(head);
                completed += 1;
                if completed == 2 {
                    return;
                }
            }
            core::hint::spin_loop();
        }
        panic!("virtio-gpu: batched command timeout");
    }

    /// Switch scanout to a different resource.
    pub fn set_scanout(&mut self, scanout_id: u32, resource_id: u32, width: u32, height: u32) {
        let mut cmd: VirtioGpuSetScanout = unsafe { core::mem::zeroed() };
        cmd.hdr.type_ = VIRTIO_GPU_CMD_SET_SCANOUT;
        cmd.rect = VirtioGpuRect {
            x: 0,
            y: 0,
            width,
            height,
        };
        cmd.scanout_id = scanout_id;
        cmd.resource_id = resource_id;
        self.send_command(&cmd, core::mem::size_of::<VirtioGpuCtrlHdr>());
    }

    // ---- Blob resource (zero-copy) path ----

    fn create_resource_blob(
        &mut self,
        resource_id: u32,
        width: u32,
        height: u32,
        backing_phys: u64,
        backing_len: u32,
    ) -> bool {
        let cmd_size = core::mem::size_of::<VirtioGpuResourceCreateBlob>();
        let entry_size = core::mem::size_of::<VirtioGpuMemEntry>();

        let mut cmd: VirtioGpuResourceCreateBlob = unsafe { core::mem::zeroed() };
        cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB;
        cmd.resource_id = resource_id;
        cmd.blob_mem = VIRTIO_GPU_BLOB_MEM_GUEST;
        cmd.blob_flags = VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE;
        cmd.nr_entries = 1;
        cmd.blob_id = 0;
        cmd.size = backing_len as u64;

        let mut entry: VirtioGpuMemEntry = unsafe { core::mem::zeroed() };
        entry.addr = backing_phys;
        entry.length = backing_len;

        // Pack cmd + entry contiguously in scratch (same pattern as create_resource)
        unsafe {
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                self.scratch.as_ptr(),
                cmd_size,
            );
            core::ptr::copy_nonoverlapping(
                &entry as *const _ as *const u8,
                self.scratch.as_ptr().add(cmd_size),
                entry_size,
            );
        }

        let total_cmd_size = cmd_size + entry_size;
        let resp_offset = (total_cmd_size + 7) & !7;

        let resp = self.execute_scratch(
            total_cmd_size,
            resp_offset,
            core::mem::size_of::<VirtioGpuCtrlHdr>(),
        );

        if resp.type_ != VIRTIO_GPU_RESP_OK_NODATA {
            println!(
                "virtio-gpu: RESOURCE_CREATE_BLOB {} failed: {:#x}, falling back",
                resource_id, resp.type_
            );
            false
        } else {
            println!(
                "virtio-gpu: blob resource {} created ({}x{})",
                resource_id, width, height
            );
            true
        }
    }

    fn set_scanout_blob(&mut self, scanout_id: u32, resource_id: u32, width: u32, height: u32) {
        let mut cmd: VirtioGpuSetScanoutBlob = unsafe { core::mem::zeroed() };
        cmd.hdr.type_ = VIRTIO_GPU_CMD_SET_SCANOUT_BLOB;
        cmd.rect = VirtioGpuRect {
            x: 0,
            y: 0,
            width,
            height,
        };
        cmd.scanout_id = scanout_id;
        cmd.resource_id = resource_id;
        cmd.width = width;
        cmd.height = height;
        cmd.format = VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM;
        cmd.strides[0] = width * 4;
        self.send_command(&cmd, core::mem::size_of::<VirtioGpuCtrlHdr>());
    }

    // ---- Hardware cursor ----

    /// Set up a hardware cursor from a 64x64 RGBA pixel buffer.
    /// `pixels` must be exactly 64*64 = 4096 u32 values.
    /// The cursor image is uploaded via the control queue (requires fencing),
    /// then UPDATE_CURSOR is sent on the cursor queue.
    pub fn setup_cursor(&mut self, pixels: &[u32], hot_x: u32, hot_y: u32) {
        // Create cursor resource (id=100 to avoid collision with framebuffer)
        let cursor_res_id = 100u32;
        let cursor_hw_size = 64u32;
        let byte_len = (cursor_hw_size * cursor_hw_size * 4) as usize;

        // Allocate DMA buffer for cursor pixel data
        let cursor_buf = DmaBuffer::allocate_sized(byte_len)
            .expect("virtio-gpu: failed to allocate cursor buffer");

        // Copy pixel data into 64x64 buffer, adding alpha.
        // Source pixels: 0x00000000 = transparent, 0x00RRGGBB = opaque.
        unsafe {
            core::ptr::write_bytes(cursor_buf.as_ptr(), 0, byte_len);
            let dst = cursor_buf.as_ptr() as *mut u32;
            let src_w = if pixels.len() == 256 { 16 } else { 64 };
            let src_h = pixels.len() / src_w.max(1);
            for y in 0..src_h.min(64) {
                for x in 0..src_w.min(64) {
                    let px = pixels[y * src_w + x];
                    let argb = if px & 0x00FFFFFF != 0 {
                        px | 0xFF000000
                    } else {
                        0
                    };
                    *dst.add(y * 64 + x) = argb;
                }
            }
        }

        // Use B8G8R8A8 format (1) for cursor to support alpha transparency.
        self.create_resource_with_format(
            cursor_res_id,
            VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM,
            cursor_hw_size,
            cursor_hw_size,
            cursor_buf.phys_addr().as_u64(),
            byte_len as u32,
        );

        // Transfer cursor to host (must use control queue with FENCE for cursor)
        let mut xfer: VirtioGpuTransferToHost2d = unsafe { core::mem::zeroed() };
        xfer.hdr.type_ = VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D;
        xfer.hdr.flags = 1; // VIRTIO_GPU_FLAG_FENCE
        xfer.hdr.fence_id = 1;
        xfer.rect = VirtioGpuRect {
            x: 0,
            y: 0,
            width: cursor_hw_size,
            height: cursor_hw_size,
        };
        xfer.resource_id = cursor_res_id;
        self.send_command(&xfer, core::mem::size_of::<VirtioGpuCtrlHdr>());

        // Send UPDATE_CURSOR on cursor queue
        let mut cmd: VirtioGpuUpdateCursor = unsafe { core::mem::zeroed() };
        cmd.hdr.type_ = VIRTIO_GPU_CMD_UPDATE_CURSOR;
        cmd.pos = VirtioGpuCursorPos {
            scanout_id: 0,
            x: 0,
            y: 0,
            padding: 0,
        };
        cmd.resource_id = cursor_res_id;
        cmd.hot_x = hot_x;
        cmd.hot_y = hot_y;
        self.send_cursor_command(&cmd);

        // Keep the DMA buffer alive (cursor lives forever)
        core::mem::forget(cursor_buf);

        println!("virtio-gpu: hardware cursor set up");
    }

    /// Move the hardware cursor to (x, y). Fire-and-forget on the cursor queue.
    pub fn move_cursor(&mut self, x: u32, y: u32) {
        // Drain any completed cursor commands first
        while let Some((head, _)) = self.cursor_queue.poll_used() {
            self.cursor_queue.reclaim(head);
        }

        let mut cmd: VirtioGpuUpdateCursor = unsafe { core::mem::zeroed() };
        cmd.hdr.type_ = VIRTIO_GPU_CMD_MOVE_CURSOR;
        cmd.pos = VirtioGpuCursorPos {
            scanout_id: 0,
            x,
            y,
            padding: 0,
        };
        // resource_id must be non-zero or QEMU's GTK backend hides the cursor
        // (dpy_mouse_set uses it as the "on" flag).
        cmd.resource_id = 100; // must match the cursor resource id from setup_cursor
        self.send_cursor_command(&cmd);
    }

    /// Send a command on the cursor queue (fire-and-forget, no response).
    fn send_cursor_command(&mut self, cmd: &VirtioGpuUpdateCursor) {
        let cmd_size = core::mem::size_of::<VirtioGpuUpdateCursor>();
        unsafe {
            core::ptr::copy_nonoverlapping(
                cmd as *const _ as *const u8,
                self.cursor_scratch.as_ptr(),
                cmd_size,
            );
        }
        let phys = self.cursor_scratch.phys_addr().as_u64();
        // Cursor queue: single device-readable descriptor, no response descriptor.
        let bufs = [(phys, cmd_size as u32, 0u16)];
        if self.cursor_queue.push(&bufs).is_some() {
            let notify_off = self.cursor_queue.notify_off;
            self.transport.notify_queue(1, notify_off);
        }
    }

    /// Access the underlying transport (e.g. to read the PCI address).
    pub fn transport(&self) -> &VirtioTransport {
        &self.transport
    }
}
