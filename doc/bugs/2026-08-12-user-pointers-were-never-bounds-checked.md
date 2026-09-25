# User pointers were never bounds-checked, only fault-fixed up

## Status

Fixed in `f10e0d66`. `uaccess::access_ok` (`kernel/src/util/uaccess.rs`)
requires `addr + len <= USER_VA_END` with a checked add, on the user side of
both copy directions. `general_protection_fault_handler`
(`kernel/src/interrupts/idt.rs`) honours the same `fault_resume` fixup as the
page fault handler.

Left standing: every exception handler in `interrupts/idt.rs` calls `eoi()` on
entry. No interrupt is in service during a fault, so a fault landing inside an
interrupt handler would clear an unrelated ISR bit. Not reachable from uaccess,
which never runs in interrupt context.

## Symptoms

`syscallfuzz -n 4 -u 0` panicked with a ring-0 General Protection Fault inside
`do_user_copy`, reached from `sys_pipe`. The pointer was
`0x0000_8000_0000_0000`, one of the fuzzer's poison values.

The second class had no symptom at all, which is worse: a kernel-half address
such as `0xffff_ffff_8000_0000` copied successfully. `read(fd, kaddr, n)`
overwrote kernel memory and `write(fd, kaddr, n)` handed it to userspace.

## Root cause

`try_copy_from_user` and `try_copy_to_user` validated only null and left every
other address to the fault fixup. The fixup was wired into the page fault
handler alone, so two classes of address went through:

- A non-canonical address raises #GP, not #PF. The #GP handler had no fixup,
  so its ring-0 arm panicked on a pointer any program can pass.
- A kernel-half address is canonical and mapped, so no fault happens and the
  copy succeeds.

The fix checks the range before the copy. Every other uaccess entry point
(`try_read_user`, `try_write_user`, `try_copy_string_from_user`) funnels
through the two copy functions, and every caller passes a pointer that came
from a syscall argument, so no in-kernel caller needs the kernel half.

## Reasoning rules going forward

- A fault fixup is a backstop, not validation. Check the range first; the
  fixup covers what the check could not know, such as an unmapped user page.
- A declared length is the caller's claim about its own buffer. The check
  covers `addr + len`, not only the bytes actually written.
- The dangerous failure is the silent success. Only a test that passes a
  kernel-half pointer catches it; `syscallfuzz`'s poison set carries one.

## If this reappears

1. A ring-0 #GP or #PF inside `do_user_copy` means an address reached the copy
   without `access_ok`. Find the entry point that skipped it.
2. A uaccess copy that faults for a reason the checks did not anticipate now
   returns failure instead of panicking; `strace` shows it as `EFAULT`.
3. Run `syscallfuzz -u 0` against the syscall in question; its poison values
   include the first non-canonical address and a kernel-half address.
