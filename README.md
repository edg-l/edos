# edos


## Prebuilt Releases

You can run edos without building the project by downloading the assets that are produced for each tagged release (e.g. `v0.1.0-alpha.0`). Grab the following files from the release page and place them in the same directory:

1. `edos-x86_64.iso` - bootable hybrid ISO
2. `filesystem.tar.gz` - root filesystem contents
3. `create-filesystem-image.sh` - helper script to build the writable disk image

### Recreate the filesystem disk image

The operating system expects a writable SATA disk that mirrors the layout produced during development. Run the helper script to rebuild it locally (requires `qemu-img`, `sgdisk`, and the `mtools` utilities):

```bash
chmod +x create-filesystem-image.sh
./create-filesystem-image.sh --output sata-disk.img
```

By default the script looks for `filesystem.tar.gz` next to the script and generates a 1 GiB image called `sata-disk.img`. Use `./create-filesystem-image.sh --help` for additional options if you want a different size or filesystem source.

### Booting with QEMU

Once the ISO and data disk are present, you can boot edos under QEMU (BIOS mode):

```bash
qemu-system-x86_64 \
  -M q35 \
  -cpu qemu64,+x2apic \
  -m 2G \
  -smp 4 \
  -cdrom edos-x86_64.iso \
  -drive id=sata0,if=none,format=raw,file=sata-disk.img \
  -device ide-hd,drive=sata0,bus=ide.1 \
  -serial stdio \
  -no-reboot
```

For a UEFI boot flow, add OVMF firmware (e.g. `-drive if=pflash,unit=0,format=raw,file=OVMF_CODE.fd,readonly=on`).

### Running on real hardware

The ISO is hybrid and can be written directly to a USB stick:

```bash
sudo dd if=edos-x86_64.iso of=/dev/sdX bs=4M status=progress conv=fsync
```

Replace `/dev/sdX` with the correct device node (all existing data on the stick will be destroyed).

If you also want the writable filesystem, generate it with the helper script and flash the resulting `sata-disk.img` to a spare drive or secondary USB device.

## How to use this?

### Dependencies

Any `make` command depends on GNU make (`gmake`) and is expected to be run using it. This usually means using `make` on most GNU/Linux distros, or `gmake` on other non-GNU systems.

All `make all*` targets depend on Rust.

Additionally, building an ISO with `make all` requires `xorriso`, and building a HDD/USB image with `make all-hdd` requires `sgdisk` (usually from `gdisk` or `gptfdisk` packages) and `mtools`.

### Architectural targets

The `KARCH` make variable determines the target architecture to build the kernel and image for.

The default `KARCH` is `x86_64`. Other options include: `aarch64`, `riscv64`, and `loongarch64`.

Other architectures will need to be enabled in kernel/rust-toolchain.toml

### Makefile targets

Running `make all` will compile the kernel (from the `kernel/` directory) and then generate a bootable ISO image.

Running `make all-hdd` will compile the kernel and then generate a raw image suitable to be flashed onto a USB stick or hard drive/SSD.

Running `make run` will build the kernel and a bootable ISO (equivalent to make all) and then run it using `qemu` (if installed).

Running `make run-hdd` will build the kernel and a raw HDD image (equivalent to make all-hdd) and then run it using `qemu` (if installed).

The `run-uefi` and `run-hdd-uefi` targets are equivalent to their non `-uefi` counterparts except that they boot `qemu` using a UEFI-compatible firmware.
