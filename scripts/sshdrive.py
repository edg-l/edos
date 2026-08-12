"""Run commands in the guest over SSH, and get stdout and an exit status back.

The alternative, and what every other check here still does, is
`scripts/edos-vm type '...' --enter` followed by grepping `run_log.txt`. That
works but it is indirect: the terminal echoes what was typed, the log carries
ANSI escapes and interleaves every other thread's output, a command's exit
status is only visible if the command is written to print it, and each step
needs a `settle` guess because there is no way to know when a command finished.

Over SSH none of that applies. A command's output is its own stream, its status
is a number, and the call returns when the command does.

This is not a replacement for driving the terminal, and existing checks are not
worth converting for its own sake. Use whichever suits the thing being tested:

- SSH suits running a command and caring what it printed or what it returned,
  and moving a file either way.
- The terminal suits everything SSH cannot reach: anything before `sshd` is up,
  a power cut (the connection dies with the machine), a graphical guest, and
  anything whose evidence is kernel log output rather than a command's own.

Requires `sshd` running in the guest with a known password; [`start_sshd`] does
that through `/etc/sshd.conf`. The guest is reachable because `scripts/edos-vm`
forwards a host port to guest port 23 (see `--ssh-fwd`).
"""

import os
import pty
import select
import shutil
import socket
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from vmdrive import type_line  # noqa: E402

# What `scripts/edos-vm` forwards to guest port 23 by default.
PORT = 2323
USER = "edgar"
# Only ever reaches this guest, over a loopback forward, and the guest forgets
# it on the next boot: `/etc` lives on the RAM-backed live root.
PASSWORD = "edos-test"

SSH_OPTS = [
    "-p", str(PORT),
    "-o", "StrictHostKeyChecking=no",
    "-o", "UserKnownHostsFile=/dev/null",
    # The host key is generated fresh on every boot, so a client that remembers
    # keys would refuse the second one. `PubkeyAuthentication=no` keeps the
    # client from offering the host's own identities before the password.
    "-o", "PubkeyAuthentication=no",
    "-o", "PreferredAuthentications=password",
    "-o", "LogLevel=ERROR",
]


class SshUnavailable(RuntimeError):
    """No usable ssh client on this host."""


def require_ssh():
    if shutil.which("ssh") is None:
        raise SshUnavailable("no ssh client on this host")


def ssh(args, stdin_path=None, stdout_path=None, password=PASSWORD, prompts=1, timeout=180):
    """Run `args` in the guest. Returns `(status, transcript)`.

    `status` is what `waitpid` reports for the client, so the command's own
    code is `status >> 8` — [`exit_code`] does that. `transcript` is everything
    the client wrote to its terminal, which is the command's output unless
    `stdout_path` redirected it.

    The password goes in over a pty because OpenSSH reads it from the terminal
    on purpose, and there is no askpass here. `stdin_path`/`stdout_path` keep a
    binary payload out of the transcript, so a transfer can be compared byte
    for byte.
    """
    cmd = ["ssh", *SSH_OPTS, "-o", f"NumberOfPasswordPrompts={prompts}",
           f"{USER}@127.0.0.1", *args]

    pid, fd = pty.fork()
    if pid == 0:
        if stdin_path:
            os.dup2(os.open(stdin_path, os.O_RDONLY), 0)
        if stdout_path:
            os.dup2(os.open(stdout_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644), 1)
        os.execvp(cmd[0], cmd)

    transcript = b""
    answered = 0
    deadline = time.time() + timeout
    while time.time() < deadline:
        ready, _, _ = select.select([fd], [], [], 1.0)
        if not ready:
            continue
        try:
            chunk = os.read(fd, 65536)
        except OSError:  # the pty closes when ssh exits
            break
        if not chunk:
            break
        transcript += chunk
        # One answer per prompt, so a rejected password is not retried with the
        # same one until the server's attempt budget runs out.
        while transcript.count(b"password: ") > answered:
            os.write(fd, password.encode() + b"\n")
            answered += 1

    os.close(fd)
    _, status = os.waitpid(pid, 0)
    return status, transcript.decode("utf-8", errors="replace")


def exit_code(status):
    """The command's own exit status, from what `waitpid` reported."""
    return status >> 8


def run(command, **kw):
    """`ssh(command)` for a single shell command string, as a convenience."""
    return ssh([command], **kw)


def check(command, **kw):
    """Run `command` and return its output, raising if it failed."""
    status, transcript = run(command, **kw)
    code = exit_code(status)
    if code != 0:
        raise RuntimeError(f"{command!r} exited {code}:\n{transcript[-2000:]}")
    return transcript


def banner(timeout=2):
    """The guest's SSH identification string, or None if it is not serving.

    Read rather than merely connecting: QEMU's user-mode forward accepts a host
    connection before it knows whether anything in the guest is listening, so a
    successful `connect` proves nothing. RFC 4253 has the server send its
    identification first, which makes the banner the cheapest real proof.
    """
    try:
        with socket.create_connection(("127.0.0.1", PORT), timeout=timeout) as sock:
            sock.settimeout(timeout)
            line = sock.recv(255)
    except OSError:
        return None
    return line.decode("utf-8", errors="replace").strip() or None


def wait_until_serving(timeout=30, poll=1.0):
    """Wait for the guest to answer with an SSH banner. Returns it."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        found = banner()
        if found and found.startswith("SSH-"):
            return found
        time.sleep(poll)
    return None


def start_sshd(user=USER, password=PASSWORD, timeout=30):
    """Write `/etc/sshd.conf` in the guest and start the server from it.

    Typed at the terminal, because this is the one step that cannot go over
    SSH. Going through the config file rather than command-line flags is also
    what an installed system does, and proves the `enabled_by` gate opens.

    Backgrounded with its output on `/dev/klog`, so the terminal stays usable
    and anything the server logs is still recoverable from the serial log. Note
    that a server started from the terminal writes to the terminal, not to the
    serial log, which is why the redirect is needed at all; one started by
    `edos-init` inherits init's console and needs none.

    Readiness is the banner rather than a log line, so this waits on the thing
    the caller actually needs.
    """
    type_line("echo port 23 > /etc/sshd.conf", settle=1)
    type_line(f"echo user {user} {password} >> /etc/sshd.conf", settle=1)
    type_line("sshd -v > /dev/klog 2>&1 &", settle=2)
    found = wait_until_serving(timeout)
    if found is None:
        raise RuntimeError("sshd did not answer on the forwarded port")
    return found
