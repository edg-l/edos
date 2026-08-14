extern crate alloc;
use alloc::vec::Vec;

pub mod framebuffer;

use spin::Once;

use crate::thread::preempt::PreemptSpinlock as Mutex;

use x86_64::{PhysAddr, VirtAddr, structures::paging::PageTableFlags};

use crate::{
    boot::boot_info,
    drivers::vga::{
        DISPI_INDEX_VIDEO_MEMORY_64K, DISPI_INDEX_VIRT_HEIGHT, DISPI_INDEX_Y_OFFSET, dispi_read,
        dispi_write,
    },
    graphics::framebuffer::FramebufferMmapInfo,
    memory::{mapper::memory_mapper, pat},
    println,
};

/// Global display, initialized once and accessed directly from ioctl handlers.
/// No render thread or Mailbox -- callers write to the framebuffer in their own
/// thread context, serialized by this Mutex.
pub static DISPLAY: Once<Mutex<Display>> = Once::new();

pub fn init() {
    DISPLAY.call_once(|| {
        // Try virtio-gpu first, fall back to VBE.
        let display = match try_init_virtio_gpu() {
            Some(d) => d,
            None => {
                println!("virtio-gpu: not found, using VBE framebuffer");
                Display::Vbe(DirectFramebuffer::new())
            }
        };

        framebuffer::FramebufferDevice::register();

        // Register display physical range as allowed for userspace MAP_PHYSICAL.
        let info = display.mmap_info();
        crate::memory::allow_physical_range(info.phys_addr, info.total_size);

        Mutex::new(display)
    });
}

fn try_init_virtio_gpu() -> Option<Display> {
    use crate::drivers::pci::pci_manager;
    use crate::drivers::virtio::gpu::VirtioGpu;
    use crate::drivers::virtio::pci::VirtioTransport;

    // Scan for virtio-gpu PCI device (vendor 0x1AF4, device 0x1050).
    let manager = pci_manager().read();
    let gpu_dev = manager
        .get_devices()
        .iter()
        .find(|d| d.header.vendor_id == 0x1AF4 && d.header.device_id == 0x1050)?;
    let gpu_dev = *gpu_dev; // copy before releasing the lock
    drop(manager);

    println!(
        "virtio-gpu: found PCI device at {:02x}:{:02x}.{}",
        gpu_dev.address.bus, gpu_dev.address.device, gpu_dev.address.function
    );

    let transport = VirtioTransport::new(&gpu_dev)?;
    transport.init_device();

    let mut gpu = match VirtioGpu::new(transport) {
        Ok(g) => g,
        Err(e) => {
            println!("virtio-gpu: init failed: {}", e);
            return None;
        }
    };

    // Use the Limine framebuffer dimensions (from UEFI GOP). With virtio-vga
    // the VGA compat mode provides GOP for Limine, so this is always available.
    // Falls back to 1920x1080 if no framebuffer (e.g. bare virtio-gpu-pci).
    let (width, height) = if let Some(fb) = &boot_info().framebuffer {
        (fb.width as u32, fb.height as u32)
    } else {
        (1920, 1080)
    };
    let refresh_rate = gpu.get_refresh_rate().unwrap_or(60);
    println!("virtio-gpu: {}x{} @ {}Hz", width, height, refresh_rate);

    let dma_buf = gpu.setup_framebuffer(width, height);

    Some(Display::VirtioGpu(VirtioGpuDisplay::new(
        gpu,
        dma_buf,
        width,
        height,
        refresh_rate,
    )))
}

// -------- Display enum --------

pub enum Display {
    Vbe(DirectFramebuffer),
    VirtioGpu(VirtioGpuDisplay),
}

impl Display {
    pub fn screen_info(&self) -> ScreenInfo {
        match self {
            Display::Vbe(fb) => fb.screen_info(),
            Display::VirtioGpu(vg) => vg.screen_info(),
        }
    }

    pub fn mmap_info(&self) -> FramebufferMmapInfo {
        match self {
            Display::Vbe(fb) => fb.mmap_info(),
            Display::VirtioGpu(vg) => vg.mmap_info(),
        }
    }

    pub fn draw(&mut self, src: &[u32], x: u64, y: u64, src_width: usize, src_height: usize) {
        match self {
            Display::Vbe(fb) => fb.draw(src, x, y, src_width, src_height),
            Display::VirtioGpu(vg) => vg.draw(src, x, y, src_width, src_height),
        }
    }

    pub fn draw_rect(&mut self, x: u64, y: u64, width: u64, height: u64, color: u32) {
        match self {
            Display::Vbe(fb) => fb.draw_rect(x, y, width, height, color),
            Display::VirtioGpu(vg) => vg.draw_rect(x, y, width, height, color),
        }
    }

    pub fn flip(&mut self) -> u64 {
        match self {
            Display::Vbe(fb) => fb.flip(),
            Display::VirtioGpu(vg) => vg.flip(),
        }
    }

    pub fn flip_rect(&mut self, x: u32, y: u32, w: u32, h: u32) -> u64 {
        match self {
            Display::Vbe(fb) => fb.flip(), // VBE always flips full screen
            Display::VirtioGpu(vg) => vg.flip_rect(x, y, w, h),
        }
    }

    /// Hand the display a cursor image, reporting whether it has a plane to
    /// hold one.
    ///
    /// A caller that is told nothing has to composite the cursor itself, so
    /// accepting the upload on a display without a cursor plane leaves the
    /// screen with no pointer at all.
    pub fn set_cursor(&mut self, pixels: &[u32], hot_x: u32, hot_y: u32) -> bool {
        match self {
            Display::Vbe(_) => false,
            Display::VirtioGpu(vg) => {
                vg.gpu.setup_cursor(pixels, hot_x, hot_y);
                true
            }
        }
    }

    pub fn move_cursor(&mut self, x: u32, y: u32) -> bool {
        match self {
            Display::Vbe(_) => false,
            Display::VirtioGpu(vg) => {
                vg.gpu.move_cursor(x, y);
                true
            }
        }
    }
}

// -------- VirtioGpuDisplay --------

pub struct VirtioGpuDisplay {
    gpu: crate::drivers::virtio::gpu::VirtioGpu,
    width: u32,
    height: u32,
    pitch: u32,
    /// Virtual pointer to the single DMA backing buffer.
    fb_base: *mut u32,
    /// Physical address of the DMA buffer.
    phys_addr: u64,
    /// Size of the buffer in bytes (width * height * 4).
    buf_size: u64,
    refresh_rate: u32,
    /// Keep ownership of the DMA buffer so it isn't dropped.
    _dma: crate::drivers::dma::DmaBuffer,
}

unsafe impl Send for VirtioGpuDisplay {}

impl VirtioGpuDisplay {
    fn new(
        gpu: crate::drivers::virtio::gpu::VirtioGpu,
        dma: crate::drivers::dma::DmaBuffer,
        width: u32,
        height: u32,
        refresh_rate: u32,
    ) -> Self {
        let pitch = width * 4;
        let buf_size = (height * pitch) as u64;
        let fb_base = dma.as_ptr() as *mut u32;
        let phys_addr = dma.phys_addr().as_u64();

        Self {
            gpu,
            width,
            height,
            pitch,
            fb_base,
            phys_addr,
            buf_size,
            refresh_rate,
            _dma: dma,
        }
    }

    pub fn screen_info(&self) -> ScreenInfo {
        ScreenInfo {
            width: self.width as usize,
            height: self.height as usize,
            refresh_rate: self.refresh_rate,
        }
    }

    pub fn mmap_info(&self) -> FramebufferMmapInfo {
        // Single buffer, no double buffering. Virtio-gpu copies via
        // TRANSFER_TO_HOST_2D so there's no tearing from direct VRAM access.
        FramebufferMmapInfo {
            phys_addr: self.phys_addr,
            total_size: self.buf_size,
            page_size: self.buf_size,
            width: self.width,
            height: self.height,
            pitch: self.pitch,
            double_buffered: 0,
            is_identity: 1, // B8G8R8X8 = 0x00RRGGBB on little-endian x86
            _padding: [0; 2],
        }
    }

    pub fn draw(&mut self, src: &[u32], x: u64, y: u64, src_width: usize, src_height: usize) {
        let fb = self.fb_base;
        let pixels_per_row = (self.pitch / 4) as usize;
        let w = self.width as usize;
        let h = self.height as usize;

        let start_x = (x as usize).min(w);
        let start_y = (y as usize).min(h);
        let end_x = (x as usize + src_width).min(w);
        let end_y = (y as usize + src_height).min(h);

        if start_x >= end_x || start_y >= end_y {
            return;
        }

        let src_offset_x = start_x - x as usize;
        let src_offset_y = start_y - y as usize;
        let row_len = end_x - start_x;

        let last_src_row = src_offset_y + (end_y - start_y) - 1;
        let last_src_index = last_src_row * src_width + src_offset_x + row_len;
        if last_src_index > src.len() {
            return;
        }

        let mut src_row = src_offset_y;
        for dst_y in start_y..end_y {
            let src_start = src_row * src_width + src_offset_x;
            let dst_start = dst_y * pixels_per_row + start_x;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src[src_start..].as_ptr(),
                    fb.add(dst_start),
                    row_len,
                );
            }
            src_row += 1;
        }
    }

    pub fn draw_rect(&mut self, x: u64, y: u64, width: u64, height: u64, color: u32) {
        if width == 0 || height == 0 {
            return;
        }

        let fb = self.fb_base;
        let pixels_per_row = (self.pitch / 4) as usize;
        let w = self.width as usize;
        let h = self.height as usize;

        let start_x = (x as usize).min(w);
        let start_y = (y as usize).min(h);
        let end_x = (x as usize + width as usize).min(w);
        let end_y = (y as usize + height as usize).min(h);

        if start_x >= end_x || start_y >= end_y {
            return;
        }

        let row_len = end_x - start_x;

        // Fill first row, then memcpy to remaining rows.
        let first_dst = start_y * pixels_per_row + start_x;
        unsafe {
            for i in 0..row_len {
                fb.add(first_dst + i).write(color);
            }
            for dst_y in (start_y + 1)..end_y {
                let dst_start = dst_y * pixels_per_row + start_x;
                core::ptr::copy_nonoverlapping(fb.add(first_dst), fb.add(dst_start), row_len);
            }
        }
    }

    /// Transfer the full buffer to the host and flush.
    pub fn flip(&mut self) -> u64 {
        self.flip_rect(0, 0, self.width, self.height)
    }

    /// Transfer only a dirty rect to the host and flush.
    pub fn flip_rect(&mut self, x: u32, y: u32, w: u32, h: u32) -> u64 {
        use crate::drivers::virtio::gpu::VirtioGpuRect;
        self.gpu.transfer_and_flush(
            1,
            VirtioGpuRect {
                x,
                y,
                width: w,
                height: h,
            },
            self.pitch,
        );
        0
    }
}

/// Screen dimensions, returned by the SCREEN_INFO ioctl.
#[derive(Debug, Clone, Copy)]
pub struct ScreenInfo {
    pub width: usize,
    pub height: usize,
    pub refresh_rate: u32,
}

pub struct DirectFramebuffer {
    width: usize,
    height: usize,
    pitch: usize,
    /// Base virtual address of the framebuffer (page 0).
    fb_base: *mut u32,
    /// True when the framebuffer pixel format is 32-bit 0x00RRGGBB
    /// (R=8 bits at shift 16, G=8 bits at shift 8, B=8 bits at shift 0,
    /// pitch aligned to 4 bytes). In this case we can memcpy rows directly
    /// without per-pixel color conversion.
    is_identity: bool,
    red_lut: [u32; 256],
    green_lut: [u32; 256],
    blue_lut: [u32; 256],
    converted_row_buffer: Vec<u32>,
    double_buffered: bool,
    /// Y offset in pixels of the back page (the page we draw into).
    back_page_y_offset: usize,
}

unsafe impl Send for DirectFramebuffer {}

impl DirectFramebuffer {
    pub fn new() -> Self {
        let fb = boot_info()
            .framebuffer
            .as_ref()
            .expect("DirectFramebuffer requires a Limine framebuffer");
        let width = fb.width as usize;
        let height = fb.height as usize;
        let pitch = fb.pitch as usize;
        let fb_base = fb.addr;

        let red_lut = Self::build_channel_lut(fb.red_mask_size, fb.red_mask_shift);
        let green_lut = Self::build_channel_lut(fb.green_mask_size, fb.green_mask_shift);
        let blue_lut = Self::build_channel_lut(fb.blue_mask_size, fb.blue_mask_shift);

        let is_identity = fb.bpp == 32
            && fb.red_mask_size == 8
            && fb.red_mask_shift == 16
            && fb.green_mask_size == 8
            && fb.green_mask_shift == 8
            && fb.blue_mask_size == 8
            && fb.blue_mask_shift == 0
            && pitch % 4 == 0;

        println!(
            "Framebuffer: {}x{} bpp={} identity={} (R={}@{} G={}@{} B={}@{})",
            width,
            height,
            fb.bpp,
            is_identity,
            fb.red_mask_size,
            fb.red_mask_shift,
            fb.green_mask_size,
            fb.green_mask_shift,
            fb.blue_mask_size,
            fb.blue_mask_shift,
        );

        let two_page_bytes = 2 * height * pitch;
        let vram_64k = dispi_read(DISPI_INDEX_VIDEO_MEMORY_64K) as usize;
        let vram_bytes = vram_64k * 64 * 1024;
        let double_buffered = vram_bytes >= two_page_bytes && pitch % 4 == 0;

        if double_buffered {
            // Map the second VRAM page into the kernel's address space.
            // Limine's HHDM only maps the first framebuffer page. The second
            // page at fb_base + page_size is unmapped MMIO that needs explicit
            // page table entries.
            let hhdm_offset = boot_info().physical_memory_offset.as_u64();
            let fb_phys = fb_base as u64 - hhdm_offset;
            let second_page_phys = fb_phys + (height * pitch) as u64;
            let second_page_virt = fb_base as u64 + (height * pitch) as u64;
            let second_page_size = (height * pitch) as u64;
            let flags =
                PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE | pat::WRITE_COMBINING;

            let mut mapper = memory_mapper();
            let page_size = 4096u64;
            let mut offset = 0u64;
            while offset < second_page_size {
                let virt = VirtAddr::new(second_page_virt + offset);
                let phys = PhysAddr::new(second_page_phys + offset);
                if let Err(e) = mapper.map_address(virt, phys, flags) {
                    println!(
                        "Framebuffer: failed to map second page at {:#x}: {:?}",
                        phys.as_u64(),
                        e
                    );
                    break;
                }
                offset += page_size;
            }

            dispi_write(DISPI_INDEX_VIRT_HEIGHT, (2 * height) as u16);
            dispi_write(DISPI_INDEX_Y_OFFSET, 0);
            println!(
                "Framebuffer: double buffering enabled (VRAM={}KB, need={}KB)",
                vram_bytes / 1024,
                two_page_bytes / 1024
            );
        } else {
            println!(
                "Framebuffer: double buffering disabled (VRAM={}KB, need={}KB)",
                vram_bytes / 1024,
                two_page_bytes / 1024
            );
        }

        // Re-map the first framebuffer page (Limine HHDM) with Write-Combining.
        {
            let first_page_virt = VirtAddr::new(fb_base as u64);
            let first_page_size = (height * pitch) as u64;
            let wc_flags =
                PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE | pat::WRITE_COMBINING;
            let mut mapper = memory_mapper();
            if let Err(e) = mapper.change_flags(first_page_virt, first_page_size, wc_flags) {
                println!("Framebuffer: failed to set WC on first page: {:?}", e);
            }
        }

        Self {
            width,
            height,
            pitch,
            fb_base,
            is_identity,
            red_lut,
            green_lut,
            blue_lut,
            // Pre-allocate to screen width so draw() never allocates under the lock.
            converted_row_buffer: alloc::vec![0u32; width],
            double_buffered,
            back_page_y_offset: if double_buffered { height } else { 0 },
        }
    }

    pub fn screen_info(&self) -> ScreenInfo {
        ScreenInfo {
            width: self.width,
            height: self.height,
            refresh_rate: 60,
        }
    }

    pub fn mmap_info(&self) -> FramebufferMmapInfo {
        let hhdm_offset = boot_info().physical_memory_offset.as_u64();
        let phys_addr = self.fb_base as u64 - hhdm_offset;
        let page_size = (self.height * self.pitch) as u64;
        let total_size = if self.double_buffered {
            2 * page_size
        } else {
            page_size
        };
        FramebufferMmapInfo {
            phys_addr,
            total_size,
            page_size,
            width: self.width as u32,
            height: self.height as u32,
            pitch: self.pitch as u32,
            double_buffered: self.double_buffered as u8,
            is_identity: self.is_identity as u8,
            _padding: [0; 2],
        }
    }

    fn build_channel_lut(mask_size: u8, shift: u8) -> [u32; 256] {
        let mask = if mask_size == 0 {
            0
        } else {
            (1u32 << mask_size.min(31)) - 1
        };

        core::array::from_fn(|value| {
            if mask == 0 {
                0
            } else {
                ((value as u32 * mask) / 255) << shift
            }
        })
    }

    /// Convert RGB color (0x00RRGGBB) to framebuffer's native format
    #[inline]
    fn convert_color(&self, rgb: u32) -> u32 {
        let r = ((rgb >> 16) & 0xFF) as u8;
        let g = ((rgb >> 8) & 0xFF) as u8;
        let b = (rgb & 0xFF) as u8;
        self.red_lut[r as usize] | self.green_lut[g as usize] | self.blue_lut[b as usize]
    }

    /// Draw pixels from `src` at position (x, y) with dimensions (src_width x src_height).
    /// Clips to screen bounds. `src` must contain at least `src_width * src_height` pixels.
    pub fn draw(&mut self, src: &[u32], x: u64, y: u64, src_width: usize, src_height: usize) {
        let start_x = x.min(self.width as u64) as usize;
        let start_y = y.min(self.height as u64) as usize;
        let end_x = (x + src_width as u64).min(self.width as u64) as usize;
        let end_y = (y + src_height as u64).min(self.height as u64) as usize;

        if start_x >= end_x || start_y >= end_y {
            return;
        }

        let pixels_per_row = self.pitch / 4;
        let src_offset_x = start_x - x as usize;
        let src_offset_y = start_y - y as usize;
        let row_len = end_x - start_x;
        let back_y = self.back_page_y_offset;

        // Bounds-check: verify the last row we'll read is within src
        let last_src_row = src_offset_y + (end_y - start_y) - 1;
        let last_src_index = last_src_row * src_width + src_offset_x + row_len;
        if last_src_index > src.len() {
            return;
        }

        if self.is_identity {
            // Fast path: memcpy rows directly to framebuffer
            let mut src_row = src_offset_y;
            for dst_y in start_y..end_y {
                let src_start = src_row * src_width + src_offset_x;
                let dst_start = (back_y + dst_y) * pixels_per_row + start_x;

                unsafe {
                    core::ptr::copy_nonoverlapping(
                        src[src_start..].as_ptr(),
                        self.fb_base.add(dst_start),
                        row_len,
                    );
                }

                src_row += 1;
            }
        } else {
            // Slow path: per-pixel LUT color conversion.
            // converted_row_buffer is pre-allocated to screen width in new().
            let red_lut = &self.red_lut;
            let green_lut = &self.green_lut;
            let blue_lut = &self.blue_lut;
            let conv = &mut self.converted_row_buffer[..row_len];

            let mut src_row = src_offset_y;
            for dst_y in start_y..end_y {
                let src_start = src_row * src_width + src_offset_x;
                let dst_start = (back_y + dst_y) * pixels_per_row + start_x;

                for i in 0..row_len {
                    let rgb = src[src_start + i];
                    let r = ((rgb >> 16) & 0xFF) as usize;
                    let g = ((rgb >> 8) & 0xFF) as usize;
                    let b = (rgb & 0xFF) as usize;
                    conv[i] = red_lut[r] | green_lut[g] | blue_lut[b];
                }

                unsafe {
                    core::ptr::copy_nonoverlapping(
                        conv.as_ptr(),
                        self.fb_base.add(dst_start),
                        row_len,
                    );
                }

                src_row += 1;
            }
        }
    }

    /// Fill a rectangle with a solid color.
    pub fn draw_rect(&mut self, x: u64, y: u64, width: u64, height: u64, color: u32) {
        if width == 0 || height == 0 {
            return;
        }

        let start_x = x.min(self.width as u64) as usize;
        let start_y = y.min(self.height as u64) as usize;
        let end_x = (x + width).min(self.width as u64) as usize;
        let end_y = (y + height).min(self.height as u64) as usize;

        if start_x >= end_x || start_y >= end_y {
            return;
        }

        let pixels_per_row = self.pitch / 4;
        let row_len = end_x - start_x;
        let back_y = self.back_page_y_offset;
        let native_color = if self.is_identity {
            color
        } else {
            self.convert_color(color)
        };

        // Fill one row in the pre-allocated buffer, then memcpy to each scanline.
        let row_buf = &mut self.converted_row_buffer[..row_len];
        row_buf.fill(native_color);

        for dst_y in start_y..end_y {
            let dst_start = (back_y + dst_y) * pixels_per_row + start_x;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    row_buf.as_ptr(),
                    self.fb_base.add(dst_start),
                    row_len,
                );
            }
        }
    }

    /// Flip the display to show the back page. When double buffering is active
    /// this writes the current `front_y` to the Bochs VBE Y_OFFSET register,
    /// making the just-drawn page visible, then swaps front and back.
    /// Returns the byte offset of the NEW back page (the one to write to next),
    /// or 0 when not double buffered.
    pub fn flip(&mut self) -> u64 {
        if !self.double_buffered {
            return 0;
        }
        let show_y = self.back_page_y_offset as u16;
        dispi_write(DISPI_INDEX_Y_OFFSET, show_y);
        if self.back_page_y_offset == 0 {
            self.back_page_y_offset = self.height;
        } else {
            self.back_page_y_offset = 0;
        }
        (self.back_page_y_offset * self.pitch) as u64
    }
}
