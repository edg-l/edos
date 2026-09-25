# An `ioctl` on a closed descriptor wedged its own CPU

## Status

Fixed in `4b45c8c1`. `sys_ioctl` (`kernel/src/syscalls/ioctl/mod.rs`) binds the
descriptor lookup to a `let` before matching on it, and the comment above
`fd_table` there says why. Errno is now set centrally from the `Errno` a
syscall body returns, so no arm re-locks the thread info to write it. No
follow-up is open.

## Symptoms

`syscallfuzz` found it on its first run:

```
syscallfuzz -n 8 -v -u 0 -o ioctl
  ioctl  fxpnx
    case  0 [ffffffffffffff9c, 80000000, 436a41, 1001, ffffffff] ->
```

The case never returns. Deterministic: the generator is seeded `seed ^ nr`, so
the same case index hangs every time. Any `ioctl` on a descriptor that is not
open reproduces it; `-1` works as well as `-100`.

The machine then dies somewhere else. CPU 0 stops taking interrupts, and the
next unrelated `munmap` on another CPU trips the TLB shootdown watchdog:

```
<cpu-2:/bin/edos-wm:u:25> tlb_shootdown: re-sending IPI to CPUs 0x1
KERNEL PANIC: tlb_shootdown: CPUs 0x1 never acknowledged a flush of
         50 page(s) at VirtAddr(0x283f000) across 3 attempts
```

The watchdog is the messenger. The bug is whatever holds CPU 0.

## Root cause

Temporaries created in a `match` scrutinee live until the end of the whole
`match`, so every guard taken there is still held while an arm runs:

```rust
let descriptor = match info.lock().fd_table.lock().get_fd(fd).cloned() {
    Some(desc) => desc,
    None => {
        info.lock().errno = Errno::EBADF;   // re-locks a lock this CPU holds
        return -1;
    }
};
```

`info` is an `Arc<IrqSpinlock<UserThreadInfo>>`. The `None` arm spins on a
spinlock its own CPU already holds, with interrupts disabled, and never leaves.
A CPU spinning with interrupts off acknowledges no IPI, which is why the panic
names a shootdown.

Edition 2024 drops `if let` scrutinee temporaries before the body. `match`
still extends them.

## Reasoning rules going forward

- A guard taken in a `match` scrutinee is held across every arm. Bind the
  looked-up value with `let` first, then match on the owned value.
- An `IrqSpinlock` re-entered by its holder does not panic or report. It turns
  the CPU deaf, and the symptom surfaces on a different CPU.
- The same statement-lifetime rule is the class in
  `2026-08-18-fd-table-locked-with-interrupts-off.md`; the grep there finds
  both shapes.

## If this reappears

1. A TLB shootdown watchdog panic naming one CPU in its mask, with nothing
   wrong in the unmapping code, means that CPU stopped taking interrupts.
2. Read that CPU's RIP over QMP or gdb. Inside `IrqSpinlock::lock` means a
   spinlock wait with interrupts off; find who holds it.
3. If the holder is the same thread, look for a guard living in a `match`
   scrutinee or a chained `.lock()` in the syscall on the stack.
