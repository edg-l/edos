use core::ffi::CStr;

use limine::{
    BaseRevision,
    framebuffer::Framebuffer,
    request::{
        DeviceTreeBlobRequest, ExecutableCmdlineRequest, FramebufferRequest, HhdmRequest,
        MemoryMapRequest, MpRequest, RequestsEndMarker, RequestsStartMarker, RsdpRequest,
        StackSizeRequest,
    },
    response::MemoryMapResponse,
};
use spin::{Mutex, Once};
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
pub static MEMORY_MAP_REQUEST: MemoryMapRequest = MemoryMapRequest::new();

// Request the higher-half direct mapping
#[used]
#[unsafe(link_section = ".requests")]
pub static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static STACK_SIZE_REQUEST: StackSizeRequest = StackSizeRequest::new().with_size(64 * 1024);

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
static DEVICE_TREE_BLOB_REQUEST: DeviceTreeBlobRequest = DeviceTreeBlobRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static MP_REQUEST: MpRequest = MpRequest::new();

/// Define the start and end markers for Limine requests.
#[used]
#[unsafe(link_section = ".requests_start_marker")]
static _START_MARKER: RequestsStartMarker = RequestsStartMarker::new();
#[used]
#[unsafe(link_section = ".requests_end_marker")]
static _END_MARKER: RequestsEndMarker = RequestsEndMarker::new();

#[expect(unused)]
pub struct BootInfo {
    pub framebuffer: Framebuffer<'static>,
    pub memory_map: &'static MemoryMapResponse,
    pub physical_memory_offset: VirtAddr,
    pub memory_manager: IrqSpinlock<MemoryManager>,
    pub rdsp: usize,
    pub cmdline: &'static CStr,
    pub device_tree_blob: Option<usize>,
    pub cr3: (PhysFrame, Cr3Flags),
}

pub static BOOT_INFO: Once<BootInfo> = Once::new();

pub fn boot_info() -> &'static BootInfo {
    unsafe { BOOT_INFO.get().unwrap_unchecked() }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn kmain() -> ! {
    // All limine requests must also be referenced in a called function, otherwise they may be
    // removed by the linker.
    assert!(BASE_REVISION.is_supported());

    unsafe { init_gs_for_bsp_static() };

    // Early init serial in case we panic on expects.
    serial::init();

    let framebuffer = FRAMEBUFFER_REQUEST
        .get_response()
        .expect("need a framebuffer")
        .framebuffers()
        .next()
        .expect("need a framebuffer");

    let memory_map = MEMORY_MAP_REQUEST
        .get_response()
        .expect("need the memory map");

    let physical_memory_offset = HHDM_REQUEST
        .get_response()
        .expect("need higher half mapping")
        .offset();

    let rdsp = RSDP_REQUEST.get_response().unwrap().address();

    let _stack_size = STACK_SIZE_REQUEST.get_response().expect("need size");

    let cmdline = EXECUTABLE_CMDLINE_REQUEST.get_response().unwrap().cmdline();

    let device_tree_blob: Option<usize> = DEVICE_TREE_BLOB_REQUEST
        .get_response()
        .map(|x| x.dtb_ptr().addr());

    let physical_memory_offset = VirtAddr::new(physical_memory_offset);

    let cr3 = Cr3::read();
    let level_4_table = unsafe { active_level_4_table(physical_memory_offset) };
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
    };

    BOOT_INFO.call_once(|| boot_info);

    main()
}
