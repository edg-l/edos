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

use crate::{memory::mapper::align_stack_pointer, util::per_cpu::get_percpu_data};

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;
pub const RING3_STACK_PST_INDEX: u16 = 0;

fn init_tss_for_current_cpu() {
    let mut tss = TaskStateSegment::new();

    let double_fault_stack_end = {
        let layout = Layout::from_size_align(1024 * 32, 4096).unwrap();
        // SAFETY: the layout is built from a non-zero size and a power-of-two
        // alignment, which is all `alloc` asks of it. The stack is never freed:
        // the CPU switches to it on a double fault for the rest of the boot.
        let stack_start = unsafe { alloc(layout) };

        if stack_start.is_null() {
            handle_alloc_error(layout)
        }

        // SAFETY: the null case returned above, so the allocation is live and
        // `layout.size()` bytes long; one past its end is the address a stack
        // grows down from, and computing it is in bounds for this rule.
        VirtAddr::from_ptr(unsafe { stack_start.byte_add(layout.size()) })
    };

    // Stack used in user space.
    let ring3_pst_stack_end = {
        let layout = Layout::from_size_align(4096, 4096).unwrap();
        // SAFETY: as for the double-fault stack; the layout is valid and the
        // allocation is leaked deliberately.
        let stack_start = unsafe { alloc(layout) };

        if stack_start.is_null() {
            handle_alloc_error(layout)
        }
        // SAFETY: as above, one past the end of a live `layout.size()` allocation.
        let stack_top = VirtAddr::from_ptr(unsafe { stack_start.byte_add(layout.size()) });
        align_stack_pointer(stack_top)
    };

    tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = double_fault_stack_end;
    tss.privilege_stack_table[RING3_STACK_PST_INDEX as usize] = ring3_pst_stack_end;

    let pcpu = get_percpu_data();
    // SAFETY: `tss_mut` wants the caller on the CPU the data belongs to,
    // holding no other reference into the same TSS and not outliving a context
    // switch. This runs once per CPU from `init_current_cpu` during bring-up,
    // before that CPU has a scheduler to be switched away by, and the reference
    // dies at the end of the statement.
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
    // SAFETY: `tss_ptr` points at this CPU's `PerCpuData`, which is leaked for
    // the life of the boot, and `init_tss_for_current_cpu` filled it in a few
    // lines above. That outlives the GDT entry the descriptor is appended to,
    // which is what the `_unchecked` in the name is about.
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

    // SAFETY: the GDT loaded on the line above is leaked, so it stays mapped
    // for as long as these selectors index it, and the four descriptors were
    // appended to it in the order these selectors were taken from. CS is
    // reloaded with a kernel code segment and SS with a kernel data segment,
    // which is the state the interrupt entry code and `syscall_entry` are
    // written against.
    unsafe {
        CS::set_reg(sels.code_selector);
        SS::set_reg(sels.data_selector);
        load_tss(sels.tss_selector);
    }

    // Publish selectors once (they are identical across CPUs)
    SELECTORS.call_once(|| sels);
}
