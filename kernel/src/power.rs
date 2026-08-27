//! Machine power control: ACPI soft-off (S5) and platform reset.
//!
//! Soft-off needs two things from the firmware tables: the PM1 control
//! register, which the FADT gives directly, and the SLP_TYP value for S5,
//! which lives in the `\_S5_` package in the DSDT. EDOS has no AML
//! interpreter, so the package is decoded structurally: ACPI 6.5 §7.4.2.6
//! defines `\_S5` as a package whose first element is the SLP_TYPa value,
//! and the encoding of a name followed by a small constant package is fixed
//! (ACPI 6.5 §20.2), so the few bytes after the name are enough.
//!
//! Progress goes to the serial port rather than the log ring buffer: the
//! buffer is drained by a kthread that will not run again once the machine
//! stops, so a `log!` line here would be written to memory nobody reads.

use acpi::{Handler, address::AddressSpace, sdt::fadt::Fadt};
use x86_64::instructions::{
    hlt, interrupts,
    port::{Port, PortWriteOnly},
};

use crate::{
    acpi::{acpi_tables, handler::AcpiHandler},
    println,
    syscalls::io::sync_all,
};

/// SLP_EN in PM1_CNT: writing it with SLP_TYP starts the transition.
const SLP_EN: u16 = 1 << 13;

/// Ports that emulators answer soft-off on when the tables cannot be read:
/// QEMU (piix4 and q35), Bochs, and VirtualBox.
const EMULATOR_SOFT_OFF: [(u16, u16); 3] = [(0x604, 0x2000), (0xB004, 0x2000), (0x4004, 0x3400)];

/// The reset control register of the PCI host bridge (ICH/PCH `RST_CNT`).
const RESET_CONTROL: u16 = 0xCF9;

/// 8042 command port; pulsing the reset line is the fallback reset.
const PS2_COMMAND: u16 = 0x64;

/// Flush every filesystem, then power the machine off. Never returns: if no
/// mechanism works the calling CPU halts rather than resuming a userspace
/// process on a machine that was told to stop.
pub fn power_off() -> ! {
    quiesce("power off");
    acpi_soft_off();
    for (port, value) in EMULATOR_SOFT_OFF {
        // SAFETY: reached only after `acpi_soft_off` returned, meaning the
        // tables described no usable S5. On real hardware these ports belong to
        // no device that answers this value, so the write is inert; on the
        // emulators listed it is the documented soft-off register.
        unsafe { PortWriteOnly::new(port).write(value) };
    }
    println!("power off: no soft-off mechanism answered, halting");
    halt_forever();
}

/// Flush every filesystem, then stop this CPU without asking the platform to
/// do anything. What a machine with no power management can offer. Never
/// returns.
pub fn halt() -> ! {
    quiesce("halt");
    println!("halt: system halted");
    halt_forever();
}

/// Flush every filesystem, then reset the machine. Never returns.
pub fn reboot() -> ! {
    quiesce("reboot");

    // RST_CNT: SYS_RST selects a reset, RST_CPU triggers it. A hard reset
    // (bit 3) also cycles the CPU, which is what a warm boot wants.
    let mut reset = Port::<u8>::new(RESET_CONTROL);
    // SAFETY: 0xCF9 is the chipset's reset control register and belongs to no
    // driver here. The filesystems are already synced, so a reset that does
    // land loses nothing.
    unsafe {
        reset.write(0x02);
        reset.write(0x0E);
    }

    // 8042 pulse: command 0xFE drives the reset line low on chipsets that
    // do not implement RST_CNT.
    // SAFETY: 0x64 is the 8042 command port. The PS/2 controller is driven
    // through the same port elsewhere, but every other CPU has been told to
    // stop and this write is the last thing before the machine resets.
    unsafe { PortWriteOnly::<u8>::new(PS2_COMMAND).write(0xFE) };

    println!("reboot: no reset mechanism answered, halting");
    halt_forever();
}

/// Get the filesystems to a state the next boot does not have to repair, and
/// stop taking new work. Interrupts stay on: `sync_all` commits the journal
/// and waits for the writeback path, which parks.
fn quiesce(what: &str) {
    debug_assert!(
        interrupts::are_enabled(),
        "power::quiesce called with interrupts disabled"
    );
    println!("{what}: syncing filesystems");
    sync_all();
    println!("{what}: filesystems synced");
    // After the sync, so anything the filesystems just wrote is inside the
    // cache this commits, and before the power transition, which gives the
    // controller no chance to write it down.
    crate::drivers::nvme::shutdown_all();
}

/// Write SLP_TYPa + SLP_EN to PM1a_CNT (and PM1b_CNT when the platform has
/// one). Returns normally if the tables do not describe a usable S5, so the
/// caller can fall back.
fn acpi_soft_off() {
    let Some(slp_typ) = s5_sleep_type() else {
        println!("power off: no \\_S5_ package in the DSDT");
        return;
    };
    let Some(fadt) = acpi_tables().find_table::<Fadt>() else {
        println!("power off: no FADT");
        return;
    };

    let value = ((slp_typ as u16) << 10) | SLP_EN;
    if let Ok(pm1a) = fadt.pm1a_control_block()
        && pm1a.address_space == AddressSpace::SystemIo
        && pm1a.address != 0
    {
        // SAFETY: the FADT named this port as PM1a_CNT and said it lives in
        // I/O space, which is the only thing that makes the address a port at
        // all; writing SLP_TYP + SLP_EN to it is the transition ACPI 6.5 §4.8.3
        // defines.
        unsafe { PortWriteOnly::new(pm1a.address as u16).write(value) };
    }
    if let Ok(Some(pm1b)) = fadt.pm1b_control_block()
        && pm1b.address_space == AddressSpace::SystemIo
        && pm1b.address != 0
    {
        // SAFETY: as for PM1a, with the optional PM1b_CNT block.
        unsafe { PortWriteOnly::new(pm1b.address as u16).write(value) };
    }
}

/// Find `\_S5_` in the DSDT and return its SLP_TYPa element.
///
/// The object is `Name(_S5, Package() { typa, typb, ... })`, which encodes as
/// NameOp, an optional root/prefix path, the four-character name, PackageOp,
/// a PkgLength, an element count, then the elements. Only constant integers
/// appear here in practice, so the first element decodes from its opcode.
fn s5_sleep_type() -> Option<u8> {
    let dsdt = acpi_tables().dsdt().ok()?;
    let body_offset = size_of::<acpi::sdt::SdtHeader>();
    let length = dsdt.length as usize;
    if length <= body_offset {
        return None;
    }
    // SAFETY: the address and length come from the DSDT's own entry in the
    // ACPI tables, so they describe a table the firmware placed in memory, and
    // `length` is checked above to be larger than the header it starts with.
    let mapping = unsafe { AcpiHandler.map_physical_region::<u8>(dsdt.phys_address, length) };
    // SAFETY: `map_physical_region` mapped exactly `length` bytes at
    // `virtual_start`, and `mapping` outlives `aml`. The bytes are AML, which
    // is read here as bytes and never as a typed value.
    let aml = unsafe { core::slice::from_raw_parts(mapping.virtual_start.as_ptr(), length) };

    let mut result = None;
    for i in body_offset..length.saturating_sub(4) {
        if &aml[i..i + 4] != b"_S5_" {
            continue;
        }
        // Guard against the bytes appearing inside a buffer: the name is
        // introduced by NameOp, optionally through a root or parent prefix.
        let introduced = matches!(aml.get(i.wrapping_sub(1)), Some(0x08))
            || (matches!(aml.get(i.wrapping_sub(1)), Some(0x5C | 0x5E))
                && matches!(aml.get(i.wrapping_sub(2)), Some(0x08)));
        if !introduced {
            continue;
        }
        result = parse_s5_package(&aml[i + 4..]);
        if result.is_some() {
            break;
        }
    }
    result
}

/// Decode `PackageOp PkgLength NumElements <first element>` and return the
/// first element's integer value.
fn parse_s5_package(bytes: &[u8]) -> Option<u8> {
    let mut at = 0usize;
    if *bytes.get(at)? != 0x12 {
        return None;
    }
    at += 1;
    // PkgLength: the top two bits of the lead byte give how many extra bytes
    // follow it (ACPI 6.5 §20.2.4).
    at += 1 + ((*bytes.get(at)? >> 6) as usize);
    // NumElements, then the first element.
    at += 1;
    match *bytes.get(at)? {
        0x00 => Some(0),
        0x01 => Some(1),
        0x0A => bytes.get(at + 1).copied(),
        _ => None,
    }
}

/// Park this CPU with interrupts off. Every other CPU keeps running, which is
/// only reached when the machine refused to stop.
fn halt_forever() -> ! {
    interrupts::disable();
    loop {
        hlt();
    }
}
