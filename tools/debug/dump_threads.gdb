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
    print(f"{'TID':>4}  {'STATE':<9}  {'CPU':>3}  {'KIND':<6}  {'EXIT':>5}  {'CREATED':>10}  NAME")
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
            exit_code = atom(t["exit_code"])
            created = atom(t["created_at_tick"])
            # user: Option<Arc<RwLock<UserThread>>>  -> kind
            try:
                is_user = t["user"]
                # Probe: tagged-enum representation differs by niche; just check
                # whether the discriminant / inner is Some.
                kind = "user" if "None" not in str(is_user)[:30] else "kernel"
            except Exception:
                kind = "?"
            name = arc_string(t["name"])
            print(f"{tid:>4}  {state:<9}  {cpu:>3}  {kind:<6}  {exit_code:>5}  {created:>10}  {name[:30]}")
        except Exception as e:
            print(f"{tid:>4}  err: {e}")

main()
end

detach
quit
