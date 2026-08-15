# lookupd

A caching name lookup daemon. This is the design; nothing of it is built yet.

## 1. Problem

Every name lookup in EDOS is a query on the wire. `edos_rt::net::lookup_a` asks
`SYS_GETDNS` for the address DHCP learned, opens a UDP socket, and sends a
query to port 53 with a 2 second receive timeout and `DNS_ATTEMPTS = 3`
retries. Nothing between the program and the server remembers anything.

The cost is not evenly spread. `ping` and `grab` resolve once and exit, so they
pay one query and nothing would help them much. `edos-web` loading a page with
subresources spread over several hosts re-resolves every host on every request,
and a lost datagram there costs 2 seconds before the first retry.

A cache inside `edos_rt` is per process. It would help `edos-web`, which is the
one consumer long-lived enough to reuse an answer, and it would do nothing for
anything else on the system. It also cannot do the thing worth most here:
collapse two processes asking the same question at the same moment into one
query. The win is system-wide, so the cache has to be.

There is exactly one resolver on the wire, in `edos_rt`.
`edos_lib::net::resolve_host` is a short wrapper that returns a dotted-quad
literal as-is and otherwise hands off to `to_socket_addrs`, which lands in the
same `lookup_a`. Whatever is decided here is decided in one place.

## 2. Why the kernel is the wrong home

DNS is policy, not mechanism. TTLs, negative caching, search domains,
per-interface resolvers, DNSSEC and the choice of transport are all decisions
about what a name means to this machine, and the moment the kernel parses a DNS
packet it owns every one of them permanently.

No mainstream system puts it there. systemd-resolved, FreeBSD's
`local_unbound`, macOS's mDNSResponder, Android's `netd` and the Windows DNS
Client service are all userspace daemons; the kernel in each case carries
packets and nothing more. The ones reached over loopback:53 rather than a
private IPC (unbound, dnsmasq) got every existing client without changing any
of them, which is the property that matters most here.

The kernel's current involvement is already the right amount, and its own doc
comment on `sys_getdns` says why:

> A resolver is configuration, not a socket operation, and there is no
> filesystem convention for it here the way `/etc/resolv.conf` serves Unix, so
> userspace asks the stack that learned it from DHCP.

## 3. Shape

`lookupd`, a service under `edos-init`, listening on `127.0.0.1:53` and
speaking ordinary DNS wire format in both directions. Clients need no change at
all: they already send DNS to a configurable address.

The name follows `sshd` and `httpd`, the two daemons already in the tree, and
is taken from the macOS daemon of the same name that cached name lookups before
DirectoryService replaced it. It says "caches name lookups" rather than "serves
DNS", which leaves room for a hosts file or mDNS later without a rename. The
port is plain `127.0.0.1:53`, the address FreeBSD's `local_unbound` and dnsmasq
take, not a private stub address of its own.

**It is a forwarding cache, not a resolver.** It parses only the question
section of a client query, and on a miss forwards the query upstream and caches
the *raw response bytes* keyed by that question. Serving a hit means rewriting
the transaction ID and decrementing the TTLs in place. It never walks from the
root, never follows a delegation, and never needs to understand a record type
it was not asked about. This is what dnsmasq does, and it is the difference
between a few hundred lines and a real resolver.

Everything it needs is already proven by `programs/socktest`: UDP bind on a
port, `sendto` and `recvfrom` carrying the peer address, loopback delivery,
`MSG_DONTWAIT` and `MSG_PEEK`.

## 4. How a client finds it

A client asks `SYS_GETDNS`, so that is where the redirect goes.

Add **`SYS_SETDNS`, number 316** (315 is the current highest), with the usual
three parts: the dispatch arm, a row in `syscalls/table.rs` so `strace` names
it, and a wrapper in `edos_lib`. The daemon calls it once at startup to point
the stack's resolver at `127.0.0.1`.

The kernel holds `resolver_override: Option<([u8; 4], Tid)>` beside
`dns_server` in `net::stack`. `sys_getdns` returns the override if its owning
thread is still in the registry, and otherwise clears it and returns the
DHCP-learned address.

Two things about that design are deliberate.

**Revocation is lazy, on the read, rather than a hook in process teardown.**
The last-thread block in `Thread::free` is under the drop contract: it must not
block, and reaching for the net stack lock there is exactly the shape that
contract exists to forbid. A registry lookup on `getdns` costs nothing
next to the query that follows it, and it means a resolver that dies takes its
override with it with no teardown code at all.

**The redirect is not a file.** `/etc/resolv` read by `edos_rt` would be the
Unix answer, and it is the wrong one twice over: it is runtime state rather
than a setting that outlives a boot, so `edos_lib::config` is not its home by
this project's own rule; and it would mean a change to the std fork, the full
publish loop, and no effect at all on binaries already on disk. The syscall
works for every program already built.

The one wrinkle is tid reuse: a dead daemon's tid could be handed to something
else, which would then appear to own the override. The window is small, the
consequence is that the override outlives its owner briefly, and a restarted
`lookupd` re-asserts the override at startup, so the state self-heals. That is
cheap enough to accept rather than design around.

## 5. What the cache does

**Key**: the question, as (lowercased name, qtype, qclass). Answers are stored
as the upstream's own response bytes.

**One dial**: `MAX_ENTRIES`, the cache size, evicted LRU. Everything else is
derived from the protocol rather than picked. Positive entries expire on the
TTL the answer carried. Negative entries expire on the SOA MINIMUM per RFC 2308
section 5, which is where a resolver is told how long an NXDOMAIN may be
believed. A TTL is clamped only at the top, to stop a broken or hostile zone
pinning an entry for a week.

**Coalescing**, and this is the part a per-process cache can never do: a query
arriving for a question already in flight attaches to the outstanding request
rather than starting a second one, and every waiter is answered from the one
response. A page pulling three subresources from the same host at once
generates one query.

**Negative caching** matters more on a desktop than it looks. Typos and dead
hosts are most of what anything resolves twice.

**Serve stale on upstream failure**, per RFC 8767: if the upstream does not
answer, an expired entry is served rather than nothing. This is worth more here
than on a well-connected machine, because the uplink is a QEMU user-mode
network or a real NIC on a hobby OS, and both fail in ways a datacenter link
does not.

## 6. What v1 does not do

Each of these is a follow-on, listed so nobody has to guess whether it was
forgotten or excluded: recursion from the root, DNSSEC, DNS over TLS or HTTPS,
search domains and a suffix list, AAAA records (the stack has no IPv6 at all),
TCP fallback for a truncated answer, `/etc/hosts`, and mDNS.

Truncation deserves a line: a response with TC set is passed through to the
client unchanged, so a program sees exactly what it sees today, and is not
cached. That keeps v1 from pretending to a completeness it does not have.

## 7. Failure modes

**The loop.** `lookupd` must not use `edos_rt::net::lookup_a` for its own
upstream queries. It would call `dns_server()`, receive `127.0.0.1` back, and
query itself forever. It reads the upstream address once at startup *before*
installing the override, or reads the `dns:` line from `/proc/net`. This is the
same reason systemd-resolved does not resolve through its own stub, and it is
the first thing to get wrong.

**The daemon is down.** This stack sends no ICMP port unreachable for a
datagram to a closed port, so a client querying a dead `lookupd` gets silence
and burns all three attempts at 2 seconds each. A crashed resolver would turn
every lookup on the system into a 6 second stall. The lazy revocation in
section 4 is what bounds it: the next `getdns` sees the owner gone and hands
back the DHCP address, so the failure degrades to "no cache" rather than "no
network". `edos-init` restarting the service covers the rest.

**Boot ordering needs nothing.** Before `lookupd` starts, `getdns` returns the
DHCP address and lookups work exactly as they do now. There is no window in
which a client is pointed at a daemon that has not bound its socket, because
the daemon installs the override itself, after binding.

## 8. Configuration and lifecycle

`/etc/services/lookupd.conf` declares it, with `enabled_by /etc/lookupd.conf`
so a system without that file never starts it, the way `sshd` is gated today.
`svc start lookupd` and `svc stop lookupd` work through the existing control
FIFO with no new mechanism. See `doc/services.md`.

`/etc/lookupd.conf` carries the upstream address, defaulting to whatever DHCP
learned, and the cache size. One value per line with `#` comments above it, per
`edos_lib::config`.

**SIGHUP flushes the cache** and logs a counter line to `/dev/klog`. That is
the BSD convention for exactly this, it gives the tests an observable, and it
needs no new signal: this kernel has SIGHUP but no SIGUSR1 or SIGINFO.

## 9. Testing

The wire handling (parse a question, rewrite an ID, decrement TTLs, decide
expiry) should live in a library with **host unit tests**, which run in about a
second alongside the other 114 and need no guest.

In the guest, `programs/dnsprobe` is already the client this needs: it builds a
raw query and takes the server as its second argument, so `dnsprobe example.com
127.0.0.1` talks to `lookupd` directly with nothing else in the way. Three
cases, all worth adding to `make guest-check`:

1. Resolve a name twice and assert the second is a hit, read from the SIGHUP
   counter line.
2. Resolve the same name from **two processes** and assert the second is a hit.
   This is the whole reason the daemon exists rather than a cache in
   `edos_rt`, so it is the case that must go red without it.
3. Kill `lookupd` and assert a lookup still answers, promptly, from the
   DHCP-learned address. This is the section 7 failure mode, and it is the one
   that would otherwise be discovered by a 6 second stall in the browser.

## 10. Sizing

`SYS_SETDNS`, the override field and the `getdns` change are an afternoon. The
daemon is a day, and a useful one beyond DNS: it is the first program in the
tree that is a server for other local programs, so it exercises the UDP receive
path with several peers on one socket, and gives `svc` something to start and
stop that is not `sshd`.
