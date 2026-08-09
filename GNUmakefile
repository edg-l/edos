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
DISPLAY_VIRTIO := -vga none -device virtio-vga,xres=1920,yres=1080,blob=on -display sdl
DISPLAY_VIRTIO_GTK := -vga none -device virtio-vga,xres=1920,yres=1080,blob=on -display gtk,zoom-to-fit=off

# Host audio backend for the emulated HDA device. `pipewire` needs a session
# bus, which a bare SSH login does not have; `AUDIODEV=none` runs the same
# device against a null backend.
AUDIODEV ?= pipewire

# QEMU runner function
# $(1) = boot media type (iso/hdd)
# $(2) = smp cores
# $(3) = extra flags
# $(4) = display device flags (defaults to DISPLAY_VIRTIO)
define run_qemu_uefi
	qemu-system-$(KARCH) \
		-M q35 \
		-cpu qemu64,+sse4.1,+sse4.2,+x2apic,+fsgsbase \
		-object memory-backend-memfd,id=mem1,size=$(QEMU_MEM) \
		-machine memory-backend=mem1 \
		-drive if=pflash,unit=0,format=raw,file=ovmf/ovmf-code-$(KARCH).fd,readonly=on \
		-drive if=pflash,unit=1,format=raw,file=ovmf/ovmf-vars-$(KARCH).fd \
		$(if $(filter iso,$(1)),-cdrom $(IMAGE_NAME).iso,-hda $(IMAGE_NAME).hdd) \
		-device isa-debug-exit,iobase=0xf4,iosize=0x04 \
		-chardev stdio,id=ser0,signal=off,logfile=run_log.txt \
		-serial chardev:ser0 \
		-no-reboot -d cpu_reset -D /tmp/qemu_reset.log \
		-drive id=sata0,if=none,format=qcow2,file=sata-disk.img,aio=$(QEMU_AIO),discard=unmap \
		-device ide-hd,drive=sata0,bus=ide.1 \
		$(if $(4),$(4),$(DISPLAY_VIRTIO)) \
		-device qemu-xhci -device usb-kbd -device usb-mouse \
		-netdev user,id=net0 -device e1000e,netdev=net0 \
		-audiodev $(AUDIODEV),id=snd0 \
		-device intel-hda -device hda-output,audiodev=snd0 \
		-smp $(2) \
		$(3) \
		$(QEMUFLAGS)
endef

.PHONY: run-x86_64
run-x86_64: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img
	$(call run_qemu_uefi,iso,4,-accel kvm -s -no-reboot -no-shutdown)

# Like `run` but swaps the SATA backend's aio=io_uring for aio=threads.
# Used to triage whether the AHCI NCQ timeout (8 MiB + fsync inflighttest
# after ~503 writes) is io_uring-specific on the host. If the stall
# disappears here, blame io_uring; otherwise the bug is deeper.
.PHONY: run-aio-threads
run-aio-threads: QEMU_AIO := threads
run-aio-threads: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img
	$(call run_qemu_uefi,iso,4,-accel kvm -s -no-reboot -no-shutdown)

.PHONY: run-vga
run-vga: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img
	$(call run_qemu_uefi,iso,4,-accel kvm,$(DISPLAY_VGA))

.PHONY: run-gtk
run-gtk: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img
	$(call run_qemu_uefi,iso,4,-accel kvm,$(DISPLAY_VIRTIO_GTK))

.PHONY: run-single
run-single: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img
	$(call run_qemu_uefi,iso,1,)

.PHONY: run-big
run-big: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img
	$(call run_qemu_uefi,iso,16,)

.PHONY: run-gdb
run-gdb: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img
	$(call run_qemu_uefi,iso,1,-no-shutdown -accel tcg -s -S)

.PHONY: run-gdb-4
run-gdb-4: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img
	$(call run_qemu_uefi,iso,4,-no-shutdown -accel tcg -s -S)

.PHONY: run-gdb-kvm
run-gdb-kvm: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img
	$(call run_qemu_uefi,iso,4,-no-shutdown -accel kvm -s -S)

# Boot with no local display: VNC for a human, QMP for scripts and agents.
# See scripts/edos-vm for screenshot, keyboard and pointer control.
.PHONY: run-headless
run-headless: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img
	scripts/edos-vm start

.PHONY: run-capture
run-capture: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img
	$(call run_qemu_uefi,iso,4,-accel kvm -object filter-dump$(comma)id=dump0$(comma)netdev=net0$(comma)file=/tmp/edos.pcap)

.PHONY: run-debug-fault
run-debug-fault: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img usb-test.img
	$(call run_qemu_uefi,iso,4,-no-shutdown -accel tcg -s -d int -D /tmp/qemu_fault.log -drive id=usbdisk0$(comma)if=none$(comma)format=raw$(comma)file=usb-test.img -device usb-storage$(comma)drive=usbdisk0)

.PHONY: run-storage
run-storage: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img usb-test.img
	$(call run_qemu_uefi,iso,4,-accel kvm -drive id=usbdisk0$(comma)if=none$(comma)format=raw$(comma)file=usb-test.img -device usb-storage$(comma)drive=usbdisk0)


usb-test.img:
	qemu-img create -f raw usb-test.img 16M

.PHONY: run-trace
run-trace: programs limine/limine ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd sata-disk.img
	$(MAKE) -C kernel CARGO_FLAGS="--features trace"
	$(MAKE) $(IMAGE_NAME).iso
	$(call run_qemu_uefi,iso,4,-accel kvm -m 2G)

.PHONY: test
test: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd sata-disk.img
	$(MAKE) $(IMAGE_NAME).iso CARGO_FLAGS="--features sched-test"
	$(call run_qemu_uefi,iso,4,-accel kvm -display none)

.PHONY: test-single
test-single: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd sata-disk.img
	$(MAKE) $(IMAGE_NAME).iso CARGO_FLAGS="--features sched-test"
	$(call run_qemu_uefi,iso,1,-display none)

# The same suite as `test`, against a null audio backend. `test` binds the
# host's PipeWire session, which a bare SSH login does not have, so this is the
# form that runs from a terminal with no desktop behind it.
.PHONY: test-headless
test-headless:
	$(MAKE) test AUDIODEV=none

# Storage regressions, both halves. `fs-regression` reboots between writing and
# verifying, so it catches data that never reached the disk; `fsbench-run`
# verifies every pattern it writes and reports throughput. Both drive a real
# guest through scripts/edos-vm and need the ISO already built.
.PHONY: storage-check
storage-check: $(IMAGE_NAME).iso
	scripts/fs-regression
	scripts/fs-regression --fat32
	scripts/fsbench-run


.PHONY: run-kvm
run-emu: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img
	$(call run_qemu_uefi,iso,4,-accel tcg)

.PHONY: run-hdd-x86_64
run-hdd-x86_64: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).hdd sata-disk.img
	$(call run_qemu_uefi,hdd,4,)

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

$(IMAGE_NAME).iso: limine/limine kernel live-root.img
	rm -rf iso_root
	mkdir -p iso_root/boot
	objcopy --strip-debug kernel/kernel iso_root/boot/kernel
	cp -v live-root.img iso_root/boot/
	mkdir -p iso_root/boot/limine
	cp -v limine.conf iso_root/boot/limine/
	mkdir -p iso_root/EFI/BOOT
	cp -v limine/limine-bios.sys limine/limine-bios-cd.bin limine/limine-uefi-cd.bin iso_root/boot/limine/
	cp -v limine/BOOTX64.EFI iso_root/EFI/BOOT/
	cp -v limine/BOOTIA32.EFI iso_root/EFI/BOOT/
	xorriso -as mkisofs -b boot/limine/limine-bios-cd.bin \
		-no-emul-boot -boot-load-size 4 -boot-info-table \
		--efi-boot boot/limine/limine-uefi-cd.bin \
		-efi-boot-part --efi-boot-image --protective-msdos-label \
		iso_root -o $(IMAGE_NAME).iso
	./limine/limine bios-install $(IMAGE_NAME).iso
	rm -rf iso_root

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
distclean: clean clean-sata
	$(MAKE) -C kernel distclean
	rm -rf limine ovmf


.PHONY: fmt
fmt:
	$(MAKE) -C kernel fmt

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
define update-manifest
find filesystem -type f ! -name '*.rlib' ! -name '*.a' ! -name '.manifest*' \
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
live-root.img: kernel limine/limine filesystem/.manifest tools/efs-mkfs/src/*.rs libs/efs-common/src/*.rs
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

sata-disk.img: filesystem/.manifest tools/efs-mkfs/src/*.rs libs/efs-common/src/*.rs
	qemu-img create -f raw sata-disk.raw 5G
	sgdisk sata-disk.raw -n 1:2048 -t 1:0700 -c 1:"EDOS_DATA" --partition-guid=1:$(PARTITION_UUID)
	cargo build --release --manifest-path tools/efs-mkfs/Cargo.toml
	tools/efs-mkfs/target/release/efs-mkfs --partition-offset 1048576 --populate filesystem/ --label EDOS sata-disk.raw
	qemu-img convert -f raw -O qcow2 sata-disk.raw sata-disk.img
	rm -f sata-disk.raw

.PHONY: efs-fsck
efs-fsck:
	cargo build --release --manifest-path tools/efs-fsck/Cargo.toml

.PHONY: check-fsck
check-fsck:
	cargo test --release --manifest-path tools/efs-fsck/Cargo.toml

.PHONY: clean-sata
clean-sata:
	rm -f sata-disk.img sata-disk.raw

# Listed one per directory rather than brace-expanded: make runs recipes under
# /bin/sh, which is dash on Debian, and dash does not do brace expansion. It
# silently creates a single directory with the braces in its name instead.
FILESYSTEM_DIRS := bin boot dev home lib var mnt opt root sys tmp

.PHONY: filesystem
filesystem:
	mkdir -p filesystem $(addprefix filesystem/,$(FILESYSTEM_DIRS))
