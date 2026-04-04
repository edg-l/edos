use core::alloc::Layout;

use alloc::{
    alloc::{alloc, handle_alloc_error},
    boxed::Box,
};
use spin::Once;
use x86_64::{
    VirtAddr,
    instructions::tables::load_tss,
    registers::segmentation::{CS, SS, Segment},
    structures::{
        gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector},
        tss::TaskStateSegment,
    },
};

use crate::{memory::mapper::align_stack_pointer, println, util::per_cpu::get_percpu_data};

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;
pub const PAGE_FAULT_IST_INDEX: u16 = 1;
pub const RING3_STACK_PST_INDEX: u16 = 0;

fn init_tss_for_current_cpu() {
    let mut tss = TaskStateSegment::new();

    let page_fault_stack_end = {
        let layout = Layout::from_size_align(1024 * 32, 4096).unwrap();
        let stack_start = unsafe { alloc(layout) };

        if stack_start.is_null() {
            handle_alloc_error(layout)
        }

        println!("Created page fault stack at : {:p}", unsafe {
            stack_start.byte_add(layout.size())
        });

        VirtAddr::from_ptr(unsafe { stack_start.byte_add(layout.size()) })
    };

    let double_fault_stack_end = {
        let layout = Layout::from_size_align(1024 * 32, 4096).unwrap();
        let stack_start = unsafe { alloc(layout) };

        if stack_start.is_null() {
            handle_alloc_error(layout)
        }

        println!("Created double fault stack at : {:p}", unsafe {
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

    let pcpu = get_percpu_data();
    unsafe { *pcpu.tss_mut() = tss };
}

// Global, CPU-independent view of segment selectors. All per-CPU GDTs are
// constructed identically so selectors are consistent across CPUs.
static SELECTORS: Once<GdtSelectors> = Once::new();

#[derive(Debug, Clone, Copy)]
pub struct GdtSelectors {
    pub code_selector: SegmentSelector,
    pub data_selector: SegmentSelector,
    pub user_code_selector: SegmentSelector,
    pub user_data_selector: SegmentSelector,
    pub tss_selector: SegmentSelector,
}

/// Returns the architecture-wide selectors corresponding to the standard
/// descriptor layout. Valid after the BSP has called `init_current_cpu()`.
pub fn selectors() -> &'static GdtSelectors {
    SELECTORS.get().expect("GDT selectors not initialized")
}

/// Initialize and load a GDT for the current CPU, tying the TSS descriptor to
/// this CPU’s per-CPU TSS. Must be called once per CPU during bring-up.
pub fn init_current_cpu() {
    // Build per-CPU TSS and IST stacks
    init_tss_for_current_cpu();

    // Construct a fresh GDT for this CPU
    let mut gdt = GlobalDescriptorTable::new();

    // Keep descriptor order identical across CPUs to maintain stable selectors
    let code_selector = gdt.append(Descriptor::kernel_code_segment());
    let data_selector = gdt.append(Descriptor::kernel_data_segment());
    let user_data_selector = gdt.append(Descriptor::user_data_segment());
    let user_code_selector = gdt.append(Descriptor::user_code_segment());
    let tss_ref = get_percpu_data().tss_ptr();
    let tss_selector = gdt.append(unsafe { Descriptor::tss_segment_unchecked(tss_ref) });

    let sels = GdtSelectors {
        code_selector,
        data_selector,
        tss_selector,
        user_code_selector,
        user_data_selector,
    };

    // Leak the GDT to keep it alive after load (required by CPU)
    let gdt_static: &'static mut GlobalDescriptorTable = Box::leak(Box::new(gdt));
    gdt_static.load();

    unsafe {
        CS::set_reg(sels.code_selector);
        SS::set_reg(sels.data_selector);
        load_tss(sels.tss_selector);
    }

    // Publish selectors once (they are identical across CPUs)
    SELECTORS.call_once(|| sels);
}
