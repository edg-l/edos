# NVMe

`kernel/src/drivers/nvme/` drives PCIe NVMe controllers. It mirrors the AHCI
driver's shape rather than inventing a second one: a probe kthread, one
`AsyncBlockDevice` per namespace registered into `block_io`, an MSI-X interrupt
that does nothing but count and wake, a dispatcher kthread that drains the
completion queue, and a watchdog kthread that turns a lost completion into a
counter and a controller reset instead of a hang.

Files:

| file | what is in it |
| --- | --- |
| `mod.rs` | probe, controller struct, completion handling, `retire_op`, status decode |
| `regs.rs` | the controller register block and the opcode/field constants |
| `admin.rs` | controller enable, admin queue setup, Identify, Create I/O SQ/CQ, interrupt configuration |
| `identify.rs` | the Identify Controller and Identify Namespace layouts |
| `queue.rs` | submission and completion queue memory, doorbells, phase tags |
| `namespace.rs` | `AsyncBlockDevice`: `submit_read`, `submit_write`, `submit_flush`, PRP building, bouncing, MDTS splitting |
| `watchdog.rs` | the timeout sweep, the counters, `nvme_timeout_ms` |
| `stats.rs` | `/proc/nvme_stats` |
| `api.rs` | what the rest of the kernel sees: the probe barrier and the namespace list |

## The model as implemented

**Registers.** The controller's BAR0 is mapped uncached. `CAP` supplies the
doorbell stride (`DSTRD`), the maximum queue entries (`MQES`), the minimum host
page size (`MPSMIN`) and the enable/reset timeout (`TO`, in 500 ms units).
Bring-up is the spec's sequence: clear `CC.EN`, wait for `CSTS.RDY == 0`,
program `AQA`/`ASQ`/`ACQ`, set `CC` (IOSQES 6, IOCQES 4, 4 KiB page size), set
`CC.EN`, wait for `CSTS.RDY == 1` bounded by `CAP.TO`. A controller reporting
`MPSMIN != 0` or a doorbell stride that would place a doorbell past the mapping
is refused by name rather than driven wrong.

**Queues.** One admin queue pair of 32 entries and one I/O queue pair of 128,
both allocated from the DMA pool and therefore `NO_CACHE`. Both are sized
against `CAP.MQES`: a controller below the admin pair's 32 entries is refused,
and the I/O pair is clamped down to what `MQES` grants, along with the cid
bitmap that bounds how many commands may be outstanding against it. Asking for
more than `MQES` fails Create I/O CQ with "Invalid Queue Size", which this
driver turns into no I/O queue at all rather than a smaller working one. A submission queue
entry is written, the tail doorbell is rung, and the completion queue is read by
phase tag. Admin commands are **polled** by the thread that issues them; I/O
commands complete on the dispatcher. That split is not a style choice: Identify
runs during init, before the dispatcher is in its loop, and a dispatcher that
also drained the admin CQ would eat the completions a live controller reset is
polling for.

**Interrupts.** MSI-X table entry 0, falling back to a single MSI message, one
IDT vector, and both completion queues created with `CDW11.IV = 0`. The handler bumps `NVME_IRQS_FIRED`, wakes the
dispatcher and EOIs. The dispatcher's park predicate compares the fired count
against the count it last processed, so a completion that lands between the
predicate and the park is not lost.

**Commands.** A command is installed in the slot table under `cmd_slots` before
it is issued, so a completion always finds its op. Reclamation of the cid, the
bounce buffer and the PRP list page happens exactly once, in `retire_op`, called
by the dispatcher on a real completion and by the watchdog when it fails a
command out.

## The nine decisions

**1. 512-byte sectors only.** Everything above `block_io` counts in 512-byte
sectors: `fs/gpt.rs`, `fs/mbr.rs`, `SECTORS_PER_PAGE = 8` in
`fs/block_page_cache.rs`, `fs/efs/`, `fs/fat32/`, `fs/devfs/block.rs`,
`programs/edos-install`, `libs/efs-common` and the host tools. Threading a
sector size through `AsyncBlockDevice` means changing the kernel, the host tools
and userspace together; a driver-internal 4Kn shim means read-modify-write for
the sub-4 KiB writes `write_partial_page` genuinely issues, which is a new
correctness surface inside a new driver. So a namespace whose active LBA format
is not 512 B, or which carries per-block metadata, is logged with the reason and
not registered. `scripts/nvme-check`'s third case is the gate.

**2. Device ids `3000 + controller_index * 64 + (nsid - 1)`.** Above the
ramdisk's 2000, so `select_root_partition`'s `is_live` test keeps preferring a
real disk over the live image. Encoding controller and namespace in the id makes
the devfs name derivable without a side table: `/dev/nvme{c}n{nsid}`. Extending
the `sd*` series was rejected because it would renumber AHCI disks whenever an
NVMe controller appears.

**3. One I/O queue pair, one MSI-X vector.** `msi/mod.rs` programs one table
entry per call with no affinity control, and every vector needs a hand-written
handler in `interrupts/io.rs` plus a variant in the static `InterruptIndex`
enum. N queues is N hand-written handlers, so per-CPU queues are a project, not
a tweak. NVMe permits completion queues to share a vector, which is what makes
one enough. The ladder is MSI-X → MSI, and a controller offering neither is
refused: an NVMe pin interrupt stays level-asserted until the CQ head doorbell
write, which this driver performs on the dispatcher rather than in the handler,
so an INTx line would re-deliver until the dispatcher is scheduled. Masking
through `INTMS`/`INTMC` around each drain would fix that, but no controller the
driver runs on lacks MSI, so the untestable path is refused by name rather than
half-implemented.

**4. Lock ranks 172, 182, 186, 192**, interleaved with AHCI's 170–200 band:
admin (172) → command slots (182) → completion queue (186) → submission queue
(192). `doc/invariants/lock-order.md` carries the rows and the per-lock notes.
`NvmeOp.resources` is a deliberately unranked `spin::Mutex`, listed in that
doc's non-ranked section: it guards a take-exactly-once of the op's reclaimable
resources and is a leaf.

**5. Errors, timeout and reset.** A completion's SCT/SC pair maps to a
`BlockError`; an unmapped pair is logged once. The watchdog kthread sweeps
in-flight commands against `nvme_timeout_ms`, and **drains the completion queue
before declaring anything hung** — that is what distinguishes a lost interrupt
(counted as `watchdog_completions`) from a wedged controller. A real timeout
resets the controller: disable, re-enable, rebuild the queues, and retire every
straggler through `retire_op`. Asserting the slot table is empty at reset time
is wrong and was tried: nothing excludes a submitter from installing a command
in that window.

**6. PRP.** PRP1 plus, above 8 KiB, a PRP list page. The list page is allocated
lazily with `allocate_sized_uninit` and is untouched entirely below 8 KiB,
because DMA memory is mapped `NO_CACHE` and an 8-byte store into it costs about
113 ns. Every page after the first is **translated**, never derived by adding
4096 to the first page's physical address: `init_heap` maps the heap a frame at
a time, so a multi-page kernel buffer is not physically contiguous in general.
A misaligned or unmapped buffer is bounced through a DMA-pool buffer, counted as
`bounced_requests`.

**7. `BlockBuffer` owns or co-owns its backing.** A cancelled command's cid,
bounce buffer and PRP list page stay reserved until the device actually
completes, because an in-flight DMA cannot be retracted.

**8. MDTS splitting lives in the driver.** `AsyncBlockDevice` does not grow a
transfer limit; a request above the controller's maximum is split into several
commands joined by one `SplitOp`, counted as `split_requests` and
`split_commands`. `mdts=1` on the QEMU device is the deliberate red gate.

**9. `submit_read_batch` needs no override.** The default implementation issues
each request through `submit_read`, which is already asynchronous.

## `/proc/nvme_stats`

One line of `key=value` pairs, in the shape of `/proc/ahci_stats`.

| key | meaning |
| --- | --- |
| `controllers`, `namespaces` | how many were probed and registered |
| `irqs` | MSI-X interrupts taken. Zero here with I/O happening means the driver is running on the watchdog, not on completions |
| `dispatcher_passes` | times the dispatcher woke and drained |
| `inflight`, `max_inflight` | commands outstanding now, and the high-water mark |
| `commands_submitted` | total commands issued to the I/O queue |
| `split_requests`, `split_commands` | requests that exceeded MDTS, and the commands they became |
| `bounced_requests` | requests whose buffer was not DMA-addressable and was copied through the pool. A hot path bouncing is visible here rather than only as slowness |
| `flushes`, `flushes_elided` | Flush commands issued, and those skipped because the controller reported no volatile write cache |
| `command_errors` | completions with a non-zero status |
| `prp_pages` | pages a PRP entry addressed beyond PRP1, each one translated separately |
| `prp_pages_discontiguous` | those of them whose frame was not the first page's frame plus the page index. See the gate below: a boot that leaves this at zero has not exercised the translation |
| `watchdog_firings` | sweeps that found a command past its deadline |
| `watchdog_completions` | commands the watchdog found already complete in the CQ — each one is an interrupt that was lost |
| `resets` | controller resets performed |
| `timeout_ms` | the current timeout (a level, not a total) |
| `mdts_bytes` | the first namespace's maximum transfer size |
| `vwc` | 1 if the controller reports a volatile write cache |

`programs/fsbench` samples this file around every run, so an NVMe run's report
carries an `nvme_stats.*` block beside the block-cache and journal deltas.

## The PRP addressing gate

`build_prp` translates every page of a transfer separately, because a virtually
contiguous kernel buffer is not physically contiguous: the heap is mapped by
`map_memory`, a frame at a time. Deriving later pages by adding 4096 to the
first addresses unrelated frames and DMAs into them.

Testing that is harder than it looks. A read that goes through a PRP list and
is compared against the same bytes read one page at a time only catches the
bug **if the buffer it read through was physically discontiguous** — and the
frame allocator scans forward from a hint, so a fresh multi-page heap
allocation at boot is very often one contiguous run, which the naive
derivation gets right by accident. A fixed four-page probe passed with the bug
deliberately reintroduced.

So the gate reports its own discriminating power. `build_prp` counts each page
it translates and, separately, each one whose frame is *not* where the naive
derivation would have looked (`prp_pages_discontiguous` above). The
`nvme_probe_read` probe reads 4 pages, then 16, then 64, then 256 — capped by
MDTS and by the 512 entries one list page holds — until that counter moves,
and only then runs the whole-versus-per-page comparison, logging

```
nvme: PRP gate discriminating: 4 pages via PRP list, 1 of them not where naive
addressing would have looked, matches 4 single-page reads
```

If no candidate size is discontiguous it logs `PRP GATE NOT DISCRIMINATING`
instead of a pass. `edos-nvme.iso` boots with `nvme_probe_read` for exactly
this reason, and `scripts/nvme-check`'s first case asserts on the word
*discriminating*. The counter is derived from the difference between the
translated address and the naive one, so a regression that goes back to
arithmetic drives it to zero: the gate then reports NOT DISCRIMINATING rather
than passing, which is how the reintroduced bug was observed to turn it red.

## Driving it

```bash
make run-nvme                  # SATA root, NVMe disk attached as a second device
make nvme-check                # the four-boot gate

scripts/edos-vm start --iso edos-nvme.iso --nvme-disk nvme-disk.img --no-sata
scripts/edos-vm start --nvme-disk nvme-disk.img --nvme-lbs 4096
```

`edos-nvme.iso` is the ordinary system with `root=` naming `nvme-disk.img`'s
partition GUID. Both ISOs come from the same tracked `limine.conf`; the ISO rule
substitutes the GUID. The two disks carry **different** partition GUIDs on
purpose, so attaching both never makes root selection a race.

### QEMU knobs and the path each one reaches

| knob | what it exercises |
| --- | --- |
| `logical_block_size=4096` | decision 1's refusal path. `scripts/edos-vm --nvme-lbs` |
| `mdts=1` | decision 8's splitter. `mdts` is a power-of-two multiplier of the page size, so `1` gives an 8 KiB ceiling |
| `max_ioqpairs=N` | how many I/O queue pairs Set Features grants. The driver asks for one and reports what it got |
| `msix_qsize=N` | the MSI-X table size, i.e. whether the MSI-X branch of the interrupt ladder is taken at all |
| `num_queues=N` | the older alias of `max_ioqpairs` |
| `-device nvme-ns` | more than one namespace on a controller, which exercises the id arithmetic and the devfs naming |
| `mqes=N` | a controller with a queue-size ceiling below the driver's 128-entry request. `scripts/edos-vm --nvme-mqes`; `--nvme-mqes 63` boots an NVMe root on a clamped 64-entry queue pair |
| `serial=` | what the probe line prints; the model string comes from Identify Controller |

## What QEMU cannot tell us

Everything here is coded for and unexercised. It is listed so a hardware bring-up
knows where to look first, not as a claim that it works.

- **The MSI fallback.** QEMU always offers MSI-X, so the ladder never reaches
  it. (There is no INTx fallback at all; see decision 3.)
- **`CSTS.CFS`-driven reset.** Nothing in QEMU sets the controller fatal status.
- **Real 4Kn refusal on hardware that also offers a 512-byte format.** The gate
  proves the refusal, not the format selection a dual-format drive would need.
- **`CAP.MPSMIN > 0`** and **`CAP.DSTRD != 0`**: both are refused or handled by
  code no available controller reaches.
- **PCIe link errors and hot-removal.** There is no surprise-removal path.
- **A machine with no AHCI controller at all**, which is exactly the machine an
  NVMe driver is for. q35 always exposes the ICH9 AHCI controller, so the
  empty-controller branch (fixed on trunk as `619f10ae`) cannot be reached here.
- **Device speed.** `nvme-disk.img` sits in the host page cache exactly as
  `sata-disk.img` does. No throughput number from these gates is a statement
  about hardware.
