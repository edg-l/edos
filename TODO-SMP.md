# SMP Readiness and TODOs

This document summarizes the current state of the kernel with respect to SMP (multi‑core) support and lays out a concrete plan to bring the system to correct, stable SMP operation. It highlights gaps, risks, and actionable work items, with file references to speed up implementation.

## Status Snapshot (today)

- Per‑CPU data: Present and used.
  - Template + slots via linker (`.percpu_tpl`, `.percpu`) and helpers in `kernel/src/util/per_cpu.rs:1`.
  - Accessors compute a per‑CPU base as `percpu_start + id * stride`. CPU id comes from CPUID x2APIC ID.
  - Risk: The reserved slots are fixed at 64 in `kernel/linker-x86_64.ld:57`. x2APIC IDs can be sparse and >63; indexing by raw APIC ID will overflow the reserved array. A compact APIC‑ID→CPU‑index map is required.

- GDT/TSS/IST: Global GDT with a per‑CPU TSS pointer baked in at creation time.
  - `kernel/src/gdt.rs:91` creates a single static `GDT: Lazy<...>` and then `init_tss()` allocates IST stacks and writes into the current CPU’s `pcpu.tss`.
  - `kernel/src/gdt.rs:99` appends a TSS descriptor with `&get_percpu_data().tss` at Lazy init time.
  - Risk: On APs, `GDT` is already initialized and its TSS descriptor still points to the BSP’s TSS; `load_tss` loads the wrong TSS. IST stacks become shared → instant corruption under load.

- IDT: Single static table initialized in `kernel/src/interrupts/idt.rs:1` and loaded in `kernel/src/interrupts/mod.rs:26`.
  - OK to share one IDT across CPUs, but each CPU must execute `lidt` (load) and rely on its own current TSS for IST stacks. Today this only runs on the BSP.

- Syscalls / GS base: Per‑CPU GS base is set in `kernel/src/syscalls/mod.rs:36` and `setup_syscall()`; must run on every CPU.

- Local APIC / IOAPIC / MSI:
  - LAPIC is enabled in `kernel/src/apic/init.rs:37` (BSP only). IOAPIC routing targets the current LAPIC ID at setup time (BSP), and MSI programs message address to the current LAPIC ID (`kernel/src/drivers/msi/mod.rs:38`).
  - Risk: With SMP, interrupts may all land on BSP unless reprogrammed per device/CPU. Need per‑CPU affinity and/or balancing.

- Timer:
  - APIC timer calibration is a global `Once` in `kernel/src/timer.rs`; currently fine for uniform cores, but each CPU must enable and program its LAPIC timer for scheduling preemption.

- Scheduler:
  - One `Scheduler` instance per CPU via `get_percpu_data().scheduler` (`kernel/src/thread/scheduler.rs:50` and `:71`).
  - Run queues use `crossbeam_queue::SegQueue` (lock‑free) per CPU.
  - Wakeups use `send_ipi_self(InterruptIndex::Timer)` (e.g., `thread_yield`, `thread_wake`), with no cross‑CPU reschedule IPI.
  - Risk: Waking a thread that’s on another core’s runqueue does not IPI that core. Driver interrupts can wake threads on the “wrong” core. No migration or load balancing.

- Memory management (MMU):
  - Mapping/unmapping uses `.flush()` locally (`kernel/src/memory/mapper.rs`), but there’s no cross‑CPU TLB shootdown for kernel/global mappings or shared address spaces.
  - Risk: Other CPUs will keep stale TLB entries → use‑after‑free/data corruption.

- Drivers / IRQ handlers:
  - AHCI interrupt handler wakes the AHCI driver thread on the current core (`kernel/src/interrupts/io.rs:10`). IOAPIC entry destinations are programmed to the BSP LAPIC ID during init → IRQ/core affinity mismatch under SMP unless reconfigured.

## High‑Priority Gaps (must fix before enabling multiple CPUs)

1. Per‑CPU index mapping
   - Replace raw x2APIC ID indexing with a compact 0..N‑1 mapping populated from ACPI MADT.
   - Store a table `[max_possible] -> Option<cpu_index>` or a small hash/map from APIC ID → cpu_index. Expose `current_cpu_index()` using APIC ID lookup.

2. Per‑CPU GDT/TSS/IST
   - Build and load a GDT per CPU, with its TSS descriptor pointing at that CPU’s `pcpu.tss`.
   - Allocate IST stacks per CPU during that CPU’s init.
   - Load `CS/SS` and `TSS` on each CPU.

3. Per‑CPU IDT load
   - Execute `IDT.load()` on each CPU after its GDT/TSS is ready.

4. AP bring‑up
   - Implement INIT‑SIPI‑SIPI + trampoline to switch APs into long mode, set up stacks, GS base, per‑CPU area copy, GDT/IDT, LAPIC, timer, and enter scheduler idle loop.

5. Reschedule + TLB IPIs
   - Add a dedicated IPI vector for rescheduling and one for TLB shootdown.
   - When waking a thread on another CPU, send a reschedule IPI to that CPU.
   - On unmap/flag changes that affect other CPUs, broadcast a shootdown IPI with address/range (or fall back to full CR3 reload on target CPUs).

6. IRQ affinity
   - Provide APIs to set IOAPIC and MSI destination LAPIC IDs per device.
   - Ensure driver threads either run on the target CPU (affinity) or the wake path targets the owning runqueue.

## Action Plan (concrete steps)

Phase 0 — Inventory and guards
- Add `SMP_DISABLED` kernel flag and keep it false until Phase 2 passes smoke tests.
- Instrument logs with CPU index prefixes, e.g., `[cpuX]` on serial.

Phase 1 — APIC ID mapping and per‑CPU slots
- Build AP list from ACPI (`kernel/src/acpi/mod.rs`) and create `apic_id_to_cpu_index()`.
- Change `kernel/src/util/per_cpu.rs:60` `get_current_cpu_id()` to return the compact index; keep a separate API to return raw APIC ID when needed.
- Increase or remove the hardcoded 64‑slot assumption in the linker script, or validate N ≤ 64 at boot with a clear panic.

Phase 2 — Per‑CPU GDT/TSS
- Refactor `kernel/src/gdt.rs` to:
  - Create a non‑static `GDT` instance per CPU and load it on that CPU; or
  - Keep static segment selectors, but re‑append a TSS descriptor for each CPU and reload it. Prefer per‑CPU GDT for clarity.
- Allocate IST stacks in a per‑CPU init routine and write into that CPU’s `pcpu.tss`.
- Call sequence per CPU: `build_gdt_this_cpu()` → `load_gdt()` → `load_tss()`.

Phase 3 — IDT per CPU
- Keep a single shared table, but run `IDT.load()` on every CPU after GDT/TSS are live.
- Confirm all handlers that use IST indices are safe with per‑CPU TSS.

Phase 4 — AP bring‑up
- Write a 16/32‑bit trampoline in low memory (1MiB region) to enter long mode and jump to a Rust `ap_start(cpu_index)`.
- BSP path:
  - Enumerate APIC IDs; allocate stacks for each AP; write wakeup vector; send INIT + SIPI(s) to each AP.
- AP path (`ap_start`):
  - `init_this_cpu_percpu()`
  - `setup_gs_base()` and `set_gs_kernel_stack()`
  - Per‑CPU GDT/TSS/IDT init; `interrupts::init()`
  - LAPIC enable; APIC timer enable; `setup_syscall()`
  - Initialize per‑CPU scheduler and enter idle loop.

Phase 5 — Scheduler: cross‑CPU reschedule and wakeups
- Add a reschedule IPI vector and handler to nudge a CPU out of `hlt` into scheduling.
- Add a CPU field to `ThreadId` and runqueue ownership.
- On `thread_wake`, if target CPU != current, enqueue on target runqueue and send reschedule IPI to that CPU.
- Start without migration; add a simple work‑stealing or periodic load balance later.

Phase 6 — TLB shootdown (kernel and userspace)
- Add an IPI for TLB invalidation; handler executes `invlpg` for the provided page(s), or full CR3 reload for that address space.
- Maintain, per address space, a CPU mask of where it’s active; broadcast shootdowns only to those CPUs.
- Use barriers/ack counts to ensure completion before freeing frames.

Phase 7 — IRQ/MSI affinity
- Extend `apic::init::configure_device_interrupt()` to accept destination CPU.
- In MSI setup (`kernel/src/drivers/msi/mod.rs`), program `msg_addr` with desired LAPIC ID, not always the current one.
- Provide a simple policy: pin storage/IO to CPU0 initially; later, distribute.

Phase 8 — Synchronization audit
- Review global structures used in interrupt context or across CPUs (e.g., BTreeMap in `Scheduler`, global device state, frame allocator, VFS) and ensure IRQ‑safe locking and clear lock ordering.
- Replace long critical sections with finer locks or lock‑free queues where needed.
- Ensure serial output lock is IRQ‑safe (currently uses `without_interrupts` and a spin::Mutex — OK as a start).

## Code Hotspots and Fixes

- Per‑CPU indexing
  - `kernel/linker-x86_64.ld:57` reserves `.percpu_stride * 64`; either raise N or add a runtime panic if `number_of_cores() > 64`.
  - `kernel/src/util/per_cpu.rs:59`: replace CPUID‑derived raw ID with `apic_id_to_cpu_index()` mapping.

- GDT/TSS
  - `kernel/src/gdt.rs:91`: static `GDT` causes TSS descriptor to bind to BSP’s `pcpu.tss`.
  - Fix by making GDT per CPU and constructing/loading it on each CPU during AP init.

- IDT
  - `kernel/src/interrupts/mod.rs:26`: ensure `IDT.load()` runs on every CPU after its GDT/TSS is ready.

- LAPIC timers
  - `kernel/src/apic/init.rs:37`: LAPIC enable is BSP‑only; mirror on APs.
  - `kernel/src/apic/mod.rs:24`: provide a per‑CPU enable+program function and call it in AP init.

- Scheduler IPIs
  - `kernel/src/thread/scheduler.rs:386, 405, 424, 441`: `send_ipi_self` used for yield/park/exit; add a path to target another CPU when cross‑CPU wakeup occurs.

- IRQ/MSI affinity
  - `kernel/src/apic/init.rs:149`: accept a CPU parameter for IOAPIC destination.
  - `kernel/src/drivers/msi/mod.rs:38`: program `msg_addr` using the intended CPU’s LAPIC ID.

- TLB shootdown
  - `kernel/src/memory/mapper.rs:*`: mapping/unmapping performs only local `.flush()`; add shootdown broadcasts when changing kernel mappings or address spaces active on other CPUs.

## Testing & Bring‑up Plan (QEMU)

- Start with `-smp 2` and AP logging at boot: BSP prints APIC IDs and cpu_index mapping; each AP announces entering idle loop.
- Verify per‑CPU GDT/TSS by printing `TSS.RSP0` addresses and IST stack tops per CPU.
- Program APIC timers on each CPU; confirm periodic timer interrupts on all CPUs (per‑CPU counters).
- Run basic concurrency stress: rapid `thread_yield` across many threads; confirm no crashes with preemption on multiple CPUs.
- Exercise unmap/map while another CPU performs memory access to validate TLB shootdown.
- Route a device IRQ (e.g., AHCI) to CPU1; ensure driver thread runs/awakens on CPU1 via affinity or cross‑CPU wake.

## Pitfalls & Gotchas

- APIC ID vs per‑CPU slot index: never multiply the linker stride by raw APIC ID; use a compact index. Failing this can corrupt memory outside `.percpu`.
- Global GDT: must not share a TSS descriptor across CPUs; each CPU needs its own TSS and IST stacks.
- Deadlocks: avoid taking non‑IRQ‑safe locks in interrupt handlers; ensure lock ordering is consistent across CPUs.
- Shootdown latency: ensure shootdown IPIs are acknowledged before freeing frames; otherwise UAF.
- Timer calibration: global calibration is acceptable to start, but different cores can drift; prefer per‑CPU LAPIC setup and, optionally, per‑CPU calibration.

## Definition of Done

- All CPUs boot via AP bring‑up, load their own GDT/TSS/IDT, set GS base, enable LAPIC timer, and run the idle loop.
- Preemption ticks fire on all CPUs; scheduler can wake threads across CPUs via reschedule IPIs.
- IRQs can be routed to specific CPUs; driver threads either have matching affinity or wake paths route to the correct runqueue.
- Memory operations that change mappings perform correct TLB shootdowns on affected CPUs.
- No cross‑CPU crashes under I/O, mapping stress, and thread churn; serial logs show correct per‑CPU operation.

---

If helpful, I can scaffold the AP init skeleton (trampoline + AP startup path) and refactor GDT/TSS into a per‑CPU API next.

