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

    /// Whether a copy on this CPU has a fault fixup armed.
    ///
    /// The page-fault handler asks this before treating a ring-0 fault on a
    /// user address as a kernel bug: an armed resume point means the fault
    /// belongs to a copy that is prepared to fail.
    #[inline]
    pub fn is_active(&self) -> bool {
        self.fault_resume.load(Ordering::Relaxed) != 0
    }

    /// Disarm the fixup, so a later ring-0 fault on a user address is the bug
    /// it looks like rather than a jump into a copy that has already finished.
    #[inline]
    pub fn clear(&self) {
        self.fault_resume.store(0, Ordering::Relaxed);
    }
}

/// The fault-fixup state of the CPU the caller is running on.
///
/// Per-CPU rather than per-thread, so the result stops being about the caller
/// the moment it can migrate. Every user of this either has interrupts off or
/// finishes inside one instruction of reading it.
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
    // SAFETY: `out` is a live local `u64`, so the destination is valid for the
    // 8 bytes asked for and aligned. The source was bounds-checked by
    // `access_ok` and alignment-checked above, and any fault on it takes the
    // fixup path rather than dereferencing garbage.
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

/// Copy `size` bytes with a fault fixup armed, answering false if either side
/// faulted.
///
/// The copy is a byte loop in assembly on purpose: each access is its own
/// instruction, so the fault handler can rewrite RIP to the landing pad at `5:`
/// knowing exactly which instruction faulted and that nothing is half-written
/// beyond the byte in flight.
///
/// # Safety
///
/// - `dst` and `src` must each be valid for `size` bytes, or be a user address
///   whose fault the armed fixup will catch;
/// - the caller must not be holding a lock or state that the fixup path skips
///   over, since a fault jumps straight to the landing pad.
#[inline(never)]
unsafe fn do_user_copy(dst: *mut u8, src: *const u8, size: usize) -> bool {
    if size == 0 {
        return true;
    }

    let mut result: u64 = 1; // 1 = success, 0 = fault

    // SAFETY: the caller guarantees both ranges. The asm names every register
    // it clobbers -- rax, rcx and rdx as outputs, rsi and rdi as inputs -- and
    // `nostack` holds because the only push is popped before the copy begins.
    // A fault inside the loop lands on `5:`, which falls through to the same
    // `clear_resume` call the success path takes, so the fixup is always
    // disarmed on the way out.
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

/// Hands the copy loop the address to store its resume label in.
///
/// # Safety
/// Called only from [`do_user_copy`]'s asm, which is what guarantees the
/// returned pointer is used before this CPU can be left.
#[inline(never)]
unsafe extern "C" fn setup_fault_resume() -> *mut AtomicU64 {
    let uaccess = current_cpu_uaccess();
    ptr::addr_of!(uaccess.fault_resume) as *mut AtomicU64
}

/// Disarms the fixup once the copy loop has finished or faulted.
///
/// # Safety
/// Called only from [`do_user_copy`]'s asm, on the same CPU that armed it.
#[inline(never)]
unsafe extern "C" fn clear_fault_resume() {
    let uaccess = current_cpu_uaccess();
    uaccess.clear();
}

/// Copy `size` bytes in from user space, answering false if the user side was
/// not there.
///
/// The user pointer is checked here rather than trusted: null and anything
/// outside the user half are rejected before the copy, for the reason
/// [`access_ok`] gives. What is *not* checked is the kernel side.
///
/// # Safety
///
/// - `dst` must be valid for writes of `size` bytes;
/// - `size` must not exceed the destination buffer.
#[inline]
pub unsafe fn try_copy_from_user(dst: *mut u8, src: *const u8, size: usize) -> bool {
    if src.is_null() || dst.is_null() || !access_ok(src as u64, size) {
        return false;
    }

    // SAFETY: `src` is non-null and inside the user half, so a fault on it is
    // caught by the fixup. `dst` is valid for `size` bytes by this function's
    // own contract.
    unsafe { do_user_copy(dst, src, size) }
}

/// Copy `size` bytes out to user space, answering false if the user side was
/// not there.
///
/// The user pointer is checked here, as in [`try_copy_from_user`]; the kernel
/// side is the caller's.
///
/// # Safety
///
/// - `src` must be valid for reads of `size` bytes;
/// - `size` must not exceed the source buffer.
#[inline]
pub unsafe fn try_copy_to_user(dst: *mut u8, src: *const u8, size: usize) -> bool {
    if src.is_null() || dst.is_null() || !access_ok(dst as u64, size) {
        return false;
    }

    // SAFETY: `dst` is non-null and inside the user half, so a fault on it is
    // caught by the fixup. `src` is valid for `size` bytes by this function's
    // own contract.
    unsafe { do_user_copy(dst, src, size) }
}

/// Copy a NUL-terminated user string, answering the length without the
/// terminator.
///
/// One byte at a time, and deliberately: the length is not known before the
/// copy, so a bulk copy would have to read past the string to find its end and
/// could fault on a page the string never touched. A string with no terminator
/// inside `max_len` is [`UAccessError::TooLong`], distinct from a fault, so the
/// caller can answer `ENAMETOOLONG` rather than `EFAULT`.
///
/// # Safety
///
/// - `dst` must be valid for writes of `max_len` bytes.
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
        // SAFETY: the destination is a live local. `src.add(len)` stays inside
        // the range `access_ok` will vet on the next line, since `len` is below
        // `max_len` and the call rejects anything that leaves the user half --
        // so an overflowing offset fails the copy rather than dereferencing.
        if !unsafe { try_copy_from_user(&mut byte as *mut u8, src.add(len), 1) } {
            return Err(UAccessError::Fault);
        }

        if byte == 0 {
            return Ok(len);
        }

        // SAFETY: `len < max_len`, and the caller guarantees `dst` is valid for
        // `max_len` bytes.
        unsafe { dst.add(len).write(byte) };
    }

    // String too long - no null terminator found
    Err(UAccessError::TooLong)
}

/// Read one `T` out of user space, or `None` if the address was not there.
///
/// # Safety
/// Every byte pattern of `T` must be a valid `T`, because user space chooses
/// them: this is for plain integers and `#[repr(C)]` structs of them, never for
/// a type with a niche, an enum discriminant or a reference in it.
#[inline]
pub unsafe fn try_read_user<T: Copy>(src: *const T) -> Option<T> {
    // SAFETY: the caller guarantees every bit pattern of `T` is valid, so all
    // zeroes is one of them.
    let mut value: T = unsafe { core::mem::zeroed() };
    // SAFETY: the destination is a live local `T`, so it is valid and aligned
    // for exactly `size_of::<T>()` bytes. `src` is only read through the fixup.
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

/// Write one `T` into user space, answering false if the address was not there.
///
/// # Safety
/// `T` must have no padding whose contents would leak kernel memory to the
/// process, since the whole `size_of::<T>()` bytes are copied out.
#[inline]
pub unsafe fn try_write_user<T: Copy>(dst: *mut T, value: T) -> bool {
    // SAFETY: the source is a live local `T`, valid and aligned for exactly
    // `size_of::<T>()` bytes. `dst` is vetted and fixed up by the callee.
    unsafe {
        try_copy_to_user(
            dst as *mut u8,
            &value as *const T as *const u8,
            core::mem::size_of::<T>(),
        )
    }
}
