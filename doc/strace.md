# Syscall tracing

`strace` shows what a process asked the kernel for. This OS is driven through
screenshots and a serial log, so a program that fails silently used to leave
nothing behind; now it leaves a transcript.

```
/ $ strace ls /etc
execve("/bin/ls", 0x442520, 0x4426c0) = 0
mmap(NULL, 65536, 0x2, 0x22, 0xffffffffffffffff, 0) = 4349952
isatty(1) = 1
getdents("/etc", 4, 0x4263d0, 4096, 0) = -1 ENOENT
write(2, "ls: cannot access '", 19) = 19
...
exit(1) = ?
+++ exited with 1 +++
```

## Using it

```
strace [options] PROGRAM [ARGS...]
strace [options] -p PID

  -p PID     trace a running process instead of starting one
  -o FILE    write the trace to FILE instead of stderr
  -e LIST    only show these syscalls (comma separated)
  -s SIZE    show at most SIZE bytes of each string (default 32)
  -c         a summary per syscall instead of a line per call
  -T         how long each call took
```

Children are followed without asking, and their lines carry a `[pid N]` prefix.
One process per line otherwise, so a single-process trace reads cleanly.

A call that has not returned by the time the reader catches up prints
`<unfinished ...>` and its return prints later as `<... name resumed>`. That is
the answer to "the program is doing nothing": it says which call it is sitting
in.

```
nanosleep(0x6fffffffe908, NULL) <unfinished ...>
<... nanosleep resumed> = 0 <1.000049>
```

## How it works

Three pieces, none of them `ptrace`.

**A mark on the thread.** `Thread::traced` holds the trace-session generation
the thread was marked under, and `syscalls/trace.rs` treats it as traced only
while that matches the current generation. Ending a session therefore costs one
increment rather than a walk of the thread table, and a mark left behind by a
tracer that died cannot come back to life under the next one. On the untraced
path — which is every thread almost always — the check is one relaxed load next
to the `last_syscall` store the dispatcher already does.

**Two records per call.** `syscall_handler` in `kernel/src/syscalls/mod.rs` is
one choke point for the whole syscall surface, so the entry record is written
there before `dispatch` and the return record after it. A `TracedCall` — tid,
session generation, number and the six arguments — is built once at entry and
carried to the return. That is what stops a thread which marks itself mid-call
from emitting a return with no matching entry, and it is why the return path
never has to trust the argument registers.

**Records are validated against the session that authorised them.** The decision
to trace is taken at entry, and a blocking `read` can outlive the tracer that
made it: without the generation check inside `emit`, that call's return would be
delivered to the *next* tracer, which would then hold a thread it never marked
and wait forever for a death record that never comes. Ownership and the
generation both move under the ring lock, which is what makes claiming and
tearing down a session mutually exclusive.

**A ring, drained by the tracer.** ~250 KiB, allocated when a tracer claims the
session and freed when it lets go. The target never blocks on the tracer: a
tracer that falls behind loses records and is told how many at the end, rather
than changing the timing of the program it is supposed to be observing.

### What the records carry

Arguments are the six syscall registers, plus up to two strings copied out of
user memory. Which arguments are strings comes from the table in
`kernel/src/syscalls/table.rs`, which also names every syscall, says which
function implements it, and is what `dispatch` is generated from — and which
`/proc/syscalls` publishes, so `strace` formats calls from the kernel's own
description instead of a second copy that would rot:

```
/ $ head -3 /proc/syscalls
call 0 read fon
call 1 write fSn
call 2 open sx
```

`f` is a descriptor, `n` a count, `s` a NUL-terminated string, `S` a string
with its length in the following argument, `o` a buffer the call *fills* — so
`read` shows what came back and `write` what went out. The same file carries
`errno <value> <name>` rows, which is where the `ENOENT` above comes from.

Output buffers are captured on the way out, sized by the return value. The
arguments used to find them are the ones copied at entry, not the registers as
they stand on return: a dispatch arm is not obliged to leave those alone.
`sys_execve` replaces the whole `SyscallContext` with the new image's, so on
that path they name an address space that no longer exists.

### Session ownership

One tracer at a time; a second `claim` fails with `EBUSY`. The session is
released explicitly or when the tracer thread dies, whichever comes first —
`thread_exit` calls into the tracer for both that and for the `+++ exited +++`
record, so killing `strace` with Ctrl+C leaves nothing marked and nothing
writing into a ring nobody drains.

`trace_read` is the tracer's alone. Anyone else draining the ring would steal
records the real tracer then never sees and `DROPPED` never counts — and would
park on the same wait queue, whose enqueue panics the kernel past 64 waiters.
Unmarking is the tracer's alone too, so an unrelated process cannot blind a live
trace one thread at a time.

Marking is deliberately *not* restricted, because the useful case is a freshly
forked child marking itself before `execve`. That is safe only while this system
has no users; when it grows them, `ctl::MARK` is the place that has to start
asking.

## Attaching to something already running

```
/ $ strace -p 25
window_list(0x6ffffffed08, 32) = 2
clock_gettime(0x6fffffe910) = 0
window_poll(2, 0x6fffffeae0, 16) = 0
window_damage(2) = 0
```

Ctrl+C ends it. Attaching cannot show the first calls a process ever made, which
is why tracing a program you start does not attach at all: `strace` forks and
the child marks *itself* before `execve`, so there is no window in which it is
running and not yet marked.

## Known limits

- **Records can be lost.** A program issuing syscalls faster than the tracer
  drains them overflows a 1024-entry ring. The count is printed at the end;
  it is never silently short. A death record is the exception: it is what tells
  the tracer to stop, so it evicts the oldest record rather than being dropped.
- **A string argument can be crowded out.** The two captured strings share 160
  bytes. A `renameat` with one very long path leaves no room for the other,
  which prints as `...` — distinct from a bad address, which prints as a raw
  pointer.
- **`readv`, `writev` and `ioctl` show pointers, not contents.** The buffer
  lives behind another indirection the capture does not follow.
- **A syscall that never returns has no return record.** `exit` prints `= ?`,
  as does any call the thread was killed inside.
- **`-p` addresses a thread, not a process.** For a single-threaded program the
  two coincide. A process that called `clone` has one mark per thread, and
  attaching by pid marks only the main one; children it *spawns* are still
  followed.
