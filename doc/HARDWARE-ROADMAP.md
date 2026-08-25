# Hardware roadmap

What EDOS needs to be usable on a machine you own, in priority order, with what
each gap actually costs. QEMU is the development environment; **real hardware is
the target**, and the README already ships a hybrid ISO for a USB stick.

That distinction decides things. Anything `virtio-` prefixed is a development
affordance and must not be counted as a capability: `virtio-gpu` and
`virtio-net` do not exist on the metal, and a design argued from their presence
is argued from the emulator. The same caution applies to measurements — the
`BASE_SLICE` derivation in `doc/SCHED-ROADMAP.md` is built on a ~1 us APIC arm
that costs that much *because a hypervisor traps it*, and on real hardware it
does not.

## Nobody has booted it on real hardware yet

Or if they have, it was never written down. **Everything below is inferred from
the driver list rather than observed**, so the first item is not a driver at all:

```bash
sudo dd if=edos-x86_64.iso of=/dev/sdX bs=4M status=progress conv=fsync
```

The ISO is hybrid and carries its own root as a Limine module served from RAM,
so it needs no disk to reach a desktop. What to record, in order:

- Does it reach the desktop at all?
- Does it see any storage? AHCI and NVMe now, so record which controller
  bound; root selection logs every partition it found before falling back to memfs.
- Does the NIC bind? `e1000e` only.
- Does USB input work? This is the one most likely to already be fine.
- Does HDA produce sound?
- What do `dmesg` and `/proc/cmdline` say about ACPI, and how many CPUs come up?

Serial is unavailable on most laptops, so plan to photograph the screen or write
`dmesg` to the live root before it forgets. **The findings rank everything
below; do not size the items from this file alone.**

## What is already right for a modern machine

Worth stating, because it is more than it sounds:

| Subsystem | State |
|---|---|
| Boot | UEFI via Limine, base revision 6 |
| Display | Limine hands over a GOP framebuffer — unaccelerated, but present |
| Input | xHCI + USB HID, which is what a machine with no PS/2 port needs |
| Audio | Intel HDA, still the current standard |
| Interrupts | APIC + MSI |
| SMP, ACPI | both present |

## 1. NVMe — landed for QEMU, unproven on hardware

`kernel/src/drivers/nvme/` is a working driver: PCIe discovery, BAR0 mapping,
controller reset and enable, admin queue, `Identify Controller`/`Identify
Namespace`, one I/O queue pair with MSI-X, PRP scatter-gather, reads, writes,
FUA, VWC-gated flush, MDTS splitting, a staleness watchdog, controller reset and
`CC.SHN` shutdown. Namespaces register into `block_io` at device id
`3000 + controller * 64 + (nsid - 1)` and appear as `/dev/nvme<c>n<n>`, so an
NVMe disk is a root and an `edos-install` target like any other. `/proc/nvme_stats`
carries 21 counters. A guest boots with root on an NVMe namespace and no SATA
disk attached.

**The staging note this section used to carry was not what got built.** It said
to poll the completion queue first and add MSI-X afterwards; the driver went
straight to MSI-X plus an IRQ-woken dispatcher kthread, because that is the shape
the AHCI NCQ path already had and copying it was cheaper than writing a polled
path to throw away. Polling survives in exactly one place, and for the opposite
reason: `admin_command_polled` polls its own completion, and the dispatcher is
forbidden to drain the admin queue, because a drain from a second thread eats the
completion the poller is waiting for. A queue pair per CPU was never started.

**What QEMU cannot tell us, and so is still unverified**: the MSI fallback (QEMU
always offers MSI-X; there is no INTx fallback, see `doc/nvme.md` decision 3),
`CSTS.CFS`-driven reset, a 4Kn namespace on
a controller that also offers a 512-byte format, `CAP.MPSMIN > 0`,
`CAP.DSTRD != 0`, PCIe link errors, and hot-removal. Nor can it reach the
no-AHCI-controller boot path (`619f10ae`), since q35 always exposes the ICH9
controller — which is exactly the machine this item exists for.

`make nvme-check` is the gate, and it is five boots: root on an NVMe namespace
with no SATA disk attached, SATA and NVMe coexisting with a device node each,
the `logical_block_size=4096` refusal, `edos-install --yes /dev/nvme0n1` onto a
blank image followed by a boot from it, and the watchdog under
`nvme_timeout_ms=0`, where every command is declared hung the moment it is
issued and the root filesystem still has to mount. `doc/nvme.md` documents the
register and queue model, the nine decisions, the `/proc/nvme_stats` fields, and
the QEMU knob that reaches each path.

## 2. A modern NIC — the gap that decides whether it networks

`e1000e` is Intel gigabit: still on desktops and servers, largely gone from
laptops. On a modern machine EDOS reaches a desktop with no network at all.

1. **Intel I225/I226** (2.5GbE, the `igc` family). Common on current desktop
   boards, and its descriptor rings are close enough to `e1000e` that the
   existing driver is a starting point rather than a blank page. Cheapest first
   win.
2. **Realtek RTL8125/RTL8168** (the `r8169` family). The most common consumer
   NIC by volume and the one most likely to be in a random laptop. A separate
   driver, but thoroughly documented by the Linux and BSD implementations.
3. **USB Ethernet** (ASIX AX88179, RTL8153). The sleeper: xHCI already works, so
   a dongle or phone tethering gives networking on *any* machine whatever is
   soldered to the board. Probably the best ratio of coverage to effort here.

**WiFi is out of scope.** An `iwlwifi`-class driver plus firmware loading plus
an 802.11 stack plus a supplicant is a multi-month subsystem, and option 3
sidesteps it for anyone with a dongle.

## 3. Graphics, and why the answer changed

On the metal the display is an unaccelerated GOP framebuffer. Nothing in the
tree needs more today — `edos_render` draws with `tiny-skia` on the CPU and
`doc/design/wm-damage.md` calls the ~100 MB/s of a large window drag the genuine
floor — so this stays low until something names a consumer.

When one appears, the ordering is:

- **Write a GL 1.x subset.** SerenityOS is the precedent: they wrote `LibGL`
  over a software rasteriser without porting anything. Fixed-function needs *no
  shader compiler*, so a few thousand lines draws something. The payoff is
  "EDOS can draw 3D", not "EDOS can run other people's 3D software".
- **Port Mesa `softpipe`.** What Haiku and ReactOS do. A real, current GL that
  real software targets, inheriting the shader compiler, the vectorisation and
  the image sampling that are the expensive parts of writing one. `softpipe`,
  not `llvmpipe` — the latter drags in LLVM. Lands behind libc stage 3, which
  needs dynamic linking.
- **Not virgl.** It was once argued here as right "for a VM-only OS". EDOS is
  not one. virgl buys nothing on hardware and is a development convenience.

Vulkan sits below both: unlike GL it has no shader-free subset, so every
implementation needs SPIR-V ingestion and a JIT before the first triangle. Rust
crates take a real bite out of that (`rspirv` parses, `naga` gives a validated
IR with structured control flow recovered, `cranelift` is a JIT backend, and
SPIR-V is already SSA) — but they do not touch the rasteriser, the image
sampling or the lane-parallel execution that is most of what a software Vulkan
*is*. Worth noting for whoever picks it up: a shader JIT would be the first JIT
in EDOS, and the first thing to map a page executable at runtime.

## Not planned, and why

- **Suspend and resume.** Wants an ACPI sleep path plus every driver growing
  save/restore. Large, and no user of a hobby OS is closing the lid on it yet.
- **Power management** beyond what firmware does. Same reasoning.
- **A real GPU driver** (i915, amdgpu). Each is larger than this entire kernel.
