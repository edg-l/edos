# lookupd

A caching name lookup daemon. Built and in `make guest-check`: `programs/lookupd`
is the daemon, `programs/lookuptest` is its regression suite, and `SYS_SETDNS`
(316) is the one piece of it in the kernel.

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

**`SYS_SETDNS` is 316**, with the usual three parts: the dispatch arm, a row in
`syscalls/table.rs` so `strace` names it, and a wrapper in `edos_lib`. The
daemon calls it once at startup, after binding, to point the stack's resolver at
`127.0.0.1`. Passing `0.0.0.0` clears the override.

The kernel holds `resolver_override: Option<([u8; 4], ThreadId)>` beside
`dns_server` in `net::stack`. One function decides what a lookup should use:
`net::stack::effective_resolver` returns the override if its owning thread is
still running, and otherwise drops it and returns the DHCP-learned address. Both
`sys_getdns` and `/proc/net` ask it, so what the machine reports and what it
does cannot drift apart.

Any process may call it. There is no privilege model here to hang it on, and the
same is true of every other configuration syscall in this kernel.

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
else, which would then appear to own the override. The window is small and the
consequence is that the override outlives its owner briefly. It is covered
rather than designed around: the daemon reclaims the slot on any idle tick that
finds `getdns` no longer naming loopback, so the state self-heals within a
second, and the same mechanism covers a second process overwriting the slot
outright.

## 5. What the cache does

**Key**: the question, as (lowercased name, qtype, qclass). Answers are stored
as the upstream's own response bytes.

**One dial**: `MAX_ENTRIES`, the cache size, evicted LRU. Everything else is
derived from the protocol rather than picked. Positive entries expire on the
TTL the answer carried. Negative entries expire on the SOA MINIMUM per RFC 2308
section 5, which is where a resolver is told how long an NXDOMAIN may be
believed. A TTL is clamped only at the top: `TTL_CEILING` of a day for a
positive answer, so a broken or hostile zone cannot pin an entry for a week, and
five minutes for a negative one, which is the maximum RFC 2308 section 5 asks
every implementation to impose.

**Coalescing**, and this is the part a per-process cache can never do: two
programs asking the same question at the same moment cost one query on the
wire, not two.

It falls out of the loop being single-threaded rather than being built. The
daemon reads one datagram, answers it, and only then reads the next; a query
that arrives while an upstream request is outstanding waits in the socket
buffer and is answered from the cache the request just filled. A page pulling
three subresources from one host generates one query.

What that costs is head-of-line blocking: one slow upstream query delays every
client behind it, by up to the two second timeout. That is the right trade at
this volume and it is the thing to revisit first if it ever is not. The fix is
an in-flight table and a thread per outstanding request, not a second cache.

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

`lookupd` is one of init's compiled-in services, beside `edos-wm` and `sshd`,
gated by `enabled_by /etc/lookupd.conf`. The image ships that file, so the
resolver is **on by default** and deleting the file is the off switch. A system
without it resolves exactly as it did before the daemon existed, which is what
makes turning it off safe.

The file holds one value, per `edos_lib::config`: the upstream resolver's
address, or `dhcp` to use the leased one. The cache size is a constant in the
program rather than a second setting, because a second value would need a
second file and nothing has yet wanted to change it.

**`SIGHUP` flushes the cache** and logs a counter line to `/dev/klog`. That is
the BSD convention, it gives the tests an observable, and it needs no new
signal: this kernel has SIGHUP but no SIGUSR1 or SIGINFO.

**Counters are published to `/tmp/lookupd.stats`**, one `key: value` per line
in the shape of a `/proc` file, rewritten at most once a second. `/tmp` is
memfs and forgets at reboot, which is right: this is a record of what this
boot's resolver has done, not a setting. A reader must therefore wait for a
counter to move rather than expect it to have moved already.

**The override is reclaimed once a second.** It is a single slot in the kernel,
so whoever writes it last owns it, and a process that installs its own and
exits leaves the machine pointed back at the upstream. The daemon compares
`getdns` against loopback on each idle tick and claims it again if something
displaced it, which makes that self-healing rather than permanent. It is also
what covers the tid-reuse wrinkle in section 4.

## 9. Testing

The wire handling and the cache are in `programs/lookupd/src/dns.rs`, which
depends on nothing but `std` and takes time as a parameter, so its **16 host
unit tests** run in `make host-tests` in about a second: question parsing and
its refusals, case-insensitive keying, the message TTL being its shortest
record, ageing in place, OPT records being left alone, negative TTLs from the
SOA, LRU eviction, and the fresh/stale/miss boundary.

In the guest, `programs/lookuptest` is the suite, and it is in
`make guest-check`. Six cases:

1. `/proc/net` and `getdns` both name the local resolver.
2. A name looked up twice is a hit the second time.
3. A **second process** is answered from the first one's lookup. This is the
   case that separates a system-wide cache from one inside a library, so it is
   the one that must go red without the daemon.
4. `SIGHUP` empties the cache without resetting the totals.
5. An override whose owner exited is revoked, and the lookup that follows
   answers promptly from the DHCP address. Without this it is the six second
   stall in section 7: the child points the system at `127.0.0.2`, where
   nothing listens.
6. The daemon reclaims the override afterwards.

`programs/dnsprobe` remains the tool for looking at raw bytes by hand: it takes
the server as its second argument, so `dnsprobe example.com 127.0.0.1` talks to
`lookupd` with nothing in between.

Like `socktest`, this needs a working upstream: the cases turn on an answer
coming back at all.

## 10. What it cost, and what was found building it

The kernel side was an afternoon as estimated: an `Option<([u8; 4], ThreadId)>`
beside `dns_server`, `effective_resolver` in `net::stack` as the one place that
decides, `sys_setdns`, and a `resolver:` line in `/proc/net` that asks the same
function so a dead resolver's override stops being reported at the same moment
it stops being used. The daemon and its suite came to about 900 lines including
tests.

Three things worth keeping:

**`requires /dev/eth0` was wrong and cost fifteen seconds of every boot.** There
is no `/dev` node for the NIC on this system, so init waited out its whole
device timeout and started the daemon anyway. The daemon reads its upstream
from `/proc/net`, which answers before DHCP has finished, so it needs no wait at
all.

**`process::spawn` takes `argv[1..]`, not `argv[0]`.** Passing the program name
as the first argument made `dns` try to resolve the name "dns", and made
`lookuptest` re-run its whole suite instead of taking its `--steal` branch,
which recursed until the guest was stopped. Init's own `args` field documents
the convention; the spawn wrapper does not.

**A formatted line reaches `/dev/klog` in fragments.** `writeln!` issues a write
per fragment and the log interleaves them with everything else on the machine,
so `lookupd:` and the message arrived as two lines. Build the string, then write
it once.
