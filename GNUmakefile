# Nuke built-in rules and variables.
MAKEFLAGS += -rR
.SUFFIXES:

# Comma helper for use inside $(call ...) arguments where literal commas would
# be misinterpreted as argument separators.
comma := ,

# Convenience macro to reliably declare user overridable variables.
override USER_VARIABLE = $(if $(filter $(origin $(1)),default undefined),$(eval override $(1) := $(2)))

# Target architecture to build for. Default to x86_64.
$(call USER_VARIABLE,KARCH,x86_64)

# Guest memory size (used by both -m and memory-backend-memfd for blob=on)
$(call USER_VARIABLE,QEMU_MEM,2G)

# Default user QEMU flags. These are appended to the QEMU command calls.
$(call USER_VARIABLE,QEMUFLAGS,-m $(QEMU_MEM))

# SATA backend AIO mode. `io_uring` is the default; `threads` is used by the
# run-aio-threads target to isolate whether AHCI NCQ stalls trace to the host
# io_uring path.
$(call USER_VARIABLE,QEMU_AIO,io_uring)

override IMAGE_NAME := edos-$(KARCH)

.PHONY: all
all: $(IMAGE_NAME).iso

.PHONY: all-hdd
all-hdd: $(IMAGE_NAME).hdd

.PHONY: run
run: run-$(KARCH)

.PHONY: run-hdd
run-hdd: run-hdd-$(KARCH)

# Display device configurations
# blob=on enables zero-copy display (requires host CONFIG_UDMABUF=y + memfd backend)
DISPLAY_VGA := -device VGA,vgamem_mb=32
# usb-mouse on purpose, even though usb-tablet feels better here.
#
# A physical mouse is a relative device; usb-tablet only exists in a VM. Running
# the relative path by default is what keeps it honest, because its problems are
# invisible on an absolute one: an interrupt endpoint that misses a service
# interval, a report buffer that is not re-armed in time, a driver that assumes
# it will be told where the pointer is rather than how far it moved.
#
# It is also the slower path here, and that is not the guest's doing. QEMU has to
# walk the guest's pointer to wherever the host's already is, so one drag that
# costs 21 reports on a tablet costs 325 on a mouse, through an endpoint whose
# descriptor asks for 8 ms against the tablet's 1 ms. An interrupt endpoint
# carries one packet per interval, so that is a ceiling of 125 a second and about
# 2.6 s of backlog for a drag. A real mouse never builds that queue: it produces
# reports at the rate its own bInterval is chosen for.
#
# `scripts/edos-vm` defaults to --pointer tablet, which is the right choice when
# the point is to drive the guest rather than to test its input path.
USB_INPUT := -device qemu-xhci -device usb-kbd -device usb-mouse

DISPLAY_VIRTIO := -vga none -device virtio-vga,xres=1920,yres=1080,blob=on -display sdl
DISPLAY_VIRTIO_GTK := -vga none -device virtio-vga,xres=1920,yres=1080,blob=on -display gtk,zoom-to-fit=off

# What a run with nobody watching asks for. `blob=on` wants /dev/udmabuf and
# `-display sdl` wants a session; neither exists on a machine with no desktop,
# and QEMU refuses to start rather than doing without them. The guest sees the
# same virtio-gpu either way, since blob is a host-side zero copy.
DISPLAY_HEADLESS := -vga none -device virtio-vga,xres=1920,yres=1080 -display none

# Host audio backend for the emulated HDA device. `pipewire` needs a session
# bus, which a bare SSH login does not have; `AUDIODEV=none` runs the same
# device against a null backend.
AUDIODEV ?= pipewire

# The ISO a run boots. The default one's `root=` names the NVMe partition, so
# an ordinary `make run` and every gate below root on `/dev/nvme0n1` with the
# SATA disk attached beside it. `edos-sata.iso` is the same system with `root=`
# naming the SATA partition, for the cases that have to assert the other way
# round. Override per target rather than globally.
BOOT_ISO ?= $(IMAGE_NAME).iso

# QEMU runner function
# $(1) = boot media type (iso/hdd)
# $(2) = smp cores
# $(3) = extra flags
# $(4) = display device flags (defaults to DISPLAY_VIRTIO)
define run_qemu_uefi
	qemu-system-$(KARCH) \
		-M q35 \
		-cpu qemu64,+sse4.1,+sse4.2,+x2apic,+fsgsbase,+invtsc \
		-object memory-backend-memfd,id=mem1,size=$(QEMU_MEM) \
		-machine memory-backend=mem1 \
		-drive if=pflash,unit=0,format=raw,file=ovmf/ovmf-code-$(KARCH).fd,readonly=on \
		-drive if=pflash,unit=1,format=raw,file=ovmf/ovmf-vars-$(KARCH).fd \
		$(if $(filter iso,$(1)),-cdrom $(BOOT_ISO),-hda $(IMAGE_NAME).hdd) \
		-device isa-debug-exit,iobase=0xf4,iosize=0x04 \
		-chardev stdio,id=ser0,signal=off,logfile=run_log.txt \
		-serial chardev:ser0 \
		-no-reboot -d cpu_reset -D /tmp/qemu_reset.log \
		-drive id=sata0,if=none,format=qcow2,file=sata-disk.img,aio=$(QEMU_AIO),discard=unmap \
		-device ide-hd,drive=sata0,bus=ide.1 \
		-drive id=nvme0,if=none,format=raw,file=nvme-disk.img \
		-device nvme,drive=nvme0,serial=EDOSNVME0 \
		$(if $(4),$(4),$(DISPLAY_VIRTIO)) \
		$(USB_INPUT) \
		-netdev user,id=net0 -device e1000e,netdev=net0 \
		-audiodev $(AUDIODEV),id=snd0 \
		-device intel-hda -device hda-output,audiodev=snd0 \
		-smp $(2) \
		$(3) \
		$(QEMUFLAGS)
endef

.PHONY: run-x86_64
run-x86_64: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img nvme-disk.img
	$(call run_qemu_uefi,iso,4,-accel kvm -s -no-reboot -no-shutdown)

# Like `run` but swaps the SATA backend's aio=io_uring for aio=threads.
# Used to triage whether the AHCI NCQ timeout (8 MiB + fsync inflighttest
# after ~503 writes) is io_uring-specific on the host. If the stall
# disappears here, blame io_uring; otherwise the bug is deeper.
.PHONY: run-aio-threads
run-aio-threads: QEMU_AIO := threads
run-aio-threads: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img nvme-disk.img
	$(call run_qemu_uefi,iso,4,-accel kvm -s -no-reboot -no-shutdown)

.PHONY: run-vga
run-vga: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img nvme-disk.img
	$(call run_qemu_uefi,iso,4,-accel kvm,$(DISPLAY_VGA))

.PHONY: run-gtk
run-gtk: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img nvme-disk.img
	$(call run_qemu_uefi,iso,4,-accel kvm,$(DISPLAY_VIRTIO_GTK))

.PHONY: run-single
run-single: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img nvme-disk.img
	$(call run_qemu_uefi,iso,1,-accel kvm)

.PHONY: run-big
run-big: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img nvme-disk.img
	$(call run_qemu_uefi,iso,16,)

.PHONY: run-gdb
run-gdb: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img nvme-disk.img
	$(call run_qemu_uefi,iso,1,-no-shutdown -accel tcg -s -S)

.PHONY: run-gdb-4
run-gdb-4: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img nvme-disk.img
	$(call run_qemu_uefi,iso,4,-no-shutdown -accel tcg -s -S)

.PHONY: run-gdb-kvm
run-gdb-kvm: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img nvme-disk.img
	$(call run_qemu_uefi,iso,4,-no-shutdown -accel kvm -s -S)

# Boot with no local display: VNC for a human, QMP for scripts and agents.
# See scripts/edos-vm for screenshot, keyboard and pointer control.
.PHONY: run-headless
run-headless: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img nvme-disk.img
	scripts/edos-vm start

.PHONY: run-capture
run-capture: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img nvme-disk.img
	$(call run_qemu_uefi,iso,4,-accel kvm -object filter-dump$(comma)id=dump0$(comma)netdev=net0$(comma)file=/tmp/edos.pcap)

.PHONY: run-debug-fault
run-debug-fault: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img nvme-disk.img usb-test.img
	$(call run_qemu_uefi,iso,4,-no-shutdown -accel tcg -s -d int -D /tmp/qemu_fault.log -drive id=usbdisk0$(comma)if=none$(comma)format=raw$(comma)file=usb-test.img -device usb-storage$(comma)drive=usbdisk0)

.PHONY: run-storage
run-storage: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img nvme-disk.img usb-test.img
	$(call run_qemu_uefi,iso,4,-accel kvm -drive id=usbdisk0$(comma)if=none$(comma)format=raw$(comma)file=usb-test.img -device usb-storage$(comma)drive=usbdisk0)


usb-test.img:
	qemu-img create -f raw usb-test.img 16M

# Attaches an NVMe controller and namespace alongside the usual SATA disk, so
# a boot here proves the driver against a real QEMU model without disturbing
# root selection: this is not the NVMe-root boot, just NVMe-present.
.PHONY: run-nvme
run-nvme: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img nvme-disk.img
	$(call run_qemu_uefi,iso,4,-accel kvm)

# `make run` with the SATA disk as root instead. Both disks are attached either
# way; the only difference is which partition GUID the ISO's `root=` names.
.PHONY: run-sata
run-sata: BOOT_ISO := edos-sata.iso
run-sata: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd edos-sata.iso sata-disk.img nvme-disk.img
	$(call run_qemu_uefi,iso,4,-accel kvm)

# A scratch EFS whose journal ring is deliberately tiny, so a metadata workload
# wraps it in seconds rather than in the hours the default 16 MiB would take.
# Wrapping is the precondition for testing replay's wrapped-region handling,
# which no ordinary boot ever reaches: a normal boot uses about 50 ring blocks.
# Its partition GUID differs from $(PARTITION_UUID) on purpose: root selection
# matches the cmdline GUID across every enumerated partition, so a copy of the
# root's GUID on a second attached disk would make which disk boots a race.
JOURNAL_TEST_UUID := 5a5a5a5a-0000-4000-8000-00000000ed05
journal-test.img: tools/efs-mkfs/src/*.rs libs/efs-common/src/*.rs
	rm -f journal-test.img
	qemu-img create -f raw journal-test.img 64M >/dev/null
	sgdisk journal-test.img -n 1:2048 -t 1:0700 -c 1:"JTEST" --partition-guid=1:$(JOURNAL_TEST_UUID)
	cargo build --release --manifest-path tools/efs-mkfs/Cargo.toml
	tools/efs-mkfs/target/release/efs-mkfs --partition-offset 1048576 --journal-size-mib 1 \
		--label JTEST journal-test.img

# Boots with `/dev/journal-ctl` available and the small-ring scratch disk
# attached as a second SATA drive. See doc/journal-recovery-test.md for the
# procedure; the disk is not the root, so cutting power on it is safe.
.PHONY: run-recovery
run-recovery: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd sata-disk.img nvme-disk.img journal-test.img
	$(MAKE) $(IMAGE_NAME).iso CARGO_FLAGS="--features fault-inject"
	scripts/edos-vm start --extra-disk journal-test.img

# Both checks below cut power on the scratch disk mid-write and leave their
# files behind, so each one starts from a freshly formatted image rather than
# from whatever the last run left. Without this `recovery-check` passes exactly
# once per image and then reports its own setup as failed forever after: its
# files already exist, so `touch` only restamps them, no metadata transaction
# is left uncheckpointed, and the precondition it asserts cannot hold. Making
# the image an ordinary prerequisite is what hid that -- it is only rebuilt
# when efs-mkfs changes, which is never during a run of these.
.PHONY: fresh-journal-test-img
fresh-journal-test-img:
	rm -f journal-test.img
	$(MAKE) journal-test.img

# Unattended version of the doc/journal-recovery-test.md procedure: pause
# checkpointing, fsync a workload, cut power, remount, and fail if replay
# did not bring it back. Needs the fault-inject build for /dev/journal-ctl.
.PHONY: recovery-check
recovery-check: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd sata-disk.img nvme-disk.img fresh-journal-test-img
	$(MAKE) $(IMAGE_NAME).iso CARGO_FLAGS="--features fault-inject"
	scripts/recovery-check

# Drive the host's own OpenSSH client against the guest's sshd: authentication,
# a refused password, exit status, ~10 MB each way compared byte for byte, and
# concurrent sessions. It does not cover the shut-window flow-control case; see
# doc/sshd.md for why, and for what covering it would take.
.PHONY: ssh-check
ssh-check: $(IMAGE_NAME).iso sata-disk.img nvme-disk.img
	scripts/ssh-check

# Hold unlinked-but-open files, cut power, and fail if the remount does not
# finish the deletions the crash interrupted. The orphan chain's regression;
# see doc/efs.md section 14.
.PHONY: orphan-check
orphan-check: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd sata-disk.img nvme-disk.img fresh-journal-test-img efs-fsck
	$(MAKE) $(IMAGE_NAME).iso
	scripts/orphan-check

.PHONY: run-trace
run-trace: programs limine/limine ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd sata-disk.img nvme-disk.img
	$(MAKE) -C kernel CARGO_FLAGS="--features trace"
	$(MAKE) $(IMAGE_NAME).iso
	$(call run_qemu_uefi,iso,4,-accel kvm -m 2G)

# The suite reports through isa-debug-exit, which the host sees as
# `(code << 1) | 1`: 1 for a passing run, 3 for a failing one, and anything
# else for a guest that died before reporting. Translate that to a shell
# exit status, or every run looks like a failed one.
#
# A passing run is not 1 alone: qemu's own startup failures also exit 1, so a
# refused `sata-disk.img` write lock (another guest already has it) reads as a
# green suite that never ran. The verdict has to come from the serial log, and
# the log is truncated first so a previous run's cannot stand in for it.
sched_test_status = ; rc=$$?; \
	if [ $$rc -eq 1 ] && grep -aq 'TESTS PASSED' run_log.txt; then exit 0; \
	elif [ $$rc -eq 3 ]; then echo "sched-test: suite reported failures"; exit 1; \
	elif [ $$rc -eq 1 ]; then \
		echo "sched-test: qemu exited before the suite reported a verdict"; exit 1; \
	else echo "sched-test: qemu exited $$rc without a verdict"; exit 1; fi

.PHONY: test
test: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd sata-disk.img nvme-disk.img
	$(MAKE) $(IMAGE_NAME).iso CARGO_FLAGS="--features sched-test"
	rm -f run_log.txt
	$(call run_qemu_uefi,iso,4,-accel kvm,$(DISPLAY_HEADLESS)) $(sched_test_status)

.PHONY: test-single
test-single: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd sata-disk.img nvme-disk.img
	$(MAKE) $(IMAGE_NAME).iso CARGO_FLAGS="--features sched-test"
	rm -f run_log.txt
	$(call run_qemu_uefi,iso,1,-accel kvm,$(DISPLAY_HEADLESS)) $(sched_test_status)

# The same suite as `test`, against a null audio backend. `test` binds the
# host's PipeWire session, which a bare SSH login does not have, so this is the
# form that runs from a terminal with no desktop behind it.
.PHONY: test-headless
test-headless:
	$(MAKE) test AUDIODEV=none

# The guest's own regression suites, in one boot. `programs/` carries
# twenty-one test programs and only three were reached by any gate, so
# `iotest`'s 23 syscall cases and `socktest`'s 16 ran whenever somebody
# remembered them and never otherwise. Each suite is judged by its exit code.
#
# Every gate that drives a real guest boots `sata-disk.img` and runs the
# userspace inside it, not the one in `filesystem/`. `make all` does not
# rebuild that image, so without the prerequisite a userspace fix is invisible
# to the gate and the run silently judges whenever the image was last made.
.PHONY: guest-check
guest-check: $(IMAGE_NAME).iso sata-disk.img nvme-disk.img
	scripts/guest-check

# The sampling profiler, end to end: the guest samples itself, the host
# resolves the addresses, and the workload's known hot function has to come out
# on top. Every way this can break -- a timer that reaches one CPU, a frame
# walk that stops at depth one, a load base off by a page -- still produces
# something shaped like a profile, so nothing but the symbol names catches it.
.PHONY: profile-check
profile-check: $(IMAGE_NAME).iso sata-disk.img nvme-disk.img
	scripts/profile-check

# Storage regressions, both halves. `fs-regression` reboots between writing and
# verifying, so it catches data that never reached the disk; `fsbench-run`
# verifies every pattern it writes and reports throughput. Both drive a real
# guest through scripts/edos-vm and need the ISO already built.
.PHONY: storage-check
storage-check: $(IMAGE_NAME).iso sata-disk.img nvme-disk.img
	scripts/fs-regression
	scripts/fs-regression --fat32
	scripts/fsbench-run


# The NVMe driver's own gate: an NVMe-root boot, a SATA+NVMe coexistence boot,
# the 4Kn refusal path, `edos-install` onto a blank NVMe image followed by a
# boot from it, and a boot whose watchdog resets the controller under a live
# root. Five boots, so budget about twelve minutes.
# `sata-disk.img` is a prerequisite even though only two of the five cases
# attach it: those two root on it, so they run whatever userspace it holds.
.PHONY: nvme-check
nvme-check: $(IMAGE_NAME).iso edos-nvme.iso edos-nvme-hostile.iso edos-sata.iso nvme-disk.img sata-disk.img fresh-nvme-blank
	scripts/nvme-check


.PHONY: run-emu
run-emu: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img nvme-disk.img
	$(call run_qemu_uefi,iso,4,-accel tcg)

.PHONY: run-hdd-x86_64
run-hdd-x86_64: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).hdd sata-disk.img nvme-disk.img
	$(call run_qemu_uefi,hdd,4,-accel kvm)

gdb:
	rust-gdb -x gdbinit

.PHONY: run-bios
run-bios: $(IMAGE_NAME).iso
	qemu-system-$(KARCH) \
		-M q35 \
		-cdrom $(IMAGE_NAME).iso \
		-boot d \
		$(QEMUFLAGS)

.PHONY: run-hdd-bios
run-hdd-bios: $(IMAGE_NAME).hdd
	qemu-system-$(KARCH) \
		-M q35 \
		-hda $(IMAGE_NAME).hdd \
		$(QEMUFLAGS)

override OVMF_URL := https://github.com/osdev0/edk2-ovmf-nightly/releases/latest/download/edk2-ovmf.tar.xz

# Upstream ships every architecture in one tarball; extract just this one.
ovmf/edk2-ovmf.tar.xz:
	mkdir -p ovmf
	curl -Lo $@ $(OVMF_URL)

ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd: ovmf/edk2-ovmf.tar.xz
	tar -xJf $< -C ovmf --strip-components=1 \
		edk2-ovmf/ovmf-code-$(KARCH).fd edk2-ovmf/ovmf-vars-$(KARCH).fd
	touch ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd

# Limine 12 ships its binary distribution as a release tarball; the
# `vN.x-binary` git branch this used to clone stops at v11. The tarball carries
# the same files that branch did, plus the Makefile that builds the host tool.
LIMINE_VERSION := 12.5.2
LIMINE_URL := https://github.com/limine-bootloader/limine/releases/download/v$(LIMINE_VERSION)/limine-binary.tar.xz

limine/limine:
	rm -rf limine limine-binary limine-binary.tar.xz
	curl -sSfL -o limine-binary.tar.xz $(LIMINE_URL)
	tar -xJf limine-binary.tar.xz
	rm -f limine-binary.tar.xz
	mv limine-binary limine
	$(MAKE) -C limine

.PHONY: kernel
kernel: programs
	$(MAKE) -C kernel CARGO_FLAGS="$(CARGO_FLAGS)"

.PHONY: check
check: programs
	$(MAKE) -C kernel check

# Userspace unit tests that need no guest: URL resolution, the CSS cascade, the
# SSH wire format, `grab`'s merge. Seconds, not a boot; see the script for why a
# plain `cargo test` cannot run them.
.PHONY: host-tests
host-tests:
	scripts/host-tests

# What the kernel would call dead if the `dead_code` allows were gone. Takes
# every allow away, builds each feature set, prints the warnings and restores
# the tree. Judgement, not a gate: run it before a release.
.PHONY: dead-code
dead-code:
	scripts/dead-code-sweep

# $(1) = output ISO, $(2) = staging directory, $(3) = partition GUID the
# cmdline's root= names. Only the GUID differs between the two ISOs this tree
# builds, and substituting it here keeps limine.conf a single tracked file
# rather than one per root candidate.
define build_iso
	rm -rf $(2)
	mkdir -p $(2)/boot
	objcopy --strip-debug kernel/kernel $(2)/boot/kernel
	cp -v live-root.img $(2)/boot/
	mkdir -p $(2)/boot/limine
	sed -e 's/$(PARTITION_UUID)/$(3)/' $(if $(4),-e '/root=UUID=$(3)/s|$$| $(4)|',) limine.conf > $(2)/boot/limine/limine.conf
	mkdir -p $(2)/EFI/BOOT
	cp -v limine/limine-bios.sys limine/limine-bios-cd.bin limine/limine-uefi-cd.bin $(2)/boot/limine/
	cp -v limine/BOOTX64.EFI $(2)/EFI/BOOT/
	cp -v limine/BOOTIA32.EFI $(2)/EFI/BOOT/
	xorriso -as mkisofs -b boot/limine/limine-bios-cd.bin \
		-no-emul-boot -boot-load-size 4 -boot-info-table \
		--efi-boot boot/limine/limine-uefi-cd.bin \
		-efi-boot-part --efi-boot-image --protective-msdos-label \
		$(2) -o $(1)
	./limine/limine bios-install $(1)
	rm -rf $(2)
endef

# Roots on NVMe. Both disks are attached by every run target, so this decides
# which one wins root selection, and NVMe is the default because it is the
# faster of the two here and the less proven of the two drivers -- every gate
# that boots is a gate exercising it.
$(IMAGE_NAME).iso: limine/limine kernel live-root.img
	$(call build_iso,$(IMAGE_NAME).iso,iso_root,$(NVME_UUID))

# The same system with `root=` naming the SATA partition. `nvme-check` boots it
# for the three cases whose whole point is that the NVMe disk does *not* become
# root: coexistence, the 4Kn refusal, and installing onto a blank NVMe image.
edos-sata.iso: limine/limine kernel live-root.img
	$(call build_iso,edos-sata.iso,iso_root_sata,$(PARTITION_UUID))

# The same system with root= naming the NVMe disk's partition GUID, so an
# NVMe-only machine mounts nvme-disk.img instead of falling back to memfs.
# `scripts/nvme-check` boots this one; nothing else does. It also carries
# `nvme_probe_read`, whose PRP gate the check asserts on: the probe is the
# only thing that reports whether the buffer it read through was physically
# discontiguous, and a PRP list over a contiguous run gates nothing.
edos-nvme.iso: limine/limine kernel live-root.img
	$(call build_iso,edos-nvme.iso,iso_root_nvme,$(NVME_UUID),nvme_probe_read)

# The NVMe root again, with `nvme_timeout_ms=0`: the watchdog declares every
# command hung the moment it is issued and resets the controller under a live
# root. `nvme-check`'s watchdog case boots this one; the timeout is baked into
# the cmdline because that is the only place the kernel reads it.
#
# What this setting can and cannot show is worth stating, because the plan that
# introduced it asked for more than it can give. The sweep interval is
# `min(1 s, max(timeout, 1 ms))`, so a zero timeout sweeps every millisecond
# and kills whatever it finds, and a reset takes a couple of milliseconds: the
# device is therefore resetting most of the time and no amount of correctness
# makes the desktop responsive. Every whole millisecond above zero is the other
# extreme -- commands complete in about a hundred microseconds under KVM, so
# nothing is ever declared hung and the watchdog never fires at all. Zero is
# the only setting that exercises the path, and the gate asserts what it can
# prove: the reset runs, and no I/O is reported failed.
edos-nvme-hostile.iso: limine/limine kernel live-root.img
	$(call build_iso,edos-nvme-hostile.iso,iso_root_nvme_hostile,$(NVME_UUID),nvme_timeout_ms=0)

$(IMAGE_NAME).hdd: limine/limine kernel
	rm -f $(IMAGE_NAME).hdd
	dd if=/dev/zero bs=1M count=0 seek=128 of=$(IMAGE_NAME).hdd
	sgdisk $(IMAGE_NAME).hdd -n 1:2048 -t 1:ef00
	./limine/limine bios-install $(IMAGE_NAME).hdd
	mformat -F -i $(IMAGE_NAME).hdd@@1M
	mmd -i $(IMAGE_NAME).hdd@@1M ::/EFI ::/EFI/BOOT ::/boot ::/boot/limine
	mcopy -i $(IMAGE_NAME).hdd@@1M kernel/kernel ::/boot
	mcopy -i $(IMAGE_NAME).hdd@@1M limine.conf ::/boot/limine
	mcopy -i $(IMAGE_NAME).hdd@@1M limine/limine-bios.sys ::/boot/limine
	mcopy -i $(IMAGE_NAME).hdd@@1M limine/BOOTX64.EFI ::/EFI/BOOT
	mcopy -i $(IMAGE_NAME).hdd@@1M limine/BOOTIA32.EFI ::/EFI/BOOT

.PHONY: clean
clean:
	$(MAKE) -C kernel clean
	$(MAKE) -C programs clean
	rm -rf iso_root $(IMAGE_NAME).iso $(IMAGE_NAME).hdd live-root.img

.PHONY: distclean
distclean: clean clean-sata clean-nvme
	$(MAKE) -C kernel distclean
	rm -rf limine ovmf


.PHONY: fmt
fmt:
	$(MAKE) -C kernel fmt
	cargo +edos fmt --manifest-path programs/Cargo.toml --all
	@for m in tools/efs-mkfs tools/efs-fsck libs/efs-common libs/intrusive_list \
	          libs/window-abi libs/edos-trace-abi; do \
		[ -f $$m/Cargo.toml ] && cargo fmt --manifest-path $$m/Cargo.toml; \
	done

# The gate form of `fmt`: reports rather than rewrites, so CI fails on
# unformatted code instead of silently fixing it in a checkout nobody keeps.
.PHONY: fmt-check
fmt-check:
	cargo fmt --manifest-path kernel/Cargo.toml -- --check
	cargo +edos fmt --manifest-path programs/Cargo.toml --all -- --check
	@for m in tools/efs-mkfs tools/efs-fsck libs/efs-common libs/intrusive_list \
	          libs/window-abi libs/edos-trace-abi; do \
		[ -f $$m/Cargo.toml ] && cargo fmt --manifest-path $$m/Cargo.toml -- --check; \
	done

# Clippy over the kernel's every feature combination and the whole userspace
# workspace, warnings denied. `clippy::too_many_arguments` is allowed globally
# (kernel/src/main.rs, programs/.cargo/config.toml); everything else is meant
# to stay at zero.
.PHONY: clippy
clippy: programs
	$(MAKE) -C kernel clippy
	# Run from inside programs/: cargo discovers .cargo/config.toml relative to
	# the working directory, not to --manifest-path, so the workspace-wide
	# `too_many_arguments` allow (and the default target) are only picked up here.
	cd programs && cargo +edos clippy --all-targets -- -D warnings

.PHONY: programs
programs:
	$(MAKE) -C programs build

DISK_UUID := 12345678-1234-5678-9abc-123456789abc
PARTITION_UUID := 87654321-4321-8765-cba9-987654321fed
FILESYSTEM_SERIAL := 305419896

# What the disk images are built from. A `$(shell find filesystem ...)` cannot
# serve here: make expands it while reading this file, before `programs` has
# run, so a binary added by this same invocation is missing from the list and
# the images silently ship without it.
#
# The manifest is regenerated after `programs` instead, and rewritten only when
# the tree really changed. Its recipe therefore runs on every invocation while
# its timestamp moves only on a real change, which is what lets the images stay
# up to date without rebuilding the persistent sata-disk.img every time.
#
# `filesystem/boot` is excluded, and that exclusion is what makes the guard
# work at all. The live-root recipe writes the stripped kernel to
# filesystem/boot/kernel on every build -- `kernel` is phony, so it always
# runs -- and the manifest records mtimes, so including it changed the manifest
# every single time and rebuilt the 5 GB sata-disk.img on every kernel edit,
# `make test` included. Nothing boots from the disk's copy of /boot: the run
# targets boot the ISO, which carries its own. An installed system built by
# `edos-install` gets its boot files from the ISO too, so the only cost of the
# exclusion is that a stale kernel can sit in sata-disk.img's /boot, where
# nothing reads it.
define update-manifest
find filesystem -type f ! -name '*.rlib' ! -name '*.a' ! -name '.manifest*' \
	! -path 'filesystem/boot/*' \
	-printf '%T@ %s %p\n' | sort > filesystem/.manifest.new; \
cmp -s filesystem/.manifest.new filesystem/.manifest \
	|| mv -f filesystem/.manifest.new filesystem/.manifest; \
rm -f filesystem/.manifest.new
endef

filesystem/.manifest: filesystem programs
	@$(update-manifest)

# The live root carried inside the ISO as a Limine module: a complete GPT disk
# image, so the kernel discovers it exactly like a real disk. Sized from the
# populated tree rather than hard-coded, because it is resident in RAM for the
# whole boot.
# `kernel` is order-only so cargo always gets a chance to run, while the real
# prerequisite is the binary it produces. Depending on the phony target
# directly rebuilt this image -- objcopy, a du of the whole tree and an
# efs-mkfs -- on every invocation, including ones where cargo had nothing to do.
live-root.img: kernel/kernel limine/limine filesystem/.manifest tools/efs-mkfs/src/*.rs libs/efs-common/src/*.rs | kernel
	mkdir -p filesystem/boot
	# Stripped: this copy is only ever loaded, never symbolized. Debug info
	# stays in kernel/kernel, which is what addr2line reads. 40 MB -> 2.5 MB,
	# which is most of the ISO and most of an install's write volume.
	objcopy --strip-debug kernel/kernel filesystem/boot/kernel
	cp limine/BOOTX64.EFI filesystem/boot/BOOTX64.EFI
	@set -e; \
	used=$$(du -sb filesystem | cut -f1); \
	size=$$(( used * 14 / 10 )); \
	min=$$(( 64 * 1024 * 1024 )); \
	if [ $$size -lt $$min ]; then size=$$min; fi; \
	size=$$(( (size + 1048575) / 1048576 * 1048576 )); \
	echo "live-root.img: filesystem/ is $$(( used / 1048576 )) MiB, image $$(( size / 1048576 )) MiB"; \
	rm -f live-root.img; \
	qemu-img create -f raw live-root.img $$size >/dev/null
	sgdisk live-root.img -n 1:2048 -t 1:0700 -c 1:"EDOS_DATA" --partition-guid=1:$(PARTITION_UUID)
	cargo build --release --manifest-path tools/efs-mkfs/Cargo.toml
	tools/efs-mkfs/target/release/efs-mkfs --partition-offset 1048576 --populate filesystem/ --label EDOS live-root.img

# A running guest holds a write lock on the qcow2, so the convert at the end
# fails with "Failed to get write lock" and takes the whole build down with it.
# That reads as a test failure when this runs underneath `test` or
# `storage-check`, so retire the guest first rather than explain it afterwards.
# The stop refuses, and so fails this rule, while a gate is driving the guest:
# rebuilding the disk out from under a running gate is the worse outcome, and
# the refusal names the gate holding it.
sata-disk.img: filesystem/.manifest tools/efs-mkfs/src/*.rs libs/efs-common/src/*.rs
	scripts/edos-vm stop >/dev/null
	qemu-img create -f raw sata-disk.raw 5G
	sgdisk sata-disk.raw -n 1:2048 -t 1:0700 -c 1:"EDOS_DATA" --partition-guid=1:$(PARTITION_UUID)
	cargo build --release --manifest-path tools/efs-mkfs/Cargo.toml
	tools/efs-mkfs/target/release/efs-mkfs --partition-offset 1048576 --populate filesystem/ --label EDOS sata-disk.raw
	qemu-img convert -f raw -O qcow2 sata-disk.raw sata-disk.img
	rm -f sata-disk.raw

# Populated the same way as sata-disk.img, but stays raw: `run-nvme` and
# `-device nvme` want `format=raw`, so there is no qcow2 conversion step here.
# Its partition GUID differs from $(PARTITION_UUID) for the same reason
# journal-test.img's does: `run_qemu_uefi` always attaches sata-disk.img, root
# selection matches the cmdline GUID across every enumerated partition, and two
# partitions carrying one GUID make which disk boots a race. Booting the NVMe
# disk as root means attaching it alone and asking for this GUID instead.
NVME_UUID := 6e766d65-0000-4000-8000-00000000ed05
nvme-disk.img: filesystem/.manifest tools/efs-mkfs/src/*.rs libs/efs-common/src/*.rs
	scripts/edos-vm stop >/dev/null
	rm -f nvme-disk.img
	qemu-img create -f raw nvme-disk.img 2G >/dev/null
	sgdisk nvme-disk.img -n 1:2048 -t 1:0700 -c 1:"EDOS_DATA" --partition-guid=1:$(NVME_UUID)
	cargo build --release --manifest-path tools/efs-mkfs/Cargo.toml
	tools/efs-mkfs/target/release/efs-mkfs --partition-offset 1048576 --populate filesystem/ --label EDOS nvme-disk.img

.PHONY: efs-fsck
efs-fsck:
	cargo build --release --manifest-path tools/efs-fsck/Cargo.toml

# The integration tests build their fixtures by running `efs-mkfs` out of its
# release target dir, and fail rather than skip when it is absent.
.PHONY: check-fsck
check-fsck: efs-mkfs
	cargo test --release --manifest-path tools/efs-fsck/Cargo.toml

.PHONY: efs-mkfs
efs-mkfs:
	cargo build --release --manifest-path tools/efs-mkfs/Cargo.toml

.PHONY: check-mkfs
check-mkfs:
	cargo test --release --manifest-path tools/efs-mkfs/Cargo.toml

.PHONY: clean-sata
clean-sata:
	rm -f sata-disk.img sata-disk.raw

# An unpartitioned disk for the install gate: `edos-install` writes its own GPT,
# ESP and root partition, so anything already here would only be overwritten.
nvme-blank.img:
	rm -f nvme-blank.img
	qemu-img create -f raw nvme-blank.img 4G >/dev/null

# The install case writes a partition table and a root filesystem onto this
# image, so a second run would install over an installed disk rather than a
# blank one. Recreate it for every gate run.
.PHONY: fresh-nvme-blank
fresh-nvme-blank:
	rm -f nvme-blank.img
	$(MAKE) nvme-blank.img

.PHONY: clean-nvme
clean-nvme:
	rm -f nvme-disk.img nvme-blank.img edos-nvme.iso edos-sata.iso

# Listed one per directory rather than brace-expanded: make runs recipes under
# /bin/sh, which is dash on Debian, and dash does not do brace expansion. It
# silently creates a single directory with the braces in its name instead.
FILESYSTEM_DIRS := bin boot dev etc home home/edos lib var mnt opt root sys tmp share share/fonts share/wallpapers share/icons share/sounds share/web

.PHONY: publish
# Build the package archives, write the index and sign it. Needs the repository
# key; see doc/grab.md.
publish: programs
	cargo build --release --manifest-path tools/grab-repo/Cargo.toml
	tools/grab-repo/target/release/grab-repo

.PHONY: filesystem
# Outline faces for the shell. Lato sets the chrome and JetBrains Mono the
# terminal; both are OFL and both ship in Debian, so they are copied from the
# host rather than committed here. A missing face is not fatal: edos_render
# falls back to its built-in bitmap font, and the shell comes up looking like
# it did before outlines.
SANS_DIR := /usr/share/fonts/truetype/lato
MONO_DIR := /usr/share/fonts/truetype/jetbrains-mono
FONT_COPIES := \
	$(SANS_DIR)/Lato-Regular.ttf:Sans-Regular.ttf \
	$(SANS_DIR)/Lato-Medium.ttf:Sans-Medium.ttf \
	$(SANS_DIR)/Lato-Semibold.ttf:Sans-Semibold.ttf \
	$(MONO_DIR)/JetBrainsMono-Regular.ttf:Mono-Regular.ttf

# The wallpapers the compositor offers alongside its generated grounds. Built
# from a formula rather than committed, because this repo holds no binaries;
# the rule depends on the script so an unchanged wallpaper keeps its timestamp
# and does not drag both disk images through a rebuild.
WALLPAPERS := filesystem/share/wallpapers/dusk.bmp

$(WALLPAPERS): scripts/mkwallpaper.py
	mkdir -p filesystem/share/wallpapers
	python3 scripts/mkwallpaper.py filesystem/share/wallpapers

# Generated on the same terms as the wallpapers, and for the same reason.
SOUNDS := filesystem/share/sounds/chime.wav

$(SOUNDS): scripts/mksounds.py
	mkdir -p filesystem/share/sounds
	python3 scripts/mksounds.py filesystem/share/sounds

filesystem: $(WALLPAPERS) $(SOUNDS)
	mkdir -p filesystem $(addprefix filesystem/,$(FILESYSTEM_DIRS))
	cp -u assets/edos.svg filesystem/share/icons/edos.svg
	cp -u assets/welcome.html filesystem/share/web/welcome.html
	@# The caching resolver is on unless this file is removed, which is what
	@# init's `enabled_by` reads. Not overwritten: an edited upstream survives
	@# a rebuild.
	@test -f filesystem/etc/lookupd.conf || printf '%s\n' \
		'# Upstream resolver for lookupd: an address, or dhcp for the leased one.' \
		'dhcp' > filesystem/etc/lookupd.conf
	@for pair in $(FONT_COPIES); do \
		src=$${pair%%:*}; dst=$${pair##*:}; \
		if [ -f "$$src" ]; then \
			cp -u "$$src" filesystem/share/fonts/$$dst; \
		else \
			echo "warning: $$src not found, shell falls back to the bitmap font" >&2; \
		fi; \
	done
