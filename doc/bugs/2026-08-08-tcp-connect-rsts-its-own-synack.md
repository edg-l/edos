# TCP client connect RSTs its own SYN-ACK

## Status

**FIXED** in `b87ca1b`. Reproducer kept as `programs/tcptest`.

## Symptoms

A connect through the socket syscalls reached SynSent, the peer answered, and
the guest reset the connection immediately:

```
10.0.2.15.49152 > 10.0.2.2.8088: Flags [S],  seq 3838609347
10.0.2.2.8088 > 10.0.2.15.49152: Flags [S.], seq 1600001, ack 3838609348
10.0.2.15.49152 > 10.0.2.2.8088: Flags [R.]
```

Userspace saw `connect` fail with ECONNREFUSED. No TCP connection had ever been
established in this kernel; it went unnoticed because `http` and `wget` use
`std::net::TcpStream`, which the std fork does not implement, so nothing had
ever tried.

## Root cause

`WaitQueue::wait_until_timeout` slept **once** and returned on any wake:

```rust
SleepAction::Sleep(dt) => {
    thread_sleep(dt);   // no loop, no predicate re-check, no deadline check
}
```

`thread_sleep` returns early when the thread is woken, including by a
wake-pending token left by an earlier wait. `sys_connect` waits twice in a row —
first for the ARP reply, then for the connection to reach Established — so the
ARP wake left a token that aborted the second sleep in microseconds. Connect
found the state was still SynSent, treated that as its five-second timeout
expiring, removed the connection from `tcp_connections`, and returned
ECONNREFUSED. The SYN-ACK arrived 0.2 ms later, found no matching connection,
and got the RST that `handle_tcp` sends for unknown segments.

The same mistake existed at a call site: `sys_read`'s socket paths called
`wait_until` once and then treated an empty receive buffer as EOF, so every TCP
read returned 0 bytes even after the connection was fixed.

This is the contract in
[`2026-04-13-sched-park-wake-missed-wakeup.md`](2026-04-13-sched-park-wake-missed-wakeup.md):
a park or sleep may return before its condition holds, so the condition must be
looped on. `wait_until_timeout` was the one primitive that did not.

## Fix

The timed arm loops until the predicate holds or the deadline has genuinely
passed. `sys_read`'s TCP and UDP paths loop on their own condition.

**The untimed arm was deliberately left parking once.** Making it loop as well
looked symmetrical and stalled the boot: a caller whose predicate only becomes
true through work that same thread has yet to do never returns. Two of three
services failed to start. If you are tempted by that change, this is the second
time it will have looked correct.

## Why the first investigation went wrong

The original writeup recorded that the connection map read `len=1` in the
connecting thread and `len=0` in the e1000e kthread microseconds later, at the
same address, with an atomic in the same struct staying coherent — and reasoned
towards memory corruption or page-table divergence.

Every one of those observations was accurate. The error was in what was *not*
instrumented: the `remove` in connect's own failure path. Tracing every mutation
of `tcp_connections` produced the answer in one run:

```
12.344720  connect-insert       len=1
12.345300  connect-fail-remove  len=0
12.345528  rx SYN-ACK           len=0   -> RST
```

The lesson is narrow and worth keeping: when a container appears to lose an
entry, instrument **every mutation** before theorising about the memory model.
A reader that disagrees with a writer is far more likely to be a third writer
you have not looked at.

## Verified

`tcptest` against a host HTTP server: 214-byte and 270367-byte responses
received intact, the latter spanning many segments. `ping` also recovers its
first packet, which the same spurious ARP timeout had been losing.
