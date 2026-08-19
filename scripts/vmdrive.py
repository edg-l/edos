"""Shared helpers for scripts that drive the guest through `scripts/edos-vm`.

Everything here assumes the guest constraints documented in
`doc/vm-control.md`: the mouse is relative-only so a window must be clicked
before it accepts keystrokes, and the serial log is a byte stream carrying ANSI
escapes rather than text.
"""

import importlib.machinery
import importlib.util
import os
import subprocess
import sys
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
VM = os.path.join(REPO, "scripts", "edos-vm")
SERIAL = os.path.join(REPO, "run_log.txt")

# `edos-vm` has no .py suffix, so it is loaded by path rather than imported.
# Doing so keeps the guest slot's lock file and its ownership rules defined
# once, on the side that every other caller -- a `make` rule, a shell -- also
# goes through. Its module body only defines things; `main()` is guarded.
# The loader is named explicitly because `spec_from_file_location` decides by
# suffix and returns None for a file that has none.
_spec = importlib.util.spec_from_loader(
    "edos_vm", importlib.machinery.SourceFileLoader("edos_vm", VM))
edos_vm = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(edos_vm)

# Importing this module means driving the guest, and there is one to drive.
# Held for the lifetime of the gate process; see `edos_vm.claim_slot`.
_slot = edos_vm.claim_slot(" ".join(sys.argv))

# The desktop takes this long to reach a shell prompt on a warm host.
BOOT_SECONDS = 26


def run(cmd, **kw):
    """Run a command, forwarding its stderr if it fails.

    `capture_output` is what lets a caller read the serial tail out of
    stdout, and it also swallows the traceback of whatever went wrong
    inside `edos-vm`. A gate that reports only `returned non-zero exit
    status 1` costs a whole re-run to find out what it was.
    """
    try:
        return subprocess.run(cmd, check=True, capture_output=True, text=True, **kw)
    except subprocess.CalledProcessError as e:
        if e.stderr:
            sys.stderr.write(e.stderr)
        if e.stdout:
            sys.stderr.write(e.stdout)
        raise


def vm(*args):
    return run([VM, *args])


def start(*args):
    """Start the guest, retiring whatever the last run left behind.

    A harness that aborts mid-run -- a timeout, a killed session -- leaves a
    guest holding the qcow2 write lock and the 2323 host forward, and the next
    `start` then dies inside QEMU with no hint that a stale guest is the cause.
    `sata-disk.img` retires the guest for the same reason before it rebuilds.
    """
    vm("stop")
    return vm("start", *args)


def boot(disk=None):
    """Boot the guest and give the terminal keyboard focus."""
    start(*(("--disk", disk) if disk else ()))
    time.sleep(BOOT_SECONDS)
    vm("click", "400", "300")
    time.sleep(1)


def type_line(text, settle=3):
    vm("type", text, "--enter")
    time.sleep(settle)


def serial_mark():
    return os.path.getsize(SERIAL)


def serial_tail(since):
    # Read as bytes: `since` is a byte offset, and the log carries ANSI escapes
    # and multi-byte sequences, so slicing a decoded string drops the tail.
    with open(SERIAL, "rb") as f:
        return f.read()[since:].decode("utf-8", errors="replace")


def wait_for(since, needle, timeout, poll=3):
    """Wait for `needle` to appear in the serial log after `since`.

    Returns the log tail once it appears, or None on timeout.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        out = serial_tail(since)
        if needle in out:
            return out
        time.sleep(poll)
    return None
