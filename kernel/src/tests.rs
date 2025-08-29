use x86_64::structures::paging::FrameAllocator;

use crate::memory::frame_allocator::frame_allocator;

#[test_case]
fn test_basic_assertion() {
    assert_eq!(1 + 1, 2);
}

#[test_case]
fn test_frame_allocator() {
    // Test your frame allocator
    let frame = frame_allocator().allocate_frame();
    assert!(frame.is_some());
}

#[test_case]
fn test_serial_output() {
    serial_print!("test serial output...");
    // If we reach here without hanging, serial works
}
