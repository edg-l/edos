# Attaching a USB mass-storage device hung the boot

**Status:** fixed. Written up from the note it replaced in `todo.txt`, which was
deleted when pending work moved into engram.

## Symptom

Booting with a USB disk attached (`scripts/edos-vm start --usb-disk`) stopped
between `xhci: initial enumeration complete` and the mailbox drain in the xHCI
event loop. Without the disk the same build booted normally.

## Root cause: two threads, each waiting for the other

`xhci_driver_main` called `crate::fs::api::register_partition` inline, before
falling into its event loop. Registration makes the FS kthread scan the new
device for a partition table, and `UsbBlockDevice::submit_read` posts those
reads to `USB_BLOCK_MAILBOX` — which only the xHCI thread drains. So the FS
kthread waited for a read that could only be answered by a thread that had not
yet reached its loop, and the xHCI thread waited for the registration it was
still inside.

Measured with `log!` on both sides: the wake was delivered at t=1.687 to a
thread whose first loop iteration was at t=21.71 — that is, only after the FS
kthread's *second* 10 s timeout expired.

## Fix

Registration is posted to a `usb-register` kthread, so `xhci_driver_main` falls
straight into its loop and can answer the reads the scan issues.

The FS side is no longer unbounded either: `send_and_wait` uses
`Response::wait_timeout(10s)` and fails the read with `BlockError::Timeout`
rather than waiting forever.

## Verification

`scripts/edos-vm start --usb-disk`: `fs: registered partition: USB Storage 0` at
t=1.691, followed by `[Terminal] Spawned shell`.

## The lesson worth keeping

**A driver thread must not do work that depends on its own event loop.**
Anything a driver publishes to the rest of the kernel — a device registration, a
partition scan, a mount — can come back as a request only that driver can serve.
Publish it from a separate kthread, and the dependency stops being a cycle.
