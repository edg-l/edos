# Dump EDOS thread registry states from a running QEMU gdbstub.
#
# Usage:
#   1. Boot EDOS with `-s` (gdbstub on :1234) — all `make run*` targets do
#      this except `make run-emu`. Any target that uses KVM or TCG with `-s`
#      works.
#   2. While QEMU is running (hung or not), from the edos-v2 repo root:
#        rust-gdb -q -batch -x tools/debug/dump_threads.gdb \
#            kernel/target/x86_64-unknown-none/debug/edos-kernel
#
# The script force-registers the Rust `BTreeMap` pretty-printer (rust-gdb's
# auto-load does not always attach for kernel ELFs that lack
# `.debug_gdb_scripts`), then iterates `THREADS.map` and prints each
# thread's state, CPU, kind (user vs kernel), exit code, and name.
#
# Handy for diagnosing missed-wakeup / scheduler-class hangs.  All four
# CPUs halted in `Scheduler::run_idle` + a Parked user thread with no
# pending wake = the classic symptom.

target remote :1234
set pagination off
set confirm off

python

import sys

RUST_ETC = "/data2/edgar/edos-programs/toolchain/edos/lib/rustlib/etc"
sys.path.insert(0, RUST_ETC)

import gdb_lookup
gdb.printing.register_pretty_printer(gdb.current_objfile(), gdb_lookup.printer)

STATES = {0: "Ready", 1: "Running", 2: "Sleeping", 3: "Parked", 4: "Waking", 5: "Dying"}

# Mirror of kernel/src/syscalls/mod.rs SYS_* constants. NO_SYSCALL = u32::MAX
# is the sentinel for "thread has not entered a syscall yet" (kthreads).
NO_SYSCALL = 0xFFFFFFFF
SYSCALLS = {
    0: "READ", 1: "WRITE", 2: "OPEN", 3: "CLOSE", 4: "LIST_DIR",
    5: "GETCWD", 6: "CHDIR", 7: "POLL", 8: "FSTAT", 9: "MMAP",
    10: "STAT", 11: "MUNMAP", 12: "LSEEK", 13: "FTRUNCATE", 14: "FSYNC",
    15: "ISATTY", 16: "IOCTL", 22: "PIPE", 32: "DUP", 33: "DUP2",
    34: "MSYNC", 39: "GETPID", 40: "WAIT_PID", 57: "SPAWN", 60: "EXIT",
    82: "RENAME", 162: "SYNC", 202: "MOUNT", 203: "LIST_PARTITIONS",
    204: "MKDIR", 205: "RMDIR", 206: "RMDIR_ALL", 207: "UNLINK",
    208: "LIST_MOUNTS", 209: "SLEEP_MS", 210: "MONOTONIC_TIME",
    211: "CLONE", 212: "FUTEX_WAIT", 213: "FUTEX_WAKE", 214: "GETRANDOM",
    215: "SHM_CREATE", 216: "SHM_MAP", 217: "SHM_UNMAP", 218: "SHM_DESTROY",
    219: "WINDOW_CREATE", 220: "WINDOW_DESTROY", 221: "WINDOW_SET",
    222: "WINDOW_GET", 223: "WINDOW_POLL", 224: "WINDOW_LIST",
    225: "WINDOW_SEND_EVENT", 226: "CLOCK_GETTIME", 227: "OPENPTY",
    228: "SPAWN2", 229: "KILL", 230: "SIGACTION", 231: "SHM_SIZE",
    232: "WINDOW_DAMAGE", 240: "SOCKET", 241: "BIND", 242: "CONNECT",
    243: "LISTEN", 244: "ACCEPT", 245: "SENDTO", 246: "RECVFROM",
    247: "SHUTDOWN", 248: "SETSOCKOPT", 249: "PING", 250: "NETINFO",
    251: "GETSOCKOPT", 252: "GETPEERNAME", 253: "GETSOCKNAME",
    254: "STATFS", 255: "FORK",
}

def syscall_name(n):
    if n == NO_SYSCALL:
        return "-"
    return SYSCALLS.get(n, f"#{n}")

def atom(v):
    """Unwrap core::sync::atomic::Atomic<T> -> raw T.

    Atomic<T>.v          : UnsafeCell<AlignN<T>>
    UnsafeCell<AlignN<T>>.value : AlignN<T>
    AlignN<T> is a tuple struct, so raw T is at field `__0`.
    """
    return int(v["v"]["value"]["__0"])

def arc_inner(arc):
    """Arc<T> -> T ref."""
    return arc["ptr"]["pointer"].dereference()["data"]

def arc_string(arc):
    """Arc<String> -> str via pretty-printer."""
    try:
        inner = arc_inner(arc)
        pp = gdb.default_visualizer(inner)
        if pp is not None:
            return str(pp.to_string()).strip('"')
        return str(inner).strip('"')
    except Exception as e:
        return f"<err:{e}>"

def main():
    try:
        registry = gdb.parse_and_eval(
            "edos_kernel::thread::thread::THREADS.map.data.value"
        )
    except gdb.error as e:
        print(f"could not read THREADS: {e}")
        return

    pp = gdb.default_visualizer(registry)
    if pp is None:
        print("no BTreeMap visualizer - did rust-gdb load gdb_providers?")
        return

    entries = list(pp.children())
    print(f"\nTHREADS BTreeMap size={len(entries)//2}\n")
    print(f"{'TID':>4}  {'STATE':<9}  {'CPU':>3}  {'KIND':<6}  {'WP':>2}  {'SYSCALL':<14}  NAME")
    print("-" * 80)

    i = 0
    while i < len(entries):
        tid = int(str(entries[i][1]).split("(")[-1].rstrip(")"))
        arc = entries[i + 1][1]
        i += 2
        try:
            t = arc_inner(arc)
            state = STATES.get(atom(t["state"]), "?")
            cpu = atom(t["cpu"])
            wp = atom(t["wake_pending"])
            wp_str = "1" if wp else "0"
            sysno = atom(t["last_syscall"]) & 0xFFFFFFFF
            sys_str = syscall_name(sysno)
            # user: Option<Arc<RwLock<UserThread>>>  -> kind
            try:
                is_user = t["user"]
                # Probe: tagged-enum representation differs by niche; just check
                # whether the discriminant / inner is Some.
                kind = "user" if "None" not in str(is_user)[:30] else "kernel"
            except Exception:
                kind = "?"
            name = arc_string(t["name"])
            print(f"{tid:>4}  {state:<9}  {cpu:>3}  {kind:<6}  {wp_str:>2}  {sys_str:<14}  {name[:30]}")
        except Exception as e:
            print(f"{tid:>4}  err: {e}")

main()
end

detach
quit
