//! Safe kernel-user data copying with fault handling
//!
//! This module provides safe mechanisms for copying data between kernel and user space
//! with proper page fault handling and recovery.

use crate::{memory::vma::USER_VA_END, util::per_cpu::get_percpu_data};
use core::{
    ptr,
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UAccessError {
    Fault,
    TooLong,
}

/// Per-CPU user access state
///
/// Tracks the current user access operation and provides a fault resume point
/// if a page fault occurs during user memory access.
#[derive(Debug)]
pub struct UAccessState {
    /// Resume label for page faults during user access
    /// If non-zero, page faults will resume execution at this address
    pub fault_resume: AtomicU64,
    /// Nesting depth of [`NoFaultGuard`]. While non-zero, a ring-0 fault on a
    /// user address takes the resume path immediately instead of being
    /// demand-paged.
    pub nofault: AtomicU64,
}

impl UAccessState {
    pub const fn new() -> Self {
        Self {
            fault_resume: AtomicU64::new(0),
            nofault: AtomicU64::new(0),
        }
    }

    /// Whether faults on this CPU must fail rather than be serviced.
    #[inline]
    pub fn is_nofault(&self) -> bool {
        self.nofault.load(Ordering::Relaxed) != 0
    }

    /// Check if we're currently in a user access operation
    #[inline]
    pub fn is_active(&self) -> bool {
        self.fault_resume.load(Ordering::Relaxed) != 0
    }

    /// Clear the fault resume point
    #[inline]
    pub fn clear(&self) {
        self.fault_resume.store(0, Ordering::Relaxed);
    }
}

/// Get a mutable reference to the current CPU's user access state
#[inline]
pub fn current_cpu_uaccess() -> &'static UAccessState {
    &get_percpu_data().uaccess
}

/// Makes a user access on this CPU *fail* on a missing page instead of
/// servicing it.
///
/// A ring-0 fault on a user address is normally demand-paged, and
/// [`crate::interrupts::idt`] re-enables interrupts to do it because filling a
/// page legitimately blocks. That is the right answer on a syscall path and
/// the wrong one anywhere blocking is forbidden: an interrupt handler reading
/// a user stack would park inside the tick.
///
/// While this guard is held such a fault takes the `fault_resume` path at
/// once, so the copy reports a fault and the caller decides what a missing
/// page means. This is `pagefault_disable()` under another name; it applies to
/// the CPU rather than the thread, so nothing may sleep or migrate while it is
/// held. Take it with interrupts already off.
pub struct NoFaultGuard {
    _private: (),
}

impl NoFaultGuard {
    pub fn new() -> Self {
        current_cpu_uaccess()
            .nofault
            .fetch_add(1, Ordering::Relaxed);
        Self { _private: () }
    }
}

impl Default for NoFaultGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for NoFaultGuard {
    fn drop(&mut self) {
        current_cpu_uaccess()
            .nofault
            .fetch_sub(1, Ordering::Relaxed);
    }
}

/// Read one `u64` from user memory, failing rather than faulting a page in.
///
/// Returns `None` when the address is outside the user half, is misaligned, or
/// is not currently mapped. Safe to call from an interrupt handler; see
/// [`NoFaultGuard`].
pub fn read_u64_nofault(addr: u64) -> Option<u64> {
    if !addr.is_multiple_of(8) || !access_ok(addr, 8) {
        return None;
    }
    let mut out: u64 = 0;
    let _nofault = NoFaultGuard::new();
    let ok = unsafe {
        do_user_copy(
            core::ptr::addr_of_mut!(out).cast::<u8>(),
            addr as *const u8,
            8,
        )
    };
    ok.then_some(out)
}

/// True when a caller-supplied range lies entirely in the user half of the
/// address space.
///
/// The fault fixup below only rescues a copy that faults; it does not make an
/// arbitrary address safe to dereference. Two ranges have to be rejected before
/// the copy rather than during it:
///
/// - an address in the kernel half is canonical and mapped, so the copy would
///   succeed and hand kernel memory to the caller;
/// - an address inside the non-canonical hole raises #GP, not #PF.
#[inline]
pub fn access_ok(addr: u64, len: usize) -> bool {
    match addr.checked_add(len as u64) {
        Some(end) => end <= USER_VA_END,
        None => false,
    }
}

/// Low-level copy with fault handling
///
/// This is the core implementation that sets up fault recovery and performs the copy.
/// The page fault handler will check if fault_resume is set and jump to it on fault.
///
/// Returns true on success, false on fault.
///
/// # Safety
///
/// - `dst` and `src` must be valid for `size` bytes
/// - Caller must ensure proper alignment if needed
#[inline(never)]
unsafe fn do_user_copy(dst: *mut u8, src: *const u8, size: usize) -> bool {
    if size == 0 {
        return true;
    }

    let mut result: u64 = 1; // 1 = success, 0 = fault

    // We need assembly for precise control over the fault point
    // The fault handler will modify RIP to jump to the fault label
    unsafe {
        core::arch::asm!(
            // Get the fault resume address (label 5f = fault)
            "lea {tmp}, [rip + 5f]",

            // Set up fault handler: call setup function and store resume address
            "push {tmp}",             // Save resume address on stack
            "call {setup_resume}",    // Get UAccessState pointer in rax
            "pop rdx",                // Get resume address back
            "mov qword ptr [rax], rdx", // Store in uaccess.fault_resume

            // Perform the copy byte-by-byte with volatile reads/writes
            // This ensures each access is a separate instruction that can fault
            "xor rcx, rcx",          // rcx = index
            "2:",                     // loop label
            "cmp rcx, {size}",
            "jae 9f",                 // if index >= size, done (jump to cleanup)

            "mov al, byte ptr [rsi + rcx]",  // Read from src (may fault)
            "mov byte ptr [rdi + rcx], al",  // Write to dst (may fault)

            "inc rcx",
            "jmp 2b",

            // Fault landing point (5)
            "5:",
            "mov {result}, 0",        // Set result to fault (false)

            // Success/cleanup (9)
            "9:",
            "call {clear_resume}",    // Clear the fault resume

            setup_resume = sym setup_fault_resume,
            clear_resume = sym clear_fault_resume,
            tmp = out(reg) _,
            result = inout(reg) result,
            size = in(reg) size,
            in("rsi") src,
            in("rdi") dst,
            out("rax") _,
            out("rcx") _,
            out("rdx") _,
            options(nostack),
        );
    }

    result != 0
}

/// Setup helper - called from assembly
/// Returns a pointer to the current CPU's UAccessState
#[inline(never)]
unsafe extern "C" fn setup_fault_resume() -> *mut AtomicU64 {
    let uaccess = current_cpu_uaccess();
    ptr::addr_of!(uaccess.fault_resume) as *mut AtomicU64
}

/// Clear helper - called from assembly
#[inline(never)]
unsafe extern "C" fn clear_fault_resume() {
    let uaccess = current_cpu_uaccess();
    uaccess.clear();
}

/// Try to copy data from user space to kernel space
///
/// This function attempts to copy `size` bytes from user space address `src`
/// to kernel space address `dst`. If a page fault occurs during the copy,
/// the operation is aborted and false is returned.
///
/// # Safety
///
/// - `src` must point to a valid user space address range of `size` bytes
/// - `dst` must point to a valid kernel space address with sufficient space
/// - `size` must not exceed the size of either buffer
#[inline]
pub unsafe fn try_copy_from_user(dst: *mut u8, src: *const u8, size: usize) -> bool {
    if src.is_null() || dst.is_null() || !access_ok(src as u64, size) {
        return false;
    }

    unsafe { do_user_copy(dst, src, size) }
}

/// Try to copy data from kernel space to user space
///
/// This function attempts to copy `size` bytes from kernel space address `src`
/// to user space address `dst`. If a page fault occurs during the copy,
/// the operation is aborted and false is returned.
///
/// # Safety
///
/// - `src` must point to a valid kernel space address range of `size` bytes
/// - `dst` must point to a valid user space address with sufficient space
/// - `size` must not exceed the size of either buffer
#[inline]
pub unsafe fn try_copy_to_user(dst: *mut u8, src: *const u8, size: usize) -> bool {
    if src.is_null() || dst.is_null() || !access_ok(dst as u64, size) {
        return false;
    }

    unsafe { do_user_copy(dst, src, size) }
}

/// Copy a C string from user space to kernel space
///
/// Copies a null-terminated string from user space to a kernel buffer.
/// Returns the number of bytes copied (excluding null terminator) on success.
/// On failure, distinguishes between memory faults and strings exceeding
/// `max_len`.
///
/// # Safety
///
/// - `src` must point to a valid null-terminated string in user space
/// - `dst` must point to a valid kernel buffer of at least `max_len` bytes
pub unsafe fn try_copy_string_from_user(
    dst: *mut u8,
    src: *const u8,
    max_len: usize,
) -> Result<usize, UAccessError> {
    if src.is_null() || dst.is_null() || max_len == 0 {
        return Err(UAccessError::Fault);
    }

    for len in 0..max_len {
        let mut byte: u8 = 0;
        if !unsafe { try_copy_from_user(&mut byte as *mut u8, src.add(len), 1) } {
            return Err(UAccessError::Fault);
        }

        if byte == 0 {
            return Ok(len);
        }

        unsafe { dst.add(len).write(byte) };
    }

    // String too long - no null terminator found
    Err(UAccessError::TooLong)
}

/// Read a single value from user space
///
/// # Safety
///
/// - `src` must point to a valid user space address containing a value of type T
/// - T must be Copy and have a valid bit pattern for all possible byte values
#[inline]
pub unsafe fn try_read_user<T: Copy>(src: *const T) -> Option<T> {
    let mut value: T = unsafe { core::mem::zeroed() };
    if unsafe {
        try_copy_from_user(
            &mut value as *mut T as *mut u8,
            src as *const u8,
            core::mem::size_of::<T>(),
        )
    } {
        Some(value)
    } else {
        None
    }
}

/// Write a single value to user space
///
/// # Safety
///
/// - `dst` must point to a valid user space address with space for a value of type T
/// - T must be Copy
#[inline]
pub unsafe fn try_write_user<T: Copy>(dst: *mut T, value: T) -> bool {
    unsafe {
        try_copy_to_user(
            dst as *mut u8,
            &value as *const T as *const u8,
            core::mem::size_of::<T>(),
        )
    }
}
