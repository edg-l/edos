# A syscall took the fd table's `BlockingMutex` with interrupts disabled

## Status

Fixed in the same commit that closed the balance-fanout work. Four call sites
changed: `sys_pipe`, `sys_openpty`, `sys_fstat`, and the last-thread teardown in
`Thread::exit`. No follow-up is open; `sys_dup` already had the correct shape and
was the model.

## Symptoms

A kernel panic under a multi-threaded program that creates pipes while its other
threads are doing I/O. Seen on the second `balancebench wake` run of a boot,
never on the first:

```
KERNEL PANIC: BlockingMutex::lock contended with interrupts disabled
  at src/thread/mutex.rs:95
Backtrace:
  core::panicking::panic_fmt
  <BlockingMutex<FileDescriptorTable>>::lock
  edos_kernel::syscalls::syscall_handler   kernel/src/syscalls/mod.rs:788
  edos_kernel::syscalls::syscall_entry
```

The line number in `syscall_handler` names the dispatch arm, so it reads as
`SYS_PIPE`, `SYS_OPENPTY` or `SYS_FSTAT` depending on which one lost.

Intermittent by nature: the assertion only fires on the *contended* path, so it
needs another thread of the same process to be holding the fd table at that
instant. A single-threaded program can never trigger it.

## Root cause

`current_thread_info()` hands back an `Arc<IrqSpinlock<UserThreadInfo>>`, and the
fd table inside it is an `Arc<BlockingMutex<FileDescriptorTable>>`. Written as
one expression:

```rust
let read_fd = info.lock().fd_table.lock().allocate_fd(...);
```

the temporary `IrqSpinlock` guard lives until the end of the enclosing statement,
so `BlockingMutex::lock` runs *inside* it — with interrupts disabled. Uncontended
that is invisible, because `try_acquire` succeeds and nothing parks. Contended,
the same line parks the thread with interrupts off, which is exactly what the
interrupt/park discipline forbids and what the debug assertion exists to catch.

The fix is to take the `Arc` out of the guard before locking it, which ends the
guard's lifetime at the `let`:

```rust
let fd_table = info.lock().fd_table.clone();
let read_fd = fd_table.lock().allocate_fd(...);
```

## Reasoning rules going forward

- A `BlockingMutex` acquired in the same *statement* as an `IrqSpinlock` lock is
  acquired inside it. Rust extends a temporary's life to the end of the
  statement; method-chaining hides the nesting completely.
- Clone the inner `Arc` out first. That is not a style preference — it is what
  ends the outer guard's lifetime.
- `UserThreadInfo` holds two `BlockingMutex` fields, `fd_table` and `cwd`. Both
  are reachable only through an `IrqSpinlock`, so both want this shape.

## If this reappears

Read the panic's `syscall_handler` line number against `kernel/src/syscalls/mod.rs`
to name the syscall, then look for a `.lock()` chained onto `current_thread_info()`
in that syscall's implementation:

```
grep -rn 'lock()\.fd_table\.lock()\|lock()\.cwd\.lock()' kernel/src
grep -rn -A3 'current_thread_info()' kernel/src/syscalls | grep -B1 -A2 'lock()'
```

Tell it apart from the nearby class it resembles: a lock-*order* violation panics
out of the rank tracker with two named locks, not out of `mutex.rs`. This one
names no second lock because the `IrqSpinlock` is not ranked — it is only the
interrupt state that is wrong.

## Saved artifacts

None kept. Seen once, on the second `balancebench wake` of an 8-CPU boot; six
consecutive `wake` and `sleep` runs on the fixed kernel produced none. How often
it fires is not established — it needs the fd table contended at that instant, so
a program that creates pipes from one thread while others read and write is the
shape to try.
