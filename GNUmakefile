# Nuke built-in rules and variables.
MAKEFLAGS += -rR
.SUFFIXES:

# Convenience macro to reliably declare user overridable variables.
override USER_VARIABLE = $(if $(filter $(origin $(1)),default undefined),$(eval override $(1) := $(2)))

# Target architecture to build for. Default to x86_64.
$(call USER_VARIABLE,KARCH,x86_64)

# Default user QEMU flags. These are appended to the QEMU command calls.
$(call USER_VARIABLE,QEMUFLAGS,-m 2G)

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
DISPLAY_VGA := -device VGA,vgamem_mb=32
DISPLAY_VIRTIO := -vga none -device virtio-vga,xres=1920,yres=1080 -display sdl
DISPLAY_VIRTIO_GTK := -vga none -device virtio-vga,xres=1920,yres=1080 -display gtk,zoom-to-fit=off

# QEMU runner function
# $(1) = boot media type (iso/hdd)
# $(2) = smp cores
# $(3) = extra flags
# $(4) = display device flags (defaults to DISPLAY_VIRTIO)
define run_qemu_uefi
	qemu-system-$(KARCH) \
		-M q35 \
		-cpu qemu64,+sse4.1,+sse4.2,+x2apic \
		-drive if=pflash,unit=0,format=raw,file=ovmf/ovmf-code-$(KARCH).fd,readonly=on \
		-drive if=pflash,unit=1,format=raw,file=ovmf/ovmf-vars-$(KARCH).fd \
		$(if $(filter iso,$(1)),-cdrom $(IMAGE_NAME).iso,-hda $(IMAGE_NAME).hdd) \
		-device isa-debug-exit,iobase=0xf4,iosize=0x04 \
		-serial stdio \
		-no-reboot \
		-drive id=sata0,if=none,format=raw,file=sata-disk.img \
		-device ide-hd,drive=sata0,bus=ide.1 \
		$(if $(4),$(4),$(DISPLAY_VIRTIO)) \
		-smp $(2) \
		$(3) \
		$(QEMUFLAGS)
endef

.PHONY: run-x86_64
run-x86_64: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img
	$(call run_qemu_uefi,iso,4,-accel kvm)

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

.PHONY: run-trace
run-trace: programs programsv2 limine/limine ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd sata-disk.img
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

.PHONY: run-kvm
run-emu: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).iso sata-disk.img
	$(call run_qemu_uefi,iso,4,-accel tcg)

.PHONY: run-hdd-x86_64
run-hdd-x86_64: ovmf/ovmf-code-$(KARCH).fd ovmf/ovmf-vars-$(KARCH).fd $(IMAGE_NAME).hdd sata-disk.img
	$(call run_qemu_uefi,hdd,4,)

gdb:
	#rust-gdb -x gdbinit
	pwndbg -x gdbinit

.PHONY: run-bios sata-disk.img
run-bios: $(IMAGE_NAME).iso
	qemu-system-$(KARCH) \
		-M q35 \
		-cdrom $(IMAGE_NAME).iso \
		-boot d \
		$(QEMUFLAGS)

.PHONY: run-hdd-bios sata-disk.img
run-hdd-bios: $(IMAGE_NAME).hdd
	qemu-system-$(KARCH) \
		-M q35 \
		-hda $(IMAGE_NAME).hdd \
		$(QEMUFLAGS)

ovmf/ovmf-code-$(KARCH).fd:
	mkdir -p ovmf
	curl -Lo $@ https://github.com/osdev0/edk2-ovmf-nightly/releases/latest/download/ovmf-code-$(KARCH).fd

ovmf/ovmf-vars-$(KARCH).fd:
	mkdir -p ovmf
	curl -Lo $@ https://github.com/osdev0/edk2-ovmf-nightly/releases/latest/download/ovmf-vars-$(KARCH).fd

limine/limine:
	rm -rf limine
	git clone https://github.com/limine-bootloader/limine.git --branch=v9.x-binary --depth=1
	$(MAKE) -C limine

.PHONY: kernel
kernel: programs programsv2
	$(MAKE) -C kernel CARGO_FLAGS="$(CARGO_FLAGS)"

.PHONY: check
check: programs programsv2
	$(MAKE) -C kernel check

$(IMAGE_NAME).iso: limine/limine kernel
	rm -rf iso_root
	mkdir -p iso_root/boot
	cp -v kernel/kernel iso_root/boot/
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
	$(MAKE) -C programsv2 clean
	rm -rf iso_root $(IMAGE_NAME).iso $(IMAGE_NAME).hdd

.PHONY: distclean
distclean: clean clean-sata
	$(MAKE) -C kernel distclean
	rm -rf limine ovmf


.PHONY: fmt
fmt:
	$(MAKE) -C kernel fmt
	$(MAKE) -C programs fmt

.PHONY: programs
programs: programsv2
	$(MAKE) -C programs build

.PHONY: programsv2
programsv2:
	$(MAKE) -C programsv2 build

DISK_UUID := 12345678-1234-5678-9abc-123456789abc
PARTITION_UUID := 87654321-4321-8765-cba9-987654321fed
FILESYSTEM_SERIAL := 305419896
FILESYSTEM_FILES := $(shell find filesystem -type f 2>/dev/null)

sata-disk.img: $(FILESYSTEM_FILES)
	qemu-img create -f raw sata-disk.img 1G
	sgdisk sata-disk.img -n 1:2048 -t 1:0700 -c 1:"EDOS_DATA" --partition-guid=1:$(PARTITION_UUID)
	mformat -F -i sata-disk.img@@1M -v EDOS
	if [ -d filesystem ]; then \
		mcopy -s -i sata-disk.img@@1M filesystem/* ::/; \
	fi

.PHONY: clean-sata
clean-sata:
	rm -f sata-disk.img

.PHONY: filesystem
filesystem:
	mkdir -p filesystem
	mkdir -p filesystem/{bin,dev,home,lib,var,mnt,opt,root,sys,tmp}
