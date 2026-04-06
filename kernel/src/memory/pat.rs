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
/// Follows the Intel SDM Vol. 3A Section 11.12 sequence:
/// 1. Disable caching (CR0.CD=1)
/// 2. Flush caches (WBINVD)
/// 3. Flush TLBs (reload CR3)
/// 4. Write PAT MSR
/// 5. Flush caches again
/// 6. Flush TLBs again
/// 7. Re-enable caching (CR0.CD=0)
pub fn init_pat() {
    if !cpuid_supports_pat() {
        println!("PAT: not supported by CPU, skipping");
        return;
    }

    unsafe {
        core::arch::asm!(
            // 1. Disable caching: set CR0.CD (bit 30)
            "mov {tmp}, cr0",
            "or {tmp}, {cd_bit}",
            "mov cr0, {tmp}",
            // 2. Flush all caches
            "wbinvd",
            // 3. Flush TLBs by reloading CR3
            "mov {tmp}, cr3",
            "mov cr3, {tmp}",
            // 4. Write PAT MSR
            "wrmsr",
            // 5. Flush caches again
            "wbinvd",
            // 6. Flush TLBs again
            "mov {tmp}, cr3",
            "mov cr3, {tmp}",
            // 7. Re-enable caching: clear CR0.CD
            "mov {tmp}, cr0",
            "and {tmp}, {cd_clear}",
            "mov cr0, {tmp}",
            // 8. Serialize
            "mfence",
            tmp = out(reg) _,
            cd_bit = const 1u64 << 30,
            cd_clear = const !(1u64 << 30),
            in("ecx") PAT_MSR,
            in("edx") (TARGET_PAT >> 32) as u32,
            in("eax") TARGET_PAT as u32,
            options(nostack, preserves_flags),
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
