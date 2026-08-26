# The device was still writing into the buffer (2026-08-26)

## Status

**Fixed** for the corruption. The recovery paths in both storage drivers
released a command's buffer while the controller could still complete into
it. NVMe: `kernel/src/drivers/nvme/{mod,admin}.rs`. AHCI: the same shape in
`kernel/src/drivers/ahci/port.rs`.

The second defect in
`2026-08-26-the-hostile-nvme-boot-is-two-bugs-and-neither-is-the-log.md`, the
parked machine with nothing runnable, is **not** this and is still open. That
writeup's shape A stands; its shape B and the `LinkedList::pop` #GP are this.

## The defect

`watchdog_sweep` failed every outstanding command and only then called
`reset_controller`, which is where `CC.EN` is cleared. Between the two the
controller is still enabled and still owns every command it was given.

Failing a command runs `retire_op`, which returns its DMA memory, drops the
`Arc<NvmeOp>` and completes the caller's handle. What that releases is
usually **not** a `dma()` page. `NvmeNamespace::build_transfer` tries the
caller's own pages first and only bounces when `build_prp` cannot describe
them, so the ordinary read and write put the device straight into a
page-cache frame or a kernel-heap `Vec`. Release one early and the caller
wakes, returns, and frees it; the allocator hands the same bytes to somebody
else; and the controller finishes the command it was given, into them.

The kernel heap's free lists thread their link through the first eight bytes
of each free block, so a 512-byte sector landing on one leaves a garbage
pointer there. That is the

```
GPF Error code: 0x0   instruction_pointer: 0xffffffff800c99a7
<buddy_system_allocator::linked_list::LinkedList>::pop
```

recorded in the earlier writeup: error code 0 in ring 0 at a memory access is
a non-canonical address.

## Why three refutations missed it

The previous investigation refuted "the device DMAs into a page the watchdog
already freed" by quarantining the freed pages and measuring no improvement.
That experiment protected the **bounce buffer and the PRP list page** -- the
memory this driver owns. On the path that actually runs there is no bounce
buffer at all: the device is writing into the caller's memory, which the
quarantine never touched. The hypothesis was right and the instrument was
aimed one layer too low.

`heap-poison` could not see it either, and could not have: it detects a block
freed twice, and this is a block written after being freed, by a writer that
is not a CPU.

## The evidence

`retire_op` now takes a [`Retire`] saying whether the device completed the
command or the driver is abandoning it, and counts an abandonment made while
`CSTS.RDY` is still set. Against the old ordering, one hostile boot:

| | |
|---|---|
| controller resets in 25 s | 558 |
| resets that released a live command's buffer | **557** |

It is not a rare race. It is every reset.

With the fail-all moved inside `reset_controller`, after the disable and
before the queue rebuild, the same boot reads **1849 resets and 0**.

## The fix

`reset_controller` takes the caller's fail-all pass as a parameter rather
than expecting the caller to have run it first, because the order **is** the
correctness argument and a doc comment asking for it had already failed to
hold. The sequence is: clear `CC.EN`, wait for `CSTS.RDY` to drop (NVMe 2.0
3.5.1, which is what makes the controller stop touching host memory), fail
everything outstanding, rebuild the queues, re-enable. A disable that times
out does not run `fail_all` at all -- a controller that would not stop is
exactly the one whose buffers must not be released.

AHCI had the same shape: `fail_all_ncq_slots` then `restart_port`. Fixed the
same way, with `restart_port` taking the pass and running it once `stop_port`
has cleared `PxCMD.ST` and seen `PxCMD.CR` clear (AHCI 1.3.1 10.1.2). This is
also what Linux does in both drivers: `nvme_dev_disable` precedes
`nvme_cancel_request`, and `ahci_stop_engine` precedes libata's error
handling.

## Gates

- `/proc/nvme_stats` gained `abandoned_while_live`, `/proc/ahci_stats` gained
  `failed_while_running`. Both must stay zero. Each driver also says so once
  in the log, which is what a hostile boot leaves behind after the ring has
  evicted everything else.
- `make nvme-check`'s watchdog case asserts the NVMe line never appears.
- AHCI has no gate: its restart path only runs on TFES or an NCQ timeout, and
  nothing in the tree provokes either. The counter is the record.

## The lesson

**A command's DMA does not end when its handle leaves `Pending`.** The buffer
lifetime contract at the top of `drivers/block_io.rs` says this already, and
names cancellation as the case where the two come apart. Watchdog recovery is
the same case and was not on the list: the driver stops waiting for a command
the device has not finished. Anything that ends a command without a
completion has to stop the device first, and the two steps belong in one
function so that the order cannot be got wrong by a caller.

The generalisation is worth keeping: **when a hypothesis about hardware
writing into freed memory is refuted by protecting the memory, check that the
protected memory is the memory the hardware was given.** Here the common path
allocated nothing, so the entire experiment ran on the rare one.
