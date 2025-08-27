use limine::{
    BaseRevision,
    framebuffer::Framebuffer,
    request::{FramebufferRequest, RequestsEndMarker, RequestsStartMarker},
};

use crate::main;

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

/// Define the stand and end markers for Limine requests.
#[used]
#[unsafe(link_section = ".requests_start_marker")]
static _START_MARKER: RequestsStartMarker = RequestsStartMarker::new();
#[used]
#[unsafe(link_section = ".requests_end_marker")]
static _END_MARKER: RequestsEndMarker = RequestsEndMarker::new();

pub struct BootInfo {
    pub framebuffer: Framebuffer<'static>,
}

#[unsafe(no_mangle)]
unsafe extern "C" fn kmain() -> ! {
    // All limine requests must also be referenced in a called function, otherwise they may be
    // removed by the linker.
    assert!(BASE_REVISION.is_supported());

    let framebuffer = FRAMEBUFFER_REQUEST
        .get_response()
        .expect("need a framebuffer")
        .framebuffers()
        .next()
        .expect("need a framebuffer");

    let boot_info = BootInfo { framebuffer };

    main(boot_info)
}
