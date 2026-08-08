# TCP client connect RSTs its own SYN-ACK

## Status

**OPEN.** Not fixed, and not caused by any change in this session: a build
from `3289aa4` (before the session's work) fails identically at the packet
level. Reproducer landed as `programs/tcptest`.

Nothing in the tree depended on this working, which is why it went unnoticed:
`http` and `wget` use `std::net::TcpStream`, which the std fork does not
implement ("operation not supported on this platform"), so no program has ever
completed a TCP connection. `ping` (SYS_PING) and DNS (UDP) are unaffected and
work.

## Symptoms

A connect through the socket syscalls reaches SynSent, the peer answers, and
the guest resets the connection immediately:

```
10.0.2.15.49152 > 10.0.2.2.8088: Flags [S],  seq 3838609347
10.0.2.2.8088 > 10.0.2.15.49152: Flags [S.], seq 1600001, ack 3838609348
10.0.2.15.49152 > 10.0.2.2.8088: Flags [R.]
```

Userspace sees `connect` fail after the 5 s timeout with ECONNREFUSED. The
serial log shows `tcp <peer> Closed -> SynSent`, an ARP request and reply, and
then nothing further for that connection.

Reproduce with a listener on the host and:

```bash
python3 -m http.server 8088 &          # on the host
tcptest 10.0.2.2 8088 /                # in the guest
```

## What is established

The RST comes from the "no connection found" arm of `NetStack::handle_tcp`
(`net/stack.rs`), so the lookup `tcp_connections.get(&(local, remote))` misses.
The keys are not the problem: instrumenting both sides showed byte-identical
`local`/`remote` (10.0.2.15:49152 / 10.0.2.2:8088) on the insert in
`sys_connect` and on the lookup in the receive path.

What the instrumentation actually showed is stranger, and is where the next
session should start:

| Observation | Value |
|---|---|
| `&NET_STACK` in both contexts | `0xffffffff801a2ad0` — identical |
| `&stack.tcp_connections` in both contexts | `0xffffffff801a0bc8` — identical |
| A plain `static AtomicU64` written in the user thread, read in the kthread | coherent |
| An `AtomicU64` **field of `NetStack`**, written in the user thread, read in the kthread | coherent |
| `tcp_connections.len()` right after insert, user thread (`/bin/tcptest`) | **1** |
| `tcp_connections.len()` ~0.3 ms later, e1000e kthread, same CPU | **0** |

So two contexts read different values from one address, while an atomic a few
bytes away in the same struct stays coherent. Nothing removes entries in that
window: the only remover is the sweep in `tcp_retransmit_main`, which was
instrumented and logged no removals, and which only drops `Closed`/`TimeWait`.

That combination is not explained by page-table divergence (the process PML4
shares kernel-half entries with the kernel table, and the adjacent atomic is
coherent), so the likely candidates are, in order:

1. **The logging is lying about ordering or values.** `log!` pushes to a queue
   drained by a kthread; confirm it formats eagerly at the call site before
   trusting any of the table above. This is the cheapest thing to rule out and
   would invalidate the rest.
2. A genuine miss with a subtler cause — e.g. the receive path running against
   the map before the insert is visible, with the timestamps misleading.
3. Real memory corruption localized to the BTreeMap's inline `length`/`root`.

## If this reappears

- Capture packets first; `-object filter-dump,id=dump0,netdev=net0,file=…`
  added to the `scripts/edos-vm` QEMU line, then `tcpdump -nr`. A guest RST in
  response to a SYN-ACK is this bug.
- Distinguish from "no route/ARP": those show no SYN on the wire at all.
- Distinguish from the retransmit path: `check_retransmit` runs from
  `tcp-retransmit` every 200 ms and would show resends in the capture.

## Related

The retransmit timeout was rebuilt on RFC 6298 in `225f372`, which is correct
by inspection but **cannot be exercised end to end until this is fixed**, since
no connection reaches Established. The server path (`listen`/`accept`) was not
tested and may or may not share the fault.
