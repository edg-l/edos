pub mod codec;
pub mod regs;

use alloc::{format, string::String, sync::Arc, vec::Vec};
use spin::Mutex;
use x86_64::{VirtAddr, structures::paging::PageTableFlags};

use crate::{
    drivers::{
        dma::{DmaBuffer, dma},
        msi,
        pci::{
            config::{pci_read_u16, pci_write_u16, read_bar_phys},
            pci_manager,
        },
    },
    fs::devfs::{self, DevFsDevice, DevFsError},
    interrupts::InterruptIndex,
    log,
    memory::{mapper::memory_mapper, valloc::vmalloc},
    thread::{runqueue::IO_PRIORITY, scheduler::sched, util::queue_spawn_kthread_named},
};

use self::regs::*;

// === Audio ioctl constants ===
pub const AUDIO_IOCTL_SET_FORMAT: u64 = 1;
pub const AUDIO_IOCTL_DRAIN: u64 = 3;

/// Intel HDA controller state.
pub struct HdaController {
    mmio_base: VirtAddr,
    /// CORB DMA buffer (256 entries * 4 bytes = 1KB)
    corb: DmaBuffer,
    /// RIRB DMA buffer (256 entries * 8 bytes = 2KB)
    rirb: DmaBuffer,
    /// Software read pointer into RIRB (0..255)
    rirb_rp: u16,
    /// CORB write pointer (software mirror)
    corb_wp: u16,
    /// Number of input streams (from GCAP)
    iss: u8,
    /// Number of output streams (from GCAP)
    oss: u8,
    /// Base offset of first output stream descriptor = 0x80 + iss * 0x20
    out_stream_base: u32,
    /// BDL (Buffer Descriptor List) DMA buffer
    bdl: Option<DmaBuffer>,
    /// Audio data DMA buffers (one per BDL entry)
    audio_buffers: Option<Vec<DmaBuffer>>,
    /// Write cursor: next BDL entry index to fill with audio data (0..BDL_ENTRIES-1)
    write_cursor: usize,
    /// Read cursor: tracks DMA consumption position (advanced on each BCIS interrupt)
    read_cursor: usize,
}

impl HdaController {
    // --- MMIO register access helpers ---

    fn read32(&self, offset: u32) -> u32 {
        unsafe { core::ptr::read_volatile((self.mmio_base + offset as u64).as_ptr::<u32>()) }
    }

    fn write32(&self, offset: u32, val: u32) {
        unsafe {
            core::ptr::write_volatile((self.mmio_base + offset as u64).as_mut_ptr::<u32>(), val)
        }
    }

    fn read16(&self, offset: u32) -> u16 {
        unsafe { core::ptr::read_volatile((self.mmio_base + offset as u64).as_ptr::<u16>()) }
    }

    fn write16(&self, offset: u32, val: u16) {
        unsafe {
            core::ptr::write_volatile((self.mmio_base + offset as u64).as_mut_ptr::<u16>(), val)
        }
    }

    fn read8(&self, offset: u32) -> u8 {
        unsafe { core::ptr::read_volatile((self.mmio_base + offset as u64).as_ptr::<u8>()) }
    }

    fn write8(&self, offset: u32, val: u8) {
        unsafe {
            core::ptr::write_volatile((self.mmio_base + offset as u64).as_mut_ptr::<u8>(), val)
        }
    }

    /// Initialize the HDA controller: reset, discover capabilities, setup CORB/RIRB.
    pub fn new(
        pci_device: &crate::drivers::pci::structures::PciDevice,
    ) -> Result<Self, alloc::string::String> {
        // Enable PCI bus mastering
        let mut cmd = pci_read_u16(pci_device.address, 0x04);
        cmd |= 0x04; // Bus Master Enable
        pci_write_u16(pci_device.address, 0x04, cmd);

        // Read BAR0 physical address
        let bar0_phys = read_bar_phys(pci_device.address, 0);

        // Map MMIO region (16KB) into a fresh virtual address with NO_CACHE.
        // Cannot use the boot identity map: it uses cacheable 2MB huge pages,
        // which causes stale reads on MMIO registers.
        let mmio_size: u64 = 0x4000;
        let mmio_virt = vmalloc(mmio_size);
        {
            let mut mapper = memory_mapper();
            mapper
                .map_address_range(
                    mmio_virt,
                    bar0_phys,
                    mmio_size as usize,
                    PageTableFlags::PRESENT
                        | PageTableFlags::WRITABLE
                        | PageTableFlags::NO_CACHE
                        | PageTableFlags::GLOBAL,
                )
                .map_err(|e| format!("HDA: MMIO map failed: {:?}", e))?;
        }

        // Allocate CORB (256 * 4 = 1024 bytes) and RIRB (256 * 8 = 2048 bytes)
        let corb = dma()
            .allocate_sized(1024)
            .map_err(|e| format!("HDA: CORB alloc: {:?}", e))?;
        let rirb = dma()
            .allocate_sized(2048)
            .map_err(|e| format!("HDA: RIRB alloc: {:?}", e))?;

        let mut ctrl = Self {
            mmio_base: mmio_virt,
            corb,
            rirb,
            rirb_rp: 0,
            corb_wp: 0,
            iss: 0,
            oss: 0,
            out_stream_base: 0,
            bdl: None,
            audio_buffers: None,
            write_cursor: 0,
            read_cursor: 0,
        };

        // Controller reset
        ctrl.reset()?;

        // Read capabilities
        let gcap = ctrl.read16(GCAP);
        ctrl.iss = ((gcap >> 8) & 0x0F) as u8;
        ctrl.oss = ((gcap >> 12) & 0x0F) as u8;
        ctrl.out_stream_base = 0x80 + (ctrl.iss as u32) * 0x20;

        let vmin = ctrl.read8(VMIN);
        let vmaj = ctrl.read8(VMAJ);
        log!(
            "hda: version {}.{}, iss={}, oss={}",
            vmaj,
            vmin,
            ctrl.iss,
            ctrl.oss
        );

        // Check for codecs
        let statests = ctrl.read16(STATESTS);
        if statests == 0 {
            return Err("HDA: no codecs detected".into());
        }
        log!("hda: codec presence mask: {:#x}", statests);

        // Setup CORB and RIRB
        ctrl.setup_corb()?;
        ctrl.setup_rirb()?;

        Ok(ctrl)
    }

    /// Reset the controller: clear CRST, wait, set CRST, wait for codec enumeration.
    fn reset(&self) -> Result<(), alloc::string::String> {
        // Enter reset: clear CRST
        self.write32(GCTL, 0);
        for _ in 0..1_000_000 {
            if self.read32(GCTL) & GCTL_CRST == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        if self.read32(GCTL) & GCTL_CRST != 0 {
            return Err("HDA: failed to enter reset".into());
        }

        // Exit reset: set CRST
        self.write32(GCTL, GCTL_CRST);
        for _ in 0..1_000_000 {
            if self.read32(GCTL) & GCTL_CRST != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        if self.read32(GCTL) & GCTL_CRST == 0 {
            return Err("HDA: failed to exit reset".into());
        }

        // Wait for codec enumeration (HDA spec: >=521us after CRST=1).
        // Spin ~1ms worth of iterations.
        for _ in 0..2_000_000 {
            core::hint::spin_loop();
        }

        Ok(())
    }

    /// Set up the CORB (Command Outbound Ring Buffer).
    fn setup_corb(&mut self) -> Result<(), alloc::string::String> {
        // Stop CORB if running
        self.write8(CORBCTL, 0);
        for _ in 0..10_000 {
            if self.read8(CORBCTL) & CORBCTL_RUN == 0 {
                break;
            }
            core::hint::spin_loop();
        }

        // Set CORB size to 256 entries (bits [1:0] = 0b10)
        self.write8(CORBSIZE, 0x02);

        // Program CORB base address
        let corb_phys = self.corb.phys_addr().as_u64();
        self.write32(CORBLBASE, corb_phys as u32);
        self.write32(CORBUBASE, (corb_phys >> 32) as u32);

        // Reset CORB read pointer: set bit 15, wait for it to read back 1
        self.write16(CORBRP, 0x8000);
        for _ in 0..10_000 {
            if self.read16(CORBRP) & 0x8000 != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        // Clear bit 15, wait for it to read back 0
        self.write16(CORBRP, 0x0000);
        for _ in 0..10_000 {
            if self.read16(CORBRP) & 0x8000 == 0 {
                break;
            }
            core::hint::spin_loop();
        }

        // Reset write pointer
        self.write16(CORBWP, 0);
        self.corb_wp = 0;

        // Start CORB DMA
        self.write8(CORBCTL, CORBCTL_RUN);

        Ok(())
    }

    /// Set up the RIRB (Response Inbound Ring Buffer).
    fn setup_rirb(&mut self) -> Result<(), alloc::string::String> {
        // Stop RIRB if running
        self.write8(RIRBCTL, 0);
        for _ in 0..10_000 {
            if self.read8(RIRBCTL) & RIRBCTL_DMA_EN == 0 {
                break;
            }
            core::hint::spin_loop();
        }

        // Set RIRB size to 256 entries (bits [1:0] = 0b10)
        self.write8(RIRBSIZE, 0x02);

        // Program RIRB base address
        let rirb_phys = self.rirb.phys_addr().as_u64();
        self.write32(RIRBLBASE, rirb_phys as u32);
        self.write32(RIRBUBASE, (rirb_phys >> 32) as u32);

        // Reset RIRB write pointer (write bit 15 - self-clearing)
        self.write16(RIRBWP, 0x8000);

        // Set interrupt count to 1 (interrupt after every response)
        self.write16(RINTCNT, 1);

        // Initialize software read pointer
        self.rirb_rp = 0;

        // Start RIRB DMA with IRQ_EN so RIRBSTS gets set when rirb_count
        // reaches RINTCNT. Required for CORB processing to resume after clearing
        // RIRBSTS (QEMU only sets RIRBSTS if IRQ_EN is active). No actual interrupt
        // fires until INTCTL GIE is enabled later.
        self.write8(RIRBCTL, RIRBCTL_DMA_EN | RIRBCTL_IRQ_EN);

        Ok(())
    }

    /// Send a command verb via CORB and wait for the response via RIRB.
    /// Used only during init (spin-polls, not interrupt-driven).
    /// `verb` is the full 32-bit command: (cad << 28) | (nid << 20) | command_payload
    pub fn send_command(&mut self, verb: u32) -> Result<u32, alloc::string::String> {
        // Clear RIRB interrupt status BEFORE sending the command.
        // QEMU's intel-hda stops processing CORB when rirb_count reaches RINTCNT.
        // Clearing RIRBSTS resets rirb_count to 0 and re-runs the CORB engine.
        self.write8(RIRBSTS, RIRBSTS_IRQ);

        // Write verb to next CORB slot
        let wp = (self.corb_wp + 1) % 256;
        let corb_ptr = self.corb.as_ptr() as *mut u32;
        unsafe { core::ptr::write_volatile(corb_ptr.add(wp as usize), verb) };
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Release);

        // Update CORB write pointer to trigger DMA
        self.write16(CORBWP, wp);
        self.corb_wp = wp;

        // Poll RIRB write pointer until at least one new entry arrives
        for _ in 0..1_000_000 {
            if self.read16(RIRBWP) != self.rirb_rp {
                break;
            }
            core::hint::spin_loop();
        }
        if self.read16(RIRBWP) == self.rirb_rp {
            return Err("HDA: RIRB timeout".into());
        }

        // Read the next response (each RIRB entry is 8 bytes: [0..4] = response, [4..8] = response_ex)
        let next_rp = (self.rirb_rp + 1) % 256;
        let rirb_ptr = self.rirb.as_ptr() as *const u64;
        let entry = unsafe { core::ptr::read_volatile(rirb_ptr.add(next_rp as usize)) };
        self.rirb_rp = next_rp;

        // Lower 32 bits are the response
        Ok(entry as u32)
    }

    /// Convenience: send a command to codec_addr, node nid, with a 20-bit verb payload.
    pub fn codec_command(
        &mut self,
        cad: u8,
        nid: u8,
        verb: u32,
    ) -> Result<u32, alloc::string::String> {
        let cmd = ((cad as u32) << 28) | ((nid as u32) << 20) | (verb & 0xFFFFF);
        self.send_command(cmd)
    }

    /// Configure MSI interrupt for this device.
    pub fn setup_msi(&self, pci_device: &crate::drivers::pci::structures::PciDevice) {
        match msi::enable_msi_for_device(pci_device, InterruptIndex::Hda.as_u8()) {
            Ok(()) => log!("hda: MSI enabled"),
            Err(e) => log!("hda: MSI setup failed: {:?}", e),
        }
    }

    /// Set up the output stream DMA engine with BDL and audio buffers.
    pub fn setup_output_stream(&mut self) -> Result<(), String> {
        // Reset the output stream: set SRST, wait for it to read back, then clear it.
        self.write32(self.out_stream_base + SD_CTL, SD_CTL_SRST);
        for _ in 0..100_000 {
            if self.read32(self.out_stream_base + SD_CTL) & SD_CTL_SRST != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        self.write32(self.out_stream_base + SD_CTL, 0);
        for _ in 0..100_000 {
            if self.read32(self.out_stream_base + SD_CTL) & SD_CTL_SRST == 0 {
                break;
            }
            core::hint::spin_loop();
        }

        // Allocate BDL DMA buffer (BDL_ENTRIES * 16 bytes = 512 bytes, page-aligned).
        let bdl_buf = dma()
            .allocate_sized(BDL_ENTRIES * 16)
            .map_err(|e| format!("HDA: BDL alloc: {:?}", e))?;

        // Allocate one audio DMA buffer per BDL entry.
        let mut audio_bufs: Vec<DmaBuffer> = Vec::with_capacity(BDL_ENTRIES);
        for i in 0..BDL_ENTRIES {
            let buf = dma()
                .allocate_sized(AUDIO_BUF_SIZE)
                .map_err(|e| format!("HDA: audio buf {} alloc: {:?}", i, e))?;
            audio_bufs.push(buf);
        }

        // Fill BDL entries.
        let bdl_ptr = bdl_buf.as_ptr() as *mut BdlEntry;
        for (i, abuf) in audio_bufs.iter().enumerate() {
            let phys = abuf.phys_addr().as_u64();
            let ioc = 1u32; // IOC on every entry for per-buffer interrupt tracking
            let entry = BdlEntry {
                address: phys,
                length: AUDIO_BUF_SIZE as u32,
                flags: ioc,
            };
            unsafe { core::ptr::write_volatile(bdl_ptr.add(i), entry) };
        }

        // Program stream descriptor registers.
        let bdl_phys = bdl_buf.phys_addr().as_u64();
        self.write32(self.out_stream_base + SD_BDLPL, bdl_phys as u32);
        self.write32(self.out_stream_base + SD_BDLPU, (bdl_phys >> 32) as u32);

        // Total cyclic buffer length = number of entries * bytes per entry.
        let cbl = (BDL_ENTRIES * AUDIO_BUF_SIZE) as u32;
        self.write32(self.out_stream_base + SD_CBL, cbl);

        // Last valid index.
        self.write16(self.out_stream_base + SD_LVI, (BDL_ENTRIES - 1) as u16);

        // Stream format: 48kHz, 16-bit, stereo (must match codec SET_STREAM_FORMAT).
        self.write16(self.out_stream_base + SD_FMT, 0x0011);

        // SD_CTL: set stream tag=1 in bits [23:20], enable IOCE (bit 2).
        // The stream tag must match the codec's SET_CHANNEL_STREAM tag (1 << 4 in codec.rs).
        let ctl_val = (1u32 << SD_CTL_STREAM_TAG_SHIFT) | SD_CTL_IOCE;
        self.write32(self.out_stream_base + SD_CTL, ctl_val);

        self.bdl = Some(bdl_buf);
        self.audio_buffers = Some(audio_bufs);
        self.write_cursor = 0;
        self.read_cursor = 0;

        log!("hda: output stream configured, cbl={}", cbl);
        Ok(())
    }

    /// Start the output stream DMA engine.
    pub fn start_stream(&self) {
        let current = self.read32(self.out_stream_base + SD_CTL);
        self.write32(self.out_stream_base + SD_CTL, current | SD_CTL_RUN);
    }

    /// Stop the output stream DMA engine and wait until it halts.
    pub fn stop_stream(&self) {
        let current = self.read32(self.out_stream_base + SD_CTL);
        self.write32(self.out_stream_base + SD_CTL, current & !SD_CTL_RUN);
        for _ in 0..1_000_000 {
            if self.read32(self.out_stream_base + SD_CTL) & SD_CTL_RUN == 0 {
                break;
            }
            core::hint::spin_loop();
        }
    }

    /// Check and clear the Buffer Completion Interrupt Status (BCIS) bit.
    /// Returns true if BCIS was set (a buffer descriptor completed).
    pub fn handle_stream_interrupt(&self) -> bool {
        let sts = self.read8(self.out_stream_base + SD_STS);
        if sts & SD_STS_BCIS != 0 {
            // Write-1-to-clear BCIS.
            self.write8(self.out_stream_base + SD_STS, SD_STS_BCIS);
            true
        } else {
            false
        }
    }

    /// Number of BDL entries queued (filled by write but not yet consumed by DMA).
    fn entries_queued(&self) -> usize {
        (self.write_cursor + BDL_ENTRIES - self.read_cursor) % BDL_ENTRIES
    }
}

// === Shared playback state ===

struct HdaPlaybackState {
    controller: HdaController,
    pci_device: crate::drivers::pci::structures::PciDevice,
    stream_running: bool,
}

// === DevFs /dev/dsp device ===

struct HdaDspDevice {
    state: Arc<Mutex<HdaPlaybackState>>,
}

impl DevFsDevice for HdaDspDevice {
    /// Write PCM audio data into DMA ring buffers, starting the stream when ready.
    ///
    /// This is a non-blocking ring fill: writes as much as the ring can hold and
    /// returns the byte count written. If the ring is full, returns 0 and the caller
    /// should retry. Data must be 48kHz, 16-bit, stereo (matching codec format).
    fn write(&self, _offset: usize, data: &[u8]) -> Result<usize, DevFsError> {
        let mut state = self.state.lock();

        if state.controller.audio_buffers.is_none() {
            return Err(DevFsError::IoError);
        }

        let mut bytes_written = 0usize;
        let mut remaining = data;

        while !remaining.is_empty() {
            // Ring full: reserve one slot so cursors distinguish empty from full.
            if state.controller.entries_queued() >= BDL_ENTRIES - 1 {
                break;
            }

            let cursor = state.controller.write_cursor;
            let buf_ptr = {
                let bufs = state.controller.audio_buffers.as_ref().unwrap();
                bufs[cursor].as_ptr()
            };
            let copy_len = remaining.len().min(AUDIO_BUF_SIZE);

            unsafe {
                core::ptr::copy_nonoverlapping(remaining.as_ptr(), buf_ptr, copy_len);
                if copy_len < AUDIO_BUF_SIZE {
                    core::ptr::write_bytes(buf_ptr.add(copy_len), 0, AUDIO_BUF_SIZE - copy_len);
                }
            }

            bytes_written += copy_len;
            remaining = &remaining[copy_len..];
            state.controller.write_cursor = (cursor + 1) % BDL_ENTRIES;
        }

        // Start the DMA engine once we have queued data.
        if !state.stream_running && state.controller.entries_queued() > 0 {
            state.controller.start_stream();
            state.stream_running = true;
            log!("hda: stream started");
        }

        Ok(bytes_written)
    }

    fn ioctl(&self, request: u64, arg: u64) -> Result<u64, DevFsError> {
        match request {
            AUDIO_IOCTL_SET_FORMAT => {
                // arg encodes format as: bits [15:0] = sample_rate,
                // bits [23:16] = bits_per_sample, bits [31:24] = channels.
                let sample_rate = (arg & 0xFFFF) as u32;
                let bits = ((arg >> 16) & 0xFF) as u16;
                let channels = ((arg >> 24) & 0xFF) as u16;

                if sample_rate != 48_000 || channels != 2 || bits != 16 {
                    log!(
                        "hda: unsupported format {}Hz/{}ch/{}bit",
                        sample_rate,
                        channels,
                        bits
                    );
                    return Err(DevFsError::Unsupported);
                }
                Ok(0)
            }
            AUDIO_IOCTL_DRAIN => {
                // Wait until all queued buffers have been consumed by DMA.
                loop {
                    let done = {
                        let state = self.state.lock();
                        !state.stream_running || state.controller.entries_queued() == 0
                    };
                    if done {
                        break;
                    }
                    // Yield to let the driver kthread process interrupts.
                    sched().thread_yield();
                }
                // Stop the stream after drain so the DMA engine doesn't cycle silence.
                {
                    let mut state = self.state.lock();
                    if state.stream_running {
                        state.controller.stop_stream();
                        state.stream_running = false;
                    }
                }
                Ok(0)
            }
            _ => Err(DevFsError::Unsupported),
        }
    }
}

// === Driver thread ===

pub fn init() {
    queue_spawn_kthread_named("hda", hda_driver_main as *const () as u64);
}

pub extern "C" fn hda_driver_main() -> ! {
    use crate::interrupts::io::HDA_DRIVER_THREAD_ID;

    let thread = sched().current_thread().unwrap();
    thread.set_priority(IO_PRIORITY);
    HDA_DRIVER_THREAD_ID.call_once(|| thread.id);

    let devices = pci_manager().read().get_devices().to_vec();
    let pci_dev = devices
        .iter()
        .find(|d| d.header.class_code == 0x04 && d.header.subclass == 0x03)
        .copied();

    let Some(pci_dev) = pci_dev else {
        log!("hda: no device found");
        loop {
            sched().thread_park();
        }
    };

    log!(
        "hda: found {:04x}:{:04x}",
        pci_dev.header.vendor_id,
        pci_dev.header.device_id
    );

    let mut controller = match HdaController::new(&pci_dev) {
        Ok(c) => c,
        Err(e) => {
            log!("hda: init failed: {}", e);
            loop {
                sched().thread_park();
            }
        }
    };

    // Discover and configure codec
    match codec::discover_and_configure(&mut controller) {
        Ok(codec_info) => log!(
            "hda: codec ready, dac=nid{}, pin=nid{}",
            codec_info.dac_nid,
            codec_info.pin_nid
        ),
        Err(e) => {
            log!("hda: codec setup failed: {}", e);
            loop {
                sched().thread_park();
            }
        }
    }

    // Set up the output stream DMA engine.
    if let Err(e) = controller.setup_output_stream() {
        log!("hda: stream setup failed: {}", e);
        loop {
            sched().thread_park();
        }
    }

    // Enable MSI interrupts.
    controller.setup_msi(&pci_dev);

    // Enable global and stream interrupts in INTCTL.
    // Output stream 0 interrupt bit = ISS + 0.
    let iss = controller.iss as u32;
    let intctl = INTCTL_GIE | INTCTL_CIE | (1u32 << (iss + 0));
    controller.write32(INTCTL, intctl);

    // Wrap controller in shared state for the /dev/dsp device.
    let shared_state = Arc::new(Mutex::new(HdaPlaybackState {
        controller,
        pci_device: pci_dev,
        stream_running: false,
    }));

    // Register /dev/dsp.
    let dsp_device = Arc::new(HdaDspDevice {
        state: Arc::clone(&shared_state),
    });
    match devfs::register_device_str("/dsp", dsp_device) {
        Ok(()) => log!("hda: /dev/dsp registered"),
        Err(e) => log!("hda: failed to register /dev/dsp: {:?}", e),
    }

    log!("hda: driver ready");

    // Main loop: park the thread, wake on interrupt, handle BCIS.
    loop {
        sched().thread_park();

        let mut state = shared_state.lock();
        if state.controller.handle_stream_interrupt() {
            // A BDL entry completed: advance read_cursor.
            if state.controller.read_cursor != state.controller.write_cursor {
                state.controller.read_cursor = (state.controller.read_cursor + 1) % BDL_ENTRIES;
            }
        }
    }
}
