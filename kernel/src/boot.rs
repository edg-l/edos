use limine::{
    BaseRevision, RequestsEndMarker, RequestsStartMarker,
    request::{
        DtbRequest, ExecutableCmdlineRequest, FramebufferRequest, HhdmRequest, MemmapRequest,
        MemmapResponse, ModulesRequest, MpRequest, RsdpRequest, StackSizeRequest,
    },
};
use spin::Once;
use x86_64::{
    VirtAddr,
    registers::control::{Cr3, Cr3Flags},
    structures::paging::{OffsetPageTable, PhysFrame},
};

use crate::{
    main,
    memory::mapper::{MemoryManager, active_level_4_table},
    serial,
    thread::irqlock::IrqSpinlock,
    util::per_cpu::init_gs_for_bsp_static,
};

/// Basename of the Limine module carrying the live root filesystem image.
pub const LIVE_ROOT_MODULE: &str = "live-root.img";

/// Sets the base revision to the latest revision supported by the crate.
/// See specification for further info.
/// Be sure to mark all limine requests with #[used], otherwise they may be removed by the compiler.
#[used]
// The .requests section allows limine to find the requests faster and more safely.
#[unsafe(link_section = ".requests")]
pub static BASE_REVISION: BaseRevision = BaseRevision::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static MEMORY_MAP_REQUEST: MemmapRequest = MemmapRequest::new();

// Request the higher-half direct mapping
#[used]
#[unsafe(link_section = ".requests")]
pub static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static STACK_SIZE_REQUEST: StackSizeRequest = StackSizeRequest::new(64 * 1024);

// Request the RSDP address
#[used]
#[unsafe(link_section = ".requests")]
static RSDP_REQUEST: RsdpRequest = RsdpRequest::new();

// Request the executable address
#[used]
#[unsafe(link_section = ".requests")]
static EXECUTABLE_CMDLINE_REQUEST: ExecutableCmdlineRequest = ExecutableCmdlineRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
static DEVICE_TREE_BLOB_REQUEST: DtbRequest = DtbRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static MP_REQUEST: MpRequest = MpRequest::new(0);

// Modules loaded from the boot medium. Used for the live root image the ISO
// carries; see `drivers::ramdisk`.
#[used]
#[unsafe(link_section = ".requests")]
static MODULES_REQUEST: ModulesRequest = ModulesRequest::new();

/// Define the start and end markers for Limine requests.
#[used]
#[unsafe(link_section = ".requests_start_marker")]
static _START_MARKER: RequestsStartMarker = RequestsStartMarker::new();
#[used]
#[unsafe(link_section = ".requests_end_marker")]
static _END_MARKER: RequestsEndMarker = RequestsEndMarker::new();

#[expect(
    unused,
    reason = "the Limine handoff in full; the kernel reads the parts it needs"
)]
pub struct BootInfo {
    pub framebuffer: Option<BootFramebuffer>,
    pub memory_map: &'static MemmapResponse,
    pub physical_memory_offset: VirtAddr,
    pub memory_manager: IrqSpinlock<MemoryManager>,
    pub rdsp: usize,
    pub cmdline: &'static str,
    pub device_tree_blob: Option<usize>,
    pub cr3: (PhysFrame, Cr3Flags),
    /// The live root image, when the boot medium carried one.
    pub live_root: Option<BootModule>,
}

/// A module Limine loaded for us.
///
/// `addr` is a kernel virtual address: from base revision 4 on, Limine reports
/// response pointers through the higher-half direct map, so no conversion is
/// needed. The bytes live in bootloader-reclaimable memory, which the frame
/// allocator never hands out (it frees only `MEMMAP_USABLE`), so they stay
/// valid and privately owned for the life of the boot.
#[derive(Clone, Copy)]
pub struct BootModule {
    pub addr: *mut u8,
    pub len: usize,
}

// SAFETY: `addr` is a higher-half direct-map address into
// bootloader-reclaimable memory, which the frame allocator never hands out,
// so the bytes stay valid and privately owned for the life of the boot. They
// are never written after Limine hands them over, so sharing the value hands
// out reads only.
unsafe impl Send for BootModule {}
// SAFETY: the same argument as the impl above.
unsafe impl Sync for BootModule {}

/// Extracted framebuffer data from Limine, safe to share across threads.
pub struct BootFramebuffer {
    pub addr: *mut u32,
    pub width: u64,
    pub height: u64,
    pub pitch: u64,
    pub bpp: u16,
    pub red_mask_size: u8,
    pub red_mask_shift: u8,
    pub green_mask_size: u8,
    pub green_mask_shift: u8,
    pub blue_mask_size: u8,
    pub blue_mask_shift: u8,
}

// SAFETY: the value is a plain record of what Limine reported, stored once in
// `BOOT_INFO` and only read from there. `addr` names a mapping that lives for
// the whole boot, and the one owner that draws through it is
// `graphics::DirectFramebuffer`.
unsafe impl Send for BootFramebuffer {}
// SAFETY: the same argument as the impl above.
unsafe impl Sync for BootFramebuffer {}

pub static BOOT_INFO: Once<BootInfo> = Once::new();

pub fn boot_info() -> &'static BootInfo {
    // SAFETY: `kmain` fills `BOOT_INFO` before it calls anything that could
    // reach this, and nothing else in the kernel runs before `kmain`.
    unsafe { BOOT_INFO.get().unwrap_unchecked() }
}

/// Kernel entry point.
///
/// # Safety
/// Only Limine may call this, once, on the BSP, with the boot protocol's state
/// still intact: the base revision answered, the requests filled in, and the
/// bootloader-provided stack and identity mappings still live. It is the first
/// Rust to run, so it installs the BSP's GS base and fills `BOOT_INFO` before
/// anything else can read either; a second entry would redo that under a
/// kernel that is already scheduling.
#[unsafe(no_mangle)]
unsafe extern "C" fn kmain() -> ! {
    // All limine requests must also be referenced in a called function, otherwise they may be
    // removed by the linker.
    assert!(BASE_REVISION.is_supported());

    crate::util::per_cpu::probe_and_enable_fsgsbase();
    // SAFETY: the first thing the BSP does, so it holds no reference into a
    // per-CPU block and there is no allocator yet for the dynamic form to use;
    // `probe_and_enable_fsgsbase` on the line above has made `wrgsbase` legal.
    unsafe { init_gs_for_bsp_static() };

    // Early init serial in case we panic on expects.
    serial::init();

    let framebuffer = FRAMEBUFFER_REQUEST
        .response()
        .and_then(|r| r.framebuffers().first())
        .map(|fb| BootFramebuffer {
            addr: fb.address() as *mut u32,
            width: fb.width,
            height: fb.height,
            pitch: fb.pitch,
            bpp: fb.bpp,
            red_mask_size: fb.red_mask_size,
            red_mask_shift: fb.red_mask_shift,
            green_mask_size: fb.green_mask_size,
            green_mask_shift: fb.green_mask_shift,
            blue_mask_size: fb.blue_mask_size,
            blue_mask_shift: fb.blue_mask_shift,
        });

    let memory_map = MEMORY_MAP_REQUEST.response().expect("need the memory map");

    let physical_memory_offset = HHDM_REQUEST
        .response()
        .expect("need higher half mapping")
        .offset;

    // Base revision >= 4: RSDP/DTB addresses are virtual (HHDM), convert to physical.
    let rdsp = RSDP_REQUEST.response().unwrap().address as usize - physical_memory_offset as usize;

    let _stack_size = STACK_SIZE_REQUEST.response().expect("need size");

    let cmdline = EXECUTABLE_CMDLINE_REQUEST.response().unwrap().cmdline();

    let device_tree_blob: Option<usize> = DEVICE_TREE_BLOB_REQUEST
        .response()
        .map(|x| x.dtb_ptr as usize - physical_memory_offset as usize);

    // The live root module, if the boot medium carried one. Matched by
    // basename so the ISO layout can move without touching this.
    let live_root = MODULES_REQUEST.response().and_then(|r| {
        r.modules()
            .iter()
            .find(|m| m.path().ends_with(LIVE_ROOT_MODULE))
            .map(|m| {
                let data = m.data();
                BootModule {
                    addr: data.as_ptr() as *mut u8,
                    len: data.len(),
                }
            })
    });

    let physical_memory_offset = VirtAddr::new(physical_memory_offset);

    let cr3 = Cr3::read();
    // SAFETY: `physical_memory_offset` is the HHDM base Limine reported, so
    // all of physical memory is mapped at it, and this is the only place that
    // takes the level-4 table — nothing else holds a reference to it.
    let level_4_table = unsafe { active_level_4_table(physical_memory_offset) };
    // SAFETY: the same HHDM offset the table was read through, which is what
    // `OffsetPageTable` needs to walk the frames the entries name.
    let kernel_page_table = unsafe { OffsetPageTable::new(level_4_table, physical_memory_offset) };

    let boot_info = BootInfo {
        framebuffer,
        memory_map,
        physical_memory_offset,
        memory_manager: IrqSpinlock::new(MemoryManager::new(kernel_page_table)),
        rdsp,
        cmdline,
        device_tree_blob,
        cr3,
        live_root,
    };

    BOOT_INFO.call_once(|| boot_info);

    main()
}
