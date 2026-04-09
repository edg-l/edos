# EDOS

An x86_64 operating system written in Rust, featuring a graphical window manager, preemptive multitasking, TCP/IP networking, and a custom Rust standard library port.

## Features

- **SMP kernel** with preemptive scheduler, work-stealing, per-CPU run queues
- **Graphical window manager** with compositor, decorations, drag/resize, 60 FPS render loop
- **Terminal emulator** with PTY support and ANSI escape codes
- **Shell** with pipes, redirects, scripting (if/while/for/functions), job control
- **FAT32 filesystem** on SATA/AHCI, plus memfs, procfs, devfs
- **TCP/IP networking** with e1000e NIC driver, DHCP, DNS, HTTP
- **Rust `std::net`** support (TcpStream, TcpListener, UdpSocket with DNS resolution)
- **USB** via xHCI (keyboard, mouse, mass storage)
- **Signals**, shared memory, futexes, pipes

### Network Stack

Full TCP/IP stack in the kernel:
- **e1000e NIC driver** with MSI-X interrupts (works on real Intel I219/I218 NICs)
- **Ethernet, ARP, IPv4, ICMP** (ping works)
- **UDP** with socket syscalls
- **TCP** with full RFC 793 state machine, MSS negotiation, retransmit timer, dynamic receive window
- **DHCP** client (auto-configures IP, mask, gateway, DNS)
- **DNS** resolution (via `std::net` or standalone `dns` program)
- **IP fragment reassembly** with interval merging and 30s timeout
- **Loopback** (127.0.0.1) support

### User Programs

- `edos-wm` -- Window manager/compositor
- `edos-terminal` -- Terminal emulator
- `edos-taskbar` -- System taskbar
- `edos-sh` -- Shell with builtins (cd, ls, cat, echo, pwd, clear, ifconfig, etc.)
- `http` -- curl-like HTTP client with DNS, URL parsing, `-i`/`-v` flags
- `dns` -- DNS A-record lookup
- `ping` -- ICMP ping with RTT stats
- `wintest` -- Window system test app

## Building

### Dependencies

- GNU make
- Rust (custom toolchain, see below)
- `xorriso` (ISO creation)
- `sgdisk`, `mtools` (disk image creation)
- QEMU with x86_64 support (for running)

### Build and Run

```bash
# Build kernel, programs, and bootable ISO
make all

# Run in QEMU with KVM (4 cores, e1000e networking)
make run

# Run with network packet capture to /tmp/edos.pcap
make run-capture

# Run with single CPU
make run-single

# Run with GDB server
make run-gdb

# Build only kernel or programs
make kernel
make programs

# Type check without building
make check

# Format code
make fmt
```

### Custom Rust Toolchain

EDOS uses a custom Rust toolchain with standard library support for the `x86_64-unknown-edos` target.

- **Toolchain**: [github.com/edg-l/rust](https://github.com/edg-l/rust/tree/edos_std_v2) (branch `edos_std_v2`)
- **Runtime library**: [github.com/edg-l/edos_rt](https://github.com/edg-l/edos_rt) (published on crates.io)
- **Target triple**: `x86_64-unknown-edos`

Programs are built with `cargo +edos build --target x86_64-unknown-edos`.

#### Toolchain Setup

```bash
# Clone the Rust fork
git clone -b edos_std_v2 https://github.com/edg-l/rust.git

# Build and install the toolchain
cd rust
./x install
```

The toolchain installs to the path configured in `bootstrap.toml`. Programs in `programs/` reference it via `rust-toolchain.toml`.

#### Updating edos_rt

1. Make changes in the edos_rt repo
2. Bump version in `Cargo.toml`
3. `cargo publish` (or `cargo publish --allow-dirty`)
4. Update the version in the Rust fork's `library/std/Cargo.toml`
5. Rebuild toolchain: `cd rust && ./x install`
6. Rebuild programs: `cd edos-v2 && make all`

## Running on Real Hardware

The ISO is hybrid and can be written directly to a USB stick:

```bash
sudo dd if=edos-x86_64.iso of=/dev/sdX bs=4M status=progress conv=fsync
```

The e1000e driver works on real Intel I219/I218/I217 NICs. DHCP will auto-configure networking if a DHCP server is available.

For the writable filesystem, flash `sata-disk.img` to a spare drive.

## Prebuilt Releases

Download from the releases page:
1. `edos-x86_64.iso` -- bootable hybrid ISO
2. `filesystem.tar.gz` -- root filesystem contents
3. `create-filesystem-image.sh` -- helper to build the writable disk image

```bash
# Recreate the filesystem disk image
chmod +x create-filesystem-image.sh
./create-filesystem-image.sh --output sata-disk.img
```

## Debugging

```bash
# Start QEMU paused with GDB server
make run-gdb

# In another terminal
make gdb   # Uses pwndbg

# Resolve kernel panic addresses
addr2line -e kernel/target/x86_64-unknown-none/debug/edos-kernel -f 0xffffffff8009c422

# Capture network traffic for Wireshark analysis
make run-capture
# Then: wireshark /tmp/edos.pcap
```

## Architecture

```
kernel/src/
  memory/     -- Physical frame allocator, page tables, virtual allocator, shared memory
  thread/     -- Scheduler, context switching, mutexes, pipes, PTYs, polling
  syscalls/   -- SYSCALL/SYSRET interface (file I/O, memory, windows, networking, sync)
  fs/         -- VFS with FAT32, memfs, procfs, devfs
  drivers/    -- AHCI/SATA, e1000e NIC, keyboard, mouse, HPET, xHCI USB
  net/        -- TCP/IP stack (ethernet, ARP, IPv4, ICMP, UDP, TCP, DHCP, DNS, sockets)
  graphics/   -- Framebuffer rendering, draw request queue
  window/     -- Window registry, input routing, focus management

programs/
  edos_render -- Shared rendering library (textures, widgets, window syscall wrappers)
  edos_lib    -- Userspace utility library (net helpers, SHM, process, I/O)
  edos-wm     -- Window manager/compositor
  edos-terminal -- Terminal emulator with PTY
  edos-taskbar  -- System taskbar
  edos-sh     -- Command-line shell
  http        -- HTTP client (uses std::net)
  dns         -- DNS lookup tool
  ping        -- ICMP ping tool
```
