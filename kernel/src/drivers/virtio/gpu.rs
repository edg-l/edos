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

// -------- Virtio GPU response types --------

const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;

// -------- Pixel format --------

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
struct VirtioGpuRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
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

// -------- VirtioGpu driver --------

pub struct VirtioGpu {
    transport: VirtioTransport,
    control_queue: Virtqueue,
    /// Scratch DMA buffer for command/response pairs (4096 bytes).
    scratch: DmaBuffer,
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
        const VIRTIO_F_VERSION_1: u64 = 1 << 32;

        let device_features = transport.read_device_features();
        let driver_features = device_features & VIRTIO_F_VERSION_1;
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

        transport.set_driver_ok();

        let scratch = dma()
            .allocate_sized(4096)
            .map_err(|_| "virtio-gpu: failed to allocate scratch buffer")?;

        println!("virtio-gpu: initialised, control queue size={}", size);

        Ok(Self {
            transport,
            control_queue,
            scratch,
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
        // RESOURCE_CREATE_2D
        let mut create: VirtioGpuResourceCreate2d = unsafe { core::mem::zeroed() };
        create.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_CREATE_2D;
        create.resource_id = resource_id;
        create.format = VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM;
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
    /// Allocates a single contiguous DMA buffer large enough for two full
    /// pages (so userspace can mmap the whole region).  Creates two GPU
    /// resources (IDs 1 and 2) backed by the first and second halves.
    /// Resource 1 is set as the initial scanout.
    ///
    /// Returns the DMA buffer (caller keeps ownership).
    pub fn setup_framebuffer(&mut self, width: u32, height: u32) -> DmaBuffer {
        let page_bytes = (width * height * 4) as usize;
        let total_bytes = page_bytes * 2;

        let dma_buf = DmaBuffer::allocate_sized(total_bytes)
            .expect("virtio-gpu: failed to allocate framebuffer backing");

        let phys_base = dma_buf.phys_addr().as_u64();

        // Resource 1 = first half, resource 2 = second half.
        self.create_resource(1, width, height, phys_base, page_bytes as u32);
        self.create_resource(
            2,
            width,
            height,
            phys_base + page_bytes as u64,
            page_bytes as u32,
        );

        // Attach resource 1 to scanout 0.
        let mut scanout: VirtioGpuSetScanout = unsafe { core::mem::zeroed() };
        scanout.hdr.type_ = VIRTIO_GPU_CMD_SET_SCANOUT;
        scanout.rect = VirtioGpuRect {
            x: 0,
            y: 0,
            width,
            height,
        };
        scanout.scanout_id = 0;
        scanout.resource_id = 1;

        let resp = self.send_command(&scanout, core::mem::size_of::<VirtioGpuCtrlHdr>());
        if resp.type_ != VIRTIO_GPU_RESP_OK_NODATA {
            println!("virtio-gpu: SET_SCANOUT failed: {:#x}", resp.type_);
        }

        // Initial transfer + flush so the display shows something immediately.
        self.transfer_and_flush(1, width, height);

        println!(
            "virtio-gpu: framebuffer ready ({}x{}, double-buffered)",
            width, height
        );

        dma_buf
    }

    /// Transfer pixel data from the backing buffer to the host and flush it to
    /// the physical display.
    pub fn transfer_and_flush(&mut self, resource_id: u32, width: u32, height: u32) {
        // TRANSFER_TO_HOST_2D
        let mut xfer: VirtioGpuTransferToHost2d = unsafe { core::mem::zeroed() };
        xfer.hdr.type_ = VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D;
        xfer.rect = VirtioGpuRect {
            x: 0,
            y: 0,
            width,
            height,
        };
        xfer.offset = 0;
        xfer.resource_id = resource_id;
        self.send_command(&xfer, core::mem::size_of::<VirtioGpuCtrlHdr>());

        // RESOURCE_FLUSH
        let mut flush: VirtioGpuResourceFlush = unsafe { core::mem::zeroed() };
        flush.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_FLUSH;
        flush.rect = VirtioGpuRect {
            x: 0,
            y: 0,
            width,
            height,
        };
        flush.resource_id = resource_id;
        self.send_command(&flush, core::mem::size_of::<VirtioGpuCtrlHdr>());
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

    /// Access the underlying transport (e.g. to read the PCI address).
    pub fn transport(&self) -> &VirtioTransport {
        &self.transport
    }
}
