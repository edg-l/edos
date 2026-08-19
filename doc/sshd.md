# sshd

An SSH-2 server, `programs/sshd`. One algorithm per role, all of them ones
OpenSSH still offers by default, so a stock `ssh` connects with no options:

| Role | Algorithm |
| --- | --- |
| Key exchange | `curve25519-sha256` (and its `@libssh.org` alias) |
| Host key | `ssh-ed25519` |
| Cipher | `aes128-ctr`, both directions |
| Integrity | `hmac-sha2-256`, both directions |
| Compression | `none` |
| Authentication | `password` |

There is no bignum arithmetic anywhere in this system, which is why there is no
RSA host key and no `diffie-hellman-group14`. A client old enough to require
either cannot connect, and nothing else is missing for one that is not.

## Running it

```
sshd [-p port] [-f config] [-u user:password] [-k hostkey] [-s shell] [-v]
```

`edos-init` supervises it alongside the desktop session, but only on a system
that has an `/etc/sshd.conf`: the service carries an `enabled_by` path and is
skipped entirely when that file is absent. Turning the server on is therefore
the act of writing its configuration, and a system whose owner never asked for
SSH neither listens nor logs about it. Nothing is generated for you — the stock
image ships no `/etc/sshd.conf`.

To run one by hand, on a port of its own so it does not collide with the
supervised instance:

```
sshd -p 2222 -u edgar:hunter2 -v
```

`-v` logs each connection and prints the host key's `authorized_keys` line
alongside its fingerprint, which is what a first connection should be checked
against.

### Configuration file

`/etc/sshd.conf`, one `keyword value` per line, `#` starts a comment. A missing
file is not an error; the command line alone is a complete configuration, and a
flag always wins over the file.

```
port 22
shell /bin/sh
home /
hostkey /var/sshd_host_ed25519
user edgar hunter2
```

`user` may appear more than once. Passwords are stored and compared in the
clear: this system has no password hash, no `/etc/passwd`, and no user database
of any kind — `whoami` reads a fixed table in `edos_lib`. The comparison itself
is constant-time, and an unknown user takes the same path as a wrong password,
so neither the name nor the length leaks through timing.

### Host key

Generated on first run and written to `hostkey` as the raw 32-byte ed25519
seed. It is not an OpenSSH private key: nothing but this program reads it, and
the OpenSSH container would need a bcrypt KDF and a cipher no other part of the
server uses. There is no `chmod` on this system, so the file gets whatever
permissions the filesystem gives it; that is the reason it belongs somewhere
only the server reads.

On an ISO boot `/var` is the RAM-backed live root, so the key is new every boot
and clients will complain that it changed. An installed system keeps it.

## Shape

One thread per connection, as in `httpd`. A client that opens a socket and then
says nothing cannot stop the next one from being served.

Within a connection the session loop polls the socket and the shell's output
together, so neither starves the other. That is what makes one thread per
connection enough: nothing is polled on a timer, and the 200 ms poll interval
only bounds how quickly the shell's exit is noticed.

Channel flow control is honoured both ways. The shell's output descriptor is
left out of the poll set entirely when the client's window is closed, so a
client that stops reading becomes backpressure on the shell rather than an
unbounded buffer in the server.

That is also why an exited shell does not close its channel immediately. A
pipe write in this kernel never blocks — the ring grows instead — so a command
can dump megabytes and exit while the client's window still holds only the
first few. The exit status is recorded and the ordinary loop keeps running
until the output is spent, because only that loop reads the socket, and only
the socket carries the `WINDOW_ADJUST` that lets the rest through. Draining in
place instead truncated a 10 MB `cat` to 4 MiB and reported success.

An unauthenticated peer gets 30 seconds and no more: everything up to a
successful authentication is work it can make a thread do, and a client that
connects and says nothing would otherwise hold one forever. The limit is
cleared once the session starts, where an idle connection is a shell waiting
for its user. Connections are capped at 16, and the surplus is refused at
accept rather than allowed to reach `thread::spawn` — which panics on failure,
and would take the listener down with it.

Each connection thread ignores `SIGPIPE`. Writing to a shell that has already
exited hits a pipe with no reader, and the kernel's default action for that
signal is to terminate the thread — which would leak the socket, since nothing
unwinds. Ignored, the failed write is a return value, and the channel's input
is closed instead. Dispositions are per thread here and are not inherited, so
this is set inside each connection rather than once in `main`.

`Child::drop` sends `SIGHUP` to the session's process group, waits briefly,
then `SIGKILL` to the shell itself. Closing the descriptors is not enough: a
command that neither reads its input nor writes its output never notices, so a
disconnected `ssh host 'sleep 3600'` would otherwise run out its hour with
nobody attached. The connection thread puts itself in a process group before
spawning anything and ignores `SIGHUP`, so the hangup names the session and
not the server. The escalation is aimed at the shell alone, because `SIGKILL`
cannot be ignored and a group-wide one would take the connection thread with
it, leaving the socket unclosed.

**A grandchild inherits the session's group.** `do_spawn` copies the spawner's
`pgid` the way both `fork` paths already did, so everything the session shell
starts shares the group the connection thread took before spawning it, and one
`SIGHUP` reaches the whole tree. `sh -c "sleep 300"` and its `sleep` now read
the same `pgid` in `/proc/processes`, where the `sleep` used to lead a group of
its own that the hangup could not name.

Inheritance alone was not enough, and that is why an earlier attempt at the
one-line kernel change looked like it did nothing: `edos-sh` put every pipeline
in a group of its own unconditionally, undoing the inheritance immediately
after the spawn. Job control is an interactive shell's business, so the shell
now only does it when it is interactive — a `-c` command and a script leave
their children where they were started. The kernel likewise no longer decides
that a child reading a pty leads a new group; an interactive shell claims the
terminal for itself with `setpgid` and `tcsetpgrp`, which is where that policy
belongs.

An interactive session is the one case where the shell does leave the
connection's group, since job control requires it. `Child::drop` therefore
signals the shell's own group as well as the session's.

### With and without a pty

`pty-req` gets a real pty and both directions run over the master. Without one
the shell is wired to pipes, because a pty's line discipline rewrites the bytes
passing through it — right for a terminal, wrong for `ssh host cat file`.
`ssh host 'cat /bin/edos-wm'` therefore returns the file byte for byte.

`window-change` reaches the pty through the winsize ioctl, so a full-screen
program redraws when the client's terminal is resized.

`SSH_MSG_CHANNEL_EOF` closes the shell's stdin on the pipe path, which is what
lets `ssh host 'cat > file'` finish. On a pty there is nothing to close: the
master is also the output, and a terminal's input does not end while it is open.

## What it does not do

- Public-key authentication. Only `password`.
- More than one session channel per connection, and no port forwarding, no
  agent forwarding, no X11, no subsystems (so no SFTP or `scp`). A shell and a
  command are what a session channel here can be.
- `chacha20-poly1305@openssh.com`, which would be one cipher fewer to negotiate
  and slightly faster than `aes128-ctr` on this hardware.
- Rekeying is implemented and a client-initiated `KEXINIT` mid-session is
  handled, but the server never asks for one itself.
- Constant-time rejection of an implausible packet length. With `aes128-ctr`
  the length is only readable after decryption, and a length outside the
  allowed range ends the connection before the MAC is checked — so an on-path
  attacker who flips bits in the first ciphertext block can tell "closed at
  once" from "still reading", at a cost of one connection per guess. That is
  the CVE-2008-5161 shape. Closing it means failing identically either way:
  read a fixed number of further bytes, run the MAC regardless, and reject
  with the same timing and the same visible behaviour.

## Testing it

```
make ssh-check
```

`scripts/ssh-check` boots the guest, writes an `/etc/sshd.conf`, starts the
server, and drives the host's own OpenSSH client against it: authentication and
a refused password, exit status for 0/1/42, ~10 MB out and 629 KB in with the
SHA-256 compared either way, and three concurrent sessions. Testing a protocol
against anything other than the implementation everyone else uses proves very
little, which is why the client here is the real one.

The check does **not** cover the flow-control bug that motivated the download
case, and that is established rather than assumed: the defect (closing the
channel while the client's window is shut) was reintroduced twice and
`ssh-check` stayed green both times. `ssh` writes straight to a file and drains
as fast as the server sends, so the window never closes and the buggy path is
never entered; the payload being larger than the client's 2 MiB starting window
is not sufficient. Covering it needs a client that stops reading mid-transfer
while the guest's command finishes and exits. An attempt at that -- stalling
the read from a pipe -- failed its own control by going red against a correct
server, and was removed rather than left in looking like coverage.

The transport lives in `scripts/sshdrive.py` and is reusable: `run(command)`
gives back a real exit status and real stdout, which every other check in this
repo currently has to reconstruct by typing at the terminal and grepping
`run_log.txt` for a marker it asked the command to print. Readiness is the SSH
banner rather than a log line, because QEMU's user-mode forward accepts a host
connection before it knows whether anything in the guest is listening.

By hand, the same thing:

```
make run-headless
scripts/edos-vm click 600 400
scripts/edos-vm type 'sshd -p 23 -u edgar:hunter2 -v' --enter
ssh -p 2323 -o StrictHostKeyChecking=no edgar@127.0.0.1
```

## Note on in-kernel crypto

There is none, and the reason is not only the usual one. The kernel is built
`-sse,+soft-float` and never touches a vector register — that is the premise of
its FXSAVE-only context switch — so a kernel SHA-256 would be scalar, while
userspace gets the SSE2 backends: 217 MiB/s SHA-256 and 402 MiB/s
ChaCha20-Poly1305, measured in the guest. An `AF_ALG`-style interface would
hand userspace a slower primitive and charge a syscall per call. If the kernel
ever needs crypto for itself (block checksums beyond CRC32, a signed loader),
that is an internal crate, not a userspace-facing API.
