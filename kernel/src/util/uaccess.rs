//! Safe kernel-user data copying with fault handling
//!
//! This module provides safe mechanisms for copying data between kernel and user space
//! with proper page fault handling and recovery.

use crate::util::per_cpu::get_percpu_data;

/// Per-CPU user access state
///
/// Tracks the current user access operation and provides a fault resume point
/// if a page fault occurs during user memory access.
#[derive(Debug, Clone, Copy)]
pub struct UAccessState {
    /// Resume label for page faults during user access
    /// If non-zero, page faults will resume execution at this address
    pub fault_resume: u64,
}

impl UAccessState {
    pub const fn new() -> Self {
        Self { fault_resume: 0 }
    }

    /// Check if we're currently in a user access operation
    #[inline]
    pub fn is_active(&self) -> bool {
        self.fault_resume != 0
    }

    /// Set the fault resume point
    #[inline]
    pub fn set_resume(&mut self, resume: u64) {
        self.fault_resume = resume;
    }

    /// Clear the fault resume point
    #[inline]
    pub fn clear(&mut self) {
        self.fault_resume = 0;
    }
}

/// RAII guard for user access operations
///
/// Automatically sets up and tears down the fault resume point.
/// While this guard is active, page faults during user memory access
/// will be caught and handled gracefully.
pub struct UAccessGuard {
    _private: (),
}

impl UAccessGuard {
    /// Create a new user access guard
    ///
    /// This sets up the fault handler to catch page faults during user access.
    /// The guard must be held for the duration of the user access operation.
    #[inline]
    pub fn new(resume_addr: u64) -> Self {
        let uaccess = current_cpu_uaccess();
        uaccess.set_resume(resume_addr);
        Self { _private: () }
    }
}

impl Drop for UAccessGuard {
    #[inline]
    fn drop(&mut self) {
        let uaccess = current_cpu_uaccess();
        uaccess.clear();
    }
}

/// Get a mutable reference to the current CPU's user access state
///
/// This is stored in the per-CPU data structure and accessed via GS base.
#[inline]
pub fn current_cpu_uaccess() -> &'static mut UAccessState {
    // For now, we'll use a static per-CPU array indexed by APIC ID
    // This could be integrated into PerCpuData later
    static mut UACCESS_STATES: [UAccessState; 256] = [UAccessState::new(); 256];

    let percpu = get_percpu_data();
    let apic_id = percpu.lapic_id as usize;

    unsafe { &mut UACCESS_STATES[apic_id] }
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
unsafe extern "C" fn setup_fault_resume() -> *mut UAccessState {
    let uaccess = current_cpu_uaccess();
    uaccess as *mut UAccessState
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
    if src.is_null() || dst.is_null() {
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
    if src.is_null() || dst.is_null() {
        return false;
    }

    unsafe { do_user_copy(dst, src, size) }
}

/// Copy a C string from user space to kernel space
///
/// Copies a null-terminated string from user space to a kernel buffer.
/// Returns the number of bytes copied (excluding null terminator) on success,
/// or None if a fault occurred or the string exceeds max_len.
///
/// # Safety
///
/// - `src` must point to a valid null-terminated string in user space
/// - `dst` must point to a valid kernel buffer of at least `max_len` bytes
pub unsafe fn try_copy_string_from_user(
    dst: *mut u8,
    src: *const u8,
    max_len: usize,
) -> Option<usize> {
    if src.is_null() || dst.is_null() || max_len == 0 {
        return None;
    }

    let mut len = 0;

    for i in 0..max_len {
        let mut byte: u8 = 0;
        if !unsafe { try_copy_from_user(&mut byte as *mut u8, src.add(i), 1) } {
            return None;
        }

        if byte == 0 {
            return Some(len);
        }

        unsafe { dst.add(i).write(byte) };
        len += 1;
    }

    // String too long - no null terminator found
    None
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
