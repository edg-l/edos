# Dump EDOS thread registry states from a running QEMU gdbstub.
#
# Usage:
#   1. Boot EDOS with `-s` (gdbstub on :1234) — all `make run*` targets do
#      this except `make run-emu`. Any target that uses KVM or TCG with `-s`
#      works. A guest started by `scripts/edos-vm` opens one on demand:
#        scripts/edos-vm qmp human-monitor-command \
#            '{"command-line":"gdbserver tcp::1234"}'
#   2. While QEMU is running (hung or not), from the edos-v2 repo root:
#        rust-gdb -q -batch -x tools/debug/dump_threads.gdb \
#            kernel/target/x86_64-unknown-none/debug/edos-kernel
#
# The script walks `THREADS.map` itself rather than through the toolchain's
# BTreeMap pretty-printer, which cannot unwrap a `ManuallyDrop` that holds a
# `MaybeDangling`, and prints each thread's state, CPU, kind (user vs kernel),
# pending wake, last syscall and name. It reads the syscall names from the
# kernel source, so run it from the repo root.
#
# Handy for diagnosing missed-wakeup / scheduler-class hangs.  All four
# CPUs halted in `Scheduler::run_idle` + a Parked user thread with no
# pending wake = the classic symptom.

target remote :1234
set pagination off
set confirm off

python

import re

STATES = {0: "Ready", 1: "Running", 2: "Sleeping", 3: "Parked", 4: "Waking", 5: "Dying"}

# Syscall names come from the tree's one list, not a copy of it: the numbers
# are the `SYS_*` consts in kernel/src/syscalls/mod.rs and the names are the
# rows of `syscall_table!` in kernel/src/syscalls/table.rs. The kernel's own
# `SYSCALLS` static is optimised out of the image, so gdb cannot read it.
# NO_SYSCALL = u32::MAX is the sentinel for "has not entered a syscall yet".
NO_SYSCALL = 0xFFFFFFFF

def load_syscalls():
    with open("kernel/src/syscalls/mod.rs") as f:
        numbers = dict(re.findall(r"const (SYS_[A-Z0-9_]+): u64 = (\d+);", f.read()))
    with open("kernel/src/syscalls/table.rs") as f:
        rows = re.findall(r"(SYS_[A-Z0-9_]+), \"([a-z0-9_]+)\"", f.read())
    return {int(numbers[c]): name for c, name in rows if c in numbers}

SYSCALLS = load_syscalls()

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
    """Arc<String> -> str, read from the Vec's buffer and length directly."""
    try:
        vec = arc_inner(arc)["vec"]
        ptr = vec["buf"]["inner"]["ptr"]["pointer"]["pointer"]
        n = int(vec["len"])
        data = gdb.selected_inferior().read_memory(int(ptr), n)
        return bytes(data).decode("utf-8", "replace")
    except Exception as e:
        return f"<err:{e}>"

def is_some_arc(opt):
    """Option<Arc<T>> -> bool. Arc's pointer is non-null, so None is the
    all-zero niche and the first word says which variant is live."""
    word = gdb.lookup_type("u64").pointer()
    return int(opt.address.cast(word).dereference()) != 0

def maybe_uninit(v):
    """MaybeUninit<T> -> T, through `value: ManuallyDrop<T>` and, on toolchains
    whose ManuallyDrop holds a `MaybeDangling<T>`, that wrapper's `__0`."""
    v = v["value"]["value"]
    if "MaybeDangling<" in str(v.type.strip_typedefs()):
        v = v["__0"]
    return v

def btree_items(map_value):
    """Yield (key, value) from a BTreeMap in order, without the toolchain's
    BTreeMap provider, which cannot unwrap this nightly's child edges."""
    # An empty map is the only one whose root is None.
    if int(map_value["length"]) == 0:
        return
    node_ref = map_value["root"]["Some"]["__0"]
    height = int(node_ref["height"])
    leaf_ptr = node_ref["node"]["pointer"]
    leaf_type = leaf_ptr.type.target()
    internal_type = gdb.lookup_type(str(leaf_type).replace("LeafNode<", "InternalNode<", 1))

    def walk(ptr, h):
        leaf = ptr.dereference()
        n = int(leaf["len"])
        edges = ptr.cast(internal_type.pointer()).dereference()["edges"] if h > 0 else None
        for i in range(n):
            if h > 0:
                yield from walk(maybe_uninit(edges[i])["pointer"], h - 1)
            yield maybe_uninit(leaf["keys"][i]), maybe_uninit(leaf["vals"][i])
        if h > 0:
            yield from walk(maybe_uninit(edges[n])["pointer"], h - 1)

    yield from walk(leaf_ptr, height)

def main():
    try:
        registry = gdb.parse_and_eval(
            "edos_kernel::thread::thread::THREADS.map.inner.data.value"
        )
    except gdb.error as e:
        print(f"could not read THREADS: {e}")
        return

    entries = list(btree_items(registry))
    print(f"\nTHREADS size={len(entries)}\n")
    print(f"{'TID':>4}  {'STATE':<9}  {'CPU':>3}  {'KIND':<6}  {'WP':>2}  {'SYSCALL':<14}  NAME")
    print("-" * 80)

    for key, arc in entries:
        tid = int(key["__0"])
        try:
            t = arc_inner(arc)
            state = STATES.get(atom(t["state"]), "?")
            cpu = atom(t["cpu"])
            wp = atom(t["wake_pending"])
            wp_str = "1" if wp else "0"
            sysno = atom(t["last_syscall"]) & 0xFFFFFFFF
            sys_str = syscall_name(sysno)
            kind = "user" if is_some_arc(t["user"]) else "kernel"
            name = arc_string(t["name"])
            print(f"{tid:>4}  {state:<9}  {cpu:>3}  {kind:<6}  {wp_str:>2}  {sys_str:<14}  {name[:30]}")
        except Exception as e:
            print(f"{tid:>4}  err: {e}")

main()
end

detach
quit
