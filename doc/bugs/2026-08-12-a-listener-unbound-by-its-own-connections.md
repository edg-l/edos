# A listener was unbound by the connections it accepted

## Status

Fixed across three commits, because the same defect was written out in four
places and each was found by a different program:

- `tcpecho` (2026-08-11): `sys_listen`'s lock order, and the close path in
  `syscalls/mod.rs::close_fd_refcount`.
- `httpd`, `d4781690` (2026-08-11): the two other close paths,
  `syscalls/io.rs::sys_close` and `thread/pipe.rs::close_descriptor`, plus the
  retransmitted-SYN backlog leak.
- `0ea8bbd7` (2026-08-12): the connection reaper in `tcp_retransmit_main`
  (`kernel/src/net/stack.rs`), plus the SYN-ACK that was never retransmitted.

The rule now lives in one place, `port_key` and `unbind_port` in
`kernel/src/net/socket.rs`, and every path that releases a port calls it.

## Symptoms

A server answers its first connection, or its first few, then every later SYN
gets RST. Three variants, by which path struck:

- `tcpecho`: the first `listen` tripped the rank tracker ("tried to acquire
  'sys_listen' (rank 250) while holding 'sys_listen' (rank 260)"), and the
  second connection was refused.
- `httpd`: refused everything after the first connection closed. `tcpecho`
  had survived the same code because it closes its listener between runs;
  `httpd` keeps one open.
- `httpd` again, later: refused everything after about eight connections,
  with no close involved.
- After the close paths were fixed: refused everything once the first
  connection left `TIME_WAIT`. This needs two connections more than five
  seconds apart, because the reaper strikes only when `TIME_WAIT` expires.
  One `curl` per boot never shows it.

## Root cause

A socket returned by `accept`, and the connection behind it, carries its
listener's local port. `PORT_TABLE` maps `(proto, port)` to the socket that
owns the port. Four paths released the entry by number alone, so the first
accepted connection to close, or be reaped, removed the listener's entry and
every later SYN found no listener:

- `close_fd_refcount`, `sys_close` and `close_descriptor`, each an open-coded
  copy of the same close sequence;
- the reaper, which collected `c.local_port` from each reaped connection and
  removed `(6, port)`.

`unbind_port` removes the entry only when it holds that exact `Arc`
(`Arc::ptr_eq`). The reaper collects the owning socket beside the port and
applies the same test; a dead `Weak` owner means the syscall path already
released it.

Two lock-order defects rode along. `sys_listen` held the socket lock (260) and
took the port table (250) under it, the reverse of `handle_tcp`'s receive path;
it now follows `sys_bind`: validate under the socket lock, drop it, take the
port table, re-take the socket. The close paths had the same inversion, and two
of them took the socket with a bare `.lock()` invisible to the rank tracker,
which is why it had never been reported. All of them now read the key under the
socket guard and release the entry after dropping it.

Two defects filled the backlog instead:

- The SYN path pushed a new `Socket` onto `accept_queue` for every SYN, and
  `sys_accept` removes only entries that reached `Connected`. A peer whose
  SYN-ACK was lost retransmits the same SYN from the same port, and each copy
  took a slot, so a backlog of 8 filled. The SYN path now drops the half-open
  entry from the same remote address and port first (RFC 793 §3.4: a
  retransmitted SYN is one attempt).
- The SYN-ACK was built inline and sent once, on no retransmit queue. A lost
  SYN-ACK was never resent, and a peer that vanished after its SYN held a slot
  forever. `TcpConnection::build_syn_ack` (`kernel/src/net/tcp.rs`) queues the
  segment like `build_syn`, which buys RFC 6298 backoff and death at
  `retries >= 5` (about 63 s). The reaper then prunes `accept_queue` of any
  connection that went `Closed` without reaching `Connected`.

## Reasoning rules going forward

- A table keyed by a resource that several objects legitimately share must be
  released by owner identity, not by key. `unbind_port` is the only release.
- A sequence written out three times gets fixed once. When a fix lands in one
  copy, grep for the others before closing the bug.
- A lock taken with a bare `.lock()` is invisible to the rank tracker, so an
  inversion through it goes unreported. Use the ranked macros.
- A server test is one connection until proven otherwise. Test with several
  connections spread over more than `TIME_WAIT`, and with more than the
  backlog in total.

## If this reappears

1. `netstat -a` on the guest. A missing `LISTEN` row with the server still
   running means something released the listener's port. A full backlog shows
   as `SYN_RECV` rows piling up on the listening port.
2. Time the failure. Refused after the first close is a close path; refused
   about five seconds after is the reaper; refused after N connections with no
   close is the backlog.
3. A `SynReceived` half-open cannot be produced through slirp: QEMU's
   `hostfwd` terminates the host connection and opens its own to the guest,
   which always completes. Exercising the half-open deadline needs a tap
   backend with a filter that drops the final ACK, or an in-guest raw-socket
   test.
