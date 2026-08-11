use spin::RwLock;
use x86_64::{
    PhysAddr, VirtAddr,
    structures::paging::{PageTable, PageTableFlags},
};

use crate::boot::boot_info;

pub mod cow;
pub mod fault;
pub mod frame_allocator;
pub mod frame_drop;
pub mod mapper;
pub mod pat;
pub mod shared;
pub mod tlb;
pub mod valloc;
pub mod vma;

/// Allowlist of physical address ranges that userspace may map via MAP_PHYSICAL.
/// Each entry is (start, end) inclusive of start, exclusive of end.
static ALLOWED_PHYS_RANGES: RwLock<heapless::Vec<(u64, u64), 8>> =
    RwLock::new(heapless::Vec::new());

/// Register a physical address range as safe for userspace mapping.
pub fn allow_physical_range(start: u64, size: u64) {
    let mut ranges = ALLOWED_PHYS_RANGES.write();
    let _ = ranges.push((start, start + size));
}

/// Check whether the entire range [start, start+size) is within an allowed range.
pub fn is_physical_range_allowed(start: u64, size: u64) -> bool {
    let end = start + size;
    let ranges = ALLOWED_PHYS_RANGES.read();
    ranges
        .iter()
        .any(|&(r_start, r_end)| start >= r_start && end <= r_end)
}

// physical offset is at 0xffff_8000_0000_0000

pub const DYNAMIC_MEM_START: VirtAddr = VirtAddr::new_truncate(0xFFFF_C000_2000_0000);

// Early heap region
pub const KERNEL_HEAP: VirtAddr = VirtAddr::new_truncate(0xFFFF_C000_0000_0000);
// Ends at 0xFFFF_C000_1000_0000
pub const KERNEL_HEAP_SIZE: u64 = 1024 * 1024 * 1; // 1 mb

// Remember Debug rust builds take a lot of stack
pub const KTHREAD_STACK_SIZE: u64 = 1024 * 32; // 32kb
/// Size of stack region including guard page (total allocation per thread)
pub const KTHREAD_STACK_REGION_SIZE: u64 = KTHREAD_STACK_SIZE + 4096;

pub const USER_STACK_TOP: VirtAddr = VirtAddr::new_truncate(0x0000_7000_0000_0000);
pub const USER_STACK_SIZE: u64 = 1024 * 1024 * 8; // 8mb

/// Stack alignment requirement for FPU/SSE instructions (16 bytes)
pub const STACK_ALIGNMENT: u64 = 16;

/// Walk kernel-half page tables in [`KERNEL_HEAP`, `valloc::VMALLOC_END`) and
/// panic if any physical frame is mapped at two different virtual addresses.
///
/// Detects the class of bug where `allocate_frame` / `allocate_contiguous_frames`
/// hands out the same physical frame for two different callers (e.g. kernel
/// heap page + vmalloc-backed DMA buffer), which would allow one caller's
/// writes to corrupt the other. Skips the HHDM range (PML4 indices 256-383)
/// since those mappings legitimately alias every physical frame.
///
/// Expensive: walks all present kernel-half page-table entries. Use for
/// debugging only. Allocates a `Vec` of ~(phys, virt) tuples, so requires a
/// working heap.
pub fn verify_kernel_no_phys_aliasing() {
    use alloc::vec::Vec;
    use x86_64::registers::control::Cr3;

    let phys_off = boot_info().physical_memory_offset;
    let cr3_phys = Cr3::read().0.start_address();
    let pml4 = unsafe { &*(phys_off + cr3_phys.as_u64()).as_ptr::<PageTable>() };

    let mut mappings: Vec<(u64, u64)> = Vec::with_capacity(8192);

    // PML4 indices covering KERNEL_HEAP..VMALLOC_END:
    // 0xffffc00000000000 >> 39 = 384; 0xffffe00000000000 >> 39 = 448.
    for i in 384..448 {
        let entry = &pml4[i];
        if !entry.flags().contains(PageTableFlags::PRESENT) {
            continue;
        }
        let base_virt = ((i as u64) << 39) | 0xffff_0000_0000_0000;
        walk_page_table(entry, 3, base_virt, phys_off, &mut mappings);
    }

    mappings.sort_unstable_by_key(|&(phys, _)| phys);
    for pair in mappings.windows(2) {
        let (pa, va) = pair[0];
        let (pb, vb) = pair[1];
        if pa == pb {
            panic!(
                "verify_kernel_no_phys_aliasing: phys {:#x} mapped at two virts {:#x} and {:#x}",
                pa, va, vb
            );
        }
    }

    crate::println!(
        "verify_kernel_no_phys_aliasing: {} kernel-half mappings, no aliases",
        mappings.len()
    );
}

/// Recursive page-table walker. `level` = 3 (PDPT) / 2 (PD) / 1 (PT).
/// Records (phys, virt) for every 4 KiB leaf. Panics on huge pages (kernel
/// heap + vmalloc are expected to use 4 KiB only; if a huge page appears
/// here we need to handle it explicitly).
fn walk_page_table(
    parent_entry: &x86_64::structures::paging::page_table::PageTableEntry,
    level: u8,
    virt_base: u64,
    phys_off: VirtAddr,
    out: &mut alloc::vec::Vec<(u64, u64)>,
) {
    let table_virt = phys_off + parent_entry.addr().as_u64();
    let table = unsafe { &*table_virt.as_ptr::<PageTable>() };
    let shift: u64 = match level {
        3 => 30,
        2 => 21,
        1 => 12,
        _ => unreachable!("walk_page_table: bad level {}", level),
    };

    for (i, entry) in table.iter().enumerate() {
        if !entry.flags().contains(PageTableFlags::PRESENT) {
            continue;
        }
        let virt = virt_base + ((i as u64) << shift);

        if level == 1 {
            // 4 KiB leaf.
            out.push((entry.addr().as_u64(), virt));
        } else if entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            // Huge page (2 MiB at level 2, 1 GiB at level 3). Record each
            // 4 KiB sub-page so alias detection is uniform.
            let huge_size = 1u64 << shift;
            let phys_base = entry.addr().as_u64();
            for off in (0..huge_size).step_by(4096) {
                out.push((phys_base + off, virt + off));
            }
        } else {
            walk_page_table(entry, level - 1, virt, phys_off, out);
        }
    }
}

/// Mark every kernel-half leaf mapping `GLOBAL`, so a `CR3` reload keeps it.
///
/// A `CR3` write drops every non-global translation, and the kernel half is
/// the same in every address space — the same page tables, reached through
/// PML4 entries copied from the kernel's. Switching between two processes
/// therefore threw away the kernel's own translations along with the outgoing
/// process's, and every one of them was re-walked on the next syscall. The
/// `G` bit says "this one is not part of any address space", which is exactly
/// true here and is what the bit is for.
///
/// This is what PCID would otherwise buy on the kernel side. It is not a
/// substitute for PCID on the *user* side: a process's own translations are
/// genuinely per-address-space and still go.
///
/// Must run before [`enable_pge`] takes effect anywhere, though not urgently:
/// `G` is ignored while `CR4.PGE` is clear, and an entry cached before its bit
/// was set is simply re-walked after the next reload.
pub fn mark_kernel_mappings_global() {
    use x86_64::registers::control::Cr3;

    let phys_off = boot_info().physical_memory_offset;
    let cr3_phys = Cr3::read().0.start_address();
    let pml4 = unsafe { &mut *(phys_off + cr3_phys.as_u64()).as_mut_ptr::<PageTable>() };

    let mut marked = 0u64;
    // The whole higher half. Every entry in it is kernel-owned: user mappings
    // live below 0x0000_8000_0000_0000 and never reach index 256.
    for i in 256..512 {
        let entry = &mut pml4[i];
        if !entry.flags().contains(PageTableFlags::PRESENT) {
            continue;
        }
        mark_global(entry, 3, phys_off, &mut marked);
    }

    // Nothing is cached as global yet; make the CPU re-walk so it starts
    // seeing the bits set above.
    crate::smp::tlb_flush_all_including_global();
    crate::println!("mark_kernel_mappings_global: {marked} kernel-half leaves");
}

/// Set `GLOBAL` on every leaf under `parent_entry`. `level` = 3 / 2 / 1.
fn mark_global(
    parent_entry: &mut x86_64::structures::paging::page_table::PageTableEntry,
    level: u8,
    phys_off: VirtAddr,
    marked: &mut u64,
) {
    let table_virt = phys_off + parent_entry.addr().as_u64();
    let table = unsafe { &mut *table_virt.as_mut_ptr::<PageTable>() };

    for entry in table.iter_mut() {
        let flags = entry.flags();
        if !flags.contains(PageTableFlags::PRESENT) {
            continue;
        }
        // `GLOBAL` means nothing on a table entry, so only leaves are touched.
        if level == 1 || flags.contains(PageTableFlags::HUGE_PAGE) {
            if !flags.contains(PageTableFlags::GLOBAL) {
                entry.set_flags(flags | PageTableFlags::GLOBAL);
                *marked += 1;
            }
        } else {
            mark_global(entry, level - 1, phys_off, marked);
        }
    }
}

/// Enable `CR4.PGE`, which is what makes the `G` bit mean anything. Per-CPU.
pub fn enable_pge() {
    use x86_64::registers::control::{Cr4, Cr4Flags};

    let mut cr4 = Cr4::read();
    cr4.insert(Cr4Flags::PAGE_GLOBAL);
    unsafe { Cr4::write(cr4) };
}

/// Get the virtual address from the given physical address
///
/// This may not be mapped! check with translate
pub fn get_virt_addr_from_phys_offset(phys: PhysAddr) -> VirtAddr {
    boot_info().physical_memory_offset + phys.as_u64()
}
