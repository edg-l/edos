//! PAT (Page Attribute Table) MSR programming.
//!
//! PAT entry 1 is reprogrammed from WT (Write-Through) to WC (Write-Combining)
//! at boot. After this, `PageTableFlags::WRITE_THROUGH` (PWT=1, PCD=0) selects
//! PAT entry 1 = WC. Do NOT use `PageTableFlags::WRITE_THROUGH` anywhere in the
//! kernel expecting actual Write-Through semantics.
//!
//! Existing `NO_CACHE` (PCD=1, PWT=0) usage is unaffected: it selects PAT
//! entry 2 = UC-, which is unchanged.

use x86_64::structures::paging::PageTableFlags;

use crate::println;

const PAT_MSR: u32 = 0x277;

// Default Intel PAT MSR value:
//   Entry 0=WB(06), 1=WT(04), 2=UC-(07), 3=UC(00), 4-7 mirror
//   0x0007_0406_0007_0406

/// Target: entry 1 changed from WT(04) to WC(01)
const TARGET_PAT: u64 = 0x0007_0406_0007_0106;

/// Page table flag that selects Write-Combining after PAT reprogramming.
/// Sets PWT=1, PCD=0 which indexes PAT entry 1 (now WC).
pub const WRITE_COMBINING: PageTableFlags = PageTableFlags::WRITE_THROUGH;

fn cpuid_supports_pat() -> bool {
    let result = core::arch::x86_64::__cpuid(0x01);
    (result.edx & (1 << 16)) != 0
}

/// Program the PAT MSR to replace entry 1 (WT) with WC.
///
/// Uses a simplified sequence (WRMSR + TLB flush) that works under KVM.
/// Called at boot before any WC mappings exist, so no stale TLB entries
/// can conflict.
pub fn init_pat() {
    if !cpuid_supports_pat() {
        println!("PAT: not supported by CPU, skipping");
        return;
    }

    unsafe {
        // The full SDM Section 11.12 sequence (CR0.CD=1, WBINVD, etc.) causes
        // triple faults under KVM because setting CR0.CD disables caching on
        // the host core. On KVM, WRMSR to PAT is intercepted and handled
        // safely by the hypervisor. On bare metal at boot (before any WC
        // mappings exist), no stale TLB entries can conflict, so a simple
        // WRMSR + TLB flush suffices.
        core::arch::asm!(
            "wrmsr",
            "mov {tmp}, cr3",
            "mov cr3, {tmp}",
            "mfence",
            tmp = out(reg) _,
            in("ecx") PAT_MSR,
            in("edx") (TARGET_PAT >> 32) as u32,
            in("eax") TARGET_PAT as u32,
            options(nostack),
        );
    }

    // Verify readback
    let readback: u64;
    unsafe {
        let lo: u32;
        let hi: u32;
        core::arch::asm!(
            "rdmsr",
            in("ecx") PAT_MSR,
            out("eax") lo,
            out("edx") hi,
            options(nostack, preserves_flags),
        );
        readback = ((hi as u64) << 32) | (lo as u64);
    }

    if readback == TARGET_PAT {
        println!("PAT: entry 1 set to WC (0x{:016x})", readback);
    } else {
        println!(
            "PAT: WARNING unexpected value 0x{:016x} (expected 0x{:016x})",
            readback, TARGET_PAT
        );
    }
}
