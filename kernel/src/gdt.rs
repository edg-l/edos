use core::alloc::Layout;

use alloc::alloc::{alloc, handle_alloc_error};
use spin::Lazy;
use x86_64::{
    VirtAddr,
    instructions::tables::load_tss,
    registers::segmentation::{CS, SS, Segment},
    structures::{
        gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector},
        tss::TaskStateSegment,
    },
};

use crate::{memory::mapper::align_stack_pointer, serial_println};

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;
pub const PAGE_FAULT_IST_INDEX: u16 = 1;
pub const RING3_STACK_PST_INDEX: u16 = 0;

pub static TSS: spin::Lazy<TaskStateSegment> = Lazy::new(|| {
    let mut tss = TaskStateSegment::new();

    let page_fault_stack_end = {
        let layout = Layout::from_size_align(4096, 4096).unwrap();
        let stack_start = unsafe { alloc(layout) };

        if stack_start.is_null() {
            handle_alloc_error(layout)
        }

        serial_println!("Created page fault stack at : {:p}", unsafe {
            stack_start.byte_add(layout.size())
        });

        VirtAddr::from_ptr(unsafe { stack_start.byte_add(layout.size()) })
    };

    let double_fault_stack_end = {
        let layout = Layout::from_size_align(4096, 4096).unwrap();
        let stack_start = unsafe { alloc(layout) };

        if stack_start.is_null() {
            handle_alloc_error(layout)
        }

        serial_println!("Created double fault stack at : {:p}", unsafe {
            stack_start.byte_add(layout.size())
        });

        VirtAddr::from_ptr(unsafe { stack_start.byte_add(layout.size()) })
    };

    // Stack used in user space.
    let ring3_pst_stack_end = {
        let layout = Layout::from_size_align(4096, 4096).unwrap();
        let stack_start = unsafe { alloc(layout) };

        if stack_start.is_null() {
            handle_alloc_error(layout)
        }
        let stack_top = VirtAddr::from_ptr(unsafe { stack_start.byte_add(layout.size()) });
        align_stack_pointer(stack_top)
    };

    tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = double_fault_stack_end;
    tss.interrupt_stack_table[PAGE_FAULT_IST_INDEX as usize] = page_fault_stack_end;
    tss.privilege_stack_table[RING3_STACK_PST_INDEX as usize] = ring3_pst_stack_end;

    tss
});

pub static GDT: spin::Lazy<(GlobalDescriptorTable, GdtSelectors)> = Lazy::new(|| {
    let mut gdt = GlobalDescriptorTable::new();

    let code_selector = gdt.append(Descriptor::kernel_code_segment());
    let data_selector = gdt.append(Descriptor::kernel_data_segment());
    let user_data_selector = gdt.append(Descriptor::user_data_segment());
    let user_code_selector = gdt.append(Descriptor::user_code_segment());
    let tss_selector = gdt.append(Descriptor::tss_segment(&TSS));

    (
        gdt,
        GdtSelectors {
            code_selector,
            data_selector,
            tss_selector,
            user_code_selector,
            user_data_selector,
        },
    )
});

#[expect(unused)]
#[derive(Debug, Clone, Copy)]
pub struct GdtSelectors {
    pub code_selector: SegmentSelector,
    pub data_selector: SegmentSelector,
    pub user_code_selector: SegmentSelector,
    pub user_data_selector: SegmentSelector,
    pub tss_selector: SegmentSelector,
}

pub fn init() {
    GDT.0.load();

    unsafe {
        CS::set_reg(GDT.1.code_selector);
        SS::set_reg(GDT.1.data_selector);
        load_tss(GDT.1.tss_selector);
    }
}
