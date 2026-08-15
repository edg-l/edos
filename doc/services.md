# Services

`bin/edos-init` is the only process the kernel starts. Everything about what a
session is lives there rather than in the kernel: it spawns each service, waits
for it, restarts it on its own schedule, and hands out the window-management
privilege the compositor and the panel need.

## What runs

Two sources, and the second only ever adds to the first.

**The desktop session is compiled in** — `edos-wm`, `edos-taskbar`,
`edos-terminal`, `sshd`, `lookupd` — so a filesystem with nothing on it still
boots to a usable machine, and a mistake in a config file cannot cost the
session its window manager.

**`/etc/services/*.conf` declares the rest.** One file per service, named for
it: `/etc/services/httpd.conf` declares `httpd`. A file naming a compiled-in
service replaces it, which is how the session is reconfigured without editing
the program.

```
# /etc/services/httpd.conf
command     /bin/httpd
args        -p 8080 /var/www
essential   no
shell       no
requires    /dev/eth0
enabled_by  /etc/httpd.conf
```

| Keyword | Meaning |
|---|---|
| `command` | the binary to run; the only required keyword |
| `args` | arguments, split on whitespace, not counting `argv[0]` |
| `essential` | `yes` if the session is not usable without it. Only changes how giving up is reported: nothing here reboots the machine |
| `shell` | `yes` to grant the privilege to move, resize, frame and focus other processes' windows. Granted per spawn, since it is per pid and dies with the process |
| `requires` | device nodes that must exist before the first spawn is worth trying |
| `enabled_by` | a file whose absence means the service is not configured, and so is not started at all |
| `restart` | `always` (the default), `on-failure`, or `never`. What to do when the service exits |

`requires` and `enabled_by` answer different questions and are not
interchangeable. `requires` is a race: drivers register their `/dev` entries
from kthreads, so a node appears some time after userspace starts, and a service
that opens one during startup would otherwise die before its driver arrived.
Init waits up to 15 seconds and then starts the service anyway, because a
service that treats the device as optional comes up permanently without it.

`enabled_by` is a decision. `sshd` uses it: the server's one credential is a
plaintext line in `/etc/sshd.conf`, so a system without that file has no
business listening, and starting it anyway would only log a failure on every
boot of a machine whose owner never asked for it.

`lookupd` uses it the other way round, as an off switch: the image ships
`/etc/lookupd.conf`, so the caching resolver runs unless that file is deleted,
and a system without it resolves names exactly as it did before the daemon
existed. See `doc/design/lookupd.md`.

A file that does not parse is reported and skipped. Init coming up without one
service beats init not coming up.

## Restarts

A service that exits is restarted. One that ran for at least ten seconds first
is treated as having done its job and exited — the ordinary case for a terminal
the user closed — and restarts promptly with its failure count cleared. One that
dies faster than that is failing to start, and backs off 100 ms, 250, 500, 1 s,
2 s, 5 s. After five rapid failures init gives up and leaves it `failed`, rather
than pinning a CPU respawning a binary that crashes on startup.

Giving up is not final: `svc start` clears the failure count and wakes the
supervisor.

## Controlling them: `svc`

```
svc list                 what every service is doing
svc status <service>     one service, in full
svc start <service>      start it, and clear its failure count
svc stop <service>       stop it, and do not restart it
svc restart <service>    stop it if it is up, then start it
```

```
/ $ svc list
SERVICE         STATE        PID  FAILURES
edos-wm         running       26  0
edos-taskbar    running       27  0
edos-terminal   running       25  0
sshd            failed         -  0
```

States are `waiting` (for the devices it requires, before its first spawn),
`running`, `stopped` (told to be, and not to be restarted), `backoff` (down
between restarts) and `failed` (given up on, or never configured).

`svc stop` signals the service with `SIGTERM` and clears its "wanted up" flag
before the signal, so the supervisor's `waitpid` returns to a service that is
already meant to be down and parks instead of restarting it. A stopped service
costs nothing: its supervisor thread waits on a condition variable rather than
polling.

## How control works

Commands go to init on a FIFO at `/var/run/svc.ctl`, one `<command> <name>` line
each. Answers come back the other way as state on disk: init rewrites
`/var/run/svc.status` whenever anything changes, and `svc list` and `svc status`
are ordinary reads of it.

This is daemontools' and runit's shape, for the reason they chose it. The
process that owns `waitpid` and the restart backoff is the only one that can act
on a service, so control has to be a message to it rather than work the caller
does. That is also why `svc` is as small as it is: everything it does is either
a line written to a pipe or a read of a file.

Two details are load-bearing:

**Init holds the FIFO open `O_RDWR`.** With only a read end, every `svc` that
finished and closed its side would put the pipe at end of file, and init's read
loop would spin on a hangup nothing was going to clear. Its own write end means
end of file cannot happen.

**`svc` opens the write end `O_NONBLOCK`.** Opening a FIFO for writing normally
waits for a reader; against a control FIFO left behind by an init that is no
longer running, that wait would never end. Non-blocking makes it `ENXIO`
instead, which is the case POSIX defines the error for.

Replies are a file rather than a second channel because a FIFO carries one
direction, and giving every caller a private reply channel would be a lot of
machinery for something a file already says. The status file is written whole
to `.new` and renamed over, so a reader polling it never sees half a table.

There is no dependency graph and there are no runlevels. runit gets by without
them, and this system has exactly one real ordering constraint — device nodes —
which `requires` already covers.

## Named pipes

The control channel needs `mkfifo`, which the kernel grew for this and which is
worth having on its own: two programs with no common parent cannot otherwise
make a pipe between them at all, since an anonymous pipe can only be inherited
across `spawn` and there is no `AF_UNIX`.

```
/ $ mkfifo /tmp/f
/ $ cat /tmp/f &
/ $ echo hello > /tmp/f
hello
```

Both EFS and memfs store them; see `doc/efs.md` for the on-disk type. The
interesting semantics are in `open` rather than in the transfer, and they are
documented at the top of `kernel/src/fs/fifo.rs`: opening one end waits for the
other, `O_NONBLOCK` turns the reader's wait into an immediate success and the
writer's into `ENXIO`, and `O_RDWR` is not a rendezvous at all. `O_NONBLOCK` is
recorded on the descriptor the open returns, so it governs the transfer too: a
read with nothing to read and a write with no room report `EAGAIN` rather than
waiting, and `fcntl(F_SETFL)` changes it afterwards.

## What happens when a service exits

`restart` is what separates a process that *failed* from one that *finished*.
Init used to make no distinction: the exit code was logged and never read, so a
window the user closed and a process that died both came back. A terminal you
closed reappeared, and because a service that had run for more than ten seconds
also has its failure count reset, it reappeared with no backoff and no limit.

- **`always`** — the default, and right for anything the session cannot be used
  without. `edos-wm` and `edos-taskbar` take it: a desktop with no compositor is
  not a desktop, whatever the compositor thought it was doing when it stopped.
- **`on-failure`** — comes back if it failed, stays gone if it finished.
  `edos-terminal` takes it. Closing the window exits 0 and is taken at its word;
  the panel menu is how another one is opened. A terminal that is killed or
  crashes still comes back.
- **`never`** — only `svc start` will run it again.

A service that is not restarted is left `stopped` with `want_up` false, which is
the same state `svc stop` leaves it in, so `svc start` is how you change your
mind either way.
