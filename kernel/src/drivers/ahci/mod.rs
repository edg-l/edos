use core::{
    ptr,
    sync::atomic::{AtomicU64, Ordering},
};

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use spin::Once;
use thiserror::Error;
use x86_64::instructions::hlt;

use crate::{
    drivers::{
        ahci::{controller::AhciController, port::AhciPort, structures::DeviceIdentifyInfo},
        pci::{
            pci_manager,
            structures::{PciAddress, PciDevice},
        },
    },
    log,
    thread::{
        runqueue::IO_PRIORITY,
        scheduler::sched,
        thread::{Thread, get_thread_weak},
        util::queue_spawn_kthread_named,
    },
};

pub mod api;
pub mod cancel_op;
pub mod controller;
pub mod fis;
pub mod port;
pub mod structures;
pub mod watchdog;

/// Registry of AHCI ports indexed by `DetectedDevice.id`. Kept here so the
/// IRQ dispatcher and ATAPI-skip predicate can reach a port without going
/// through the block-io trait registry.
pub(super) static AHCI_PORTS: Once<Vec<Arc<AhciPort>>> = Once::new();

/// Returns true if the AHCI device with `device_id` is an ATAPI device
/// (CD/DVD). The partition scanner skips these because EDOS has no
/// ISO9660 driver and probing CD-ROMs is artificially slow under QEMU.
pub fn is_atapi(device_id: u64) -> bool {
    AHCI_PORTS
        .get()
        .and_then(|p| p.get(device_id as usize))
        .map(|p| p.device_type == DeviceType::Atapi)
        .unwrap_or(false)
}

#[derive(Debug, Error, Clone, Copy)]
pub enum AhciError {
    #[error("invalid device")]
    InvalidDevice,
    #[error(transparent)]
    DmaError(#[from] crate::drivers::dma::DmaError),
    #[error("port not ready")]
    PortNotReady,
    #[error("command timeout")]
    CommandTimeout,
    #[error("i/o error")]
    IoError,
    #[error("invalid command slot")]
    InvalidSlot,
    #[error("device is read-only")]
    ReadOnly,
}

impl From<crate::drivers::block_io::BlockError> for AhciError {
    fn from(e: crate::drivers::block_io::BlockError) -> Self {
        use crate::drivers::block_io::BlockError;
        match e {
            BlockError::Timeout => AhciError::CommandTimeout,
            BlockError::DeviceGone => AhciError::InvalidDevice,
            BlockError::Io
            | BlockError::Cancelled
            | BlockError::InvalidArg
            | BlockError::NoMemory => AhciError::IoError,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    Ata,
    Atapi,
}

pub static AHCI_DRIVER_THREAD_ID: Once<Weak<Thread>> = Once::new();

pub static DETECTED_DEVICES: Once<Vec<DetectedDevice>> = Once::new();

/// Dispatcher ran its loop body (not just a spurious park-exit).
/// Incremented once per pass of the inner for-controller loop.
pub static AHCI_DISPATCHER_PASSES: AtomicU64 = AtomicU64::new(0);

/// Total `wake_thread` calls issued from `wake_all_slot_waiters` across all
/// ports. Lets the NCQ-timeout diagnostic decide whether the wake path was
/// active while we were parked.
pub static AHCI_SLOT_WAKES: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    AHCI_DRIVER_THREAD_ID.call_once(|| {
        let tid = queue_spawn_kthread_named("ahci", ahci_driver_main as *const () as u64);
        get_thread_weak(tid).expect("ahci kthread vanished before call_once")
    });
}

#[derive(Debug, Clone)]
pub struct DetectedDevice {
    pub id: u64,
    pub controller_pci_address: PciAddress,
    pub port_idx: usize,
    #[expect(unused)]
    pub device_info: DeviceIdentifyInfo,
    #[expect(unused)]
    pub device_type: DeviceType,
    #[expect(unused)]
    pub supports_ncq: bool,
    #[expect(unused)]
    pub ncq_depth: u8,
}

pub extern "C" fn ahci_driver_main() -> ! {
    let thread = sched().current_thread().unwrap();
    thread.set_priority(IO_PRIORITY);

    let devices: Vec<PciDevice> = pci_manager().read().get_devices().to_vec();

    let mut controllers = Vec::new();

    // Find and initialize AHCI controllers
    for device in devices {
        if device.header.class_code == 0x01 && device.header.subclass == 0x06 {
            match AhciController::new(device) {
                Ok(controller) => {
                    log!("AHCI controller initialized successfully");
                    controllers.push(controller);
                }
                Err(e) => {
                    log!("Failed to initialize AHCI controller: {:?}", e);
                }
            }
        }
    }

    if controllers.is_empty() {
        log!("No AHCI controllers found");
        loop {
            hlt();
        }
    }

    let mut detected_devices: Vec<DetectedDevice> = Vec::new();

    // Arc-wrap all ports first so weak_self is set before any command submission.
    let mut port_arcs: Vec<(Arc<AhciPort>, PciAddress, usize, bool, u8)> = Vec::new();
    for controller in &mut controllers {
        for port_idx in 0..controller.ports.len() {
            if let Some(port) = controller.ports[port_idx].take() {
                let arc = Arc::new(port);
                arc.set_weak_self(Arc::downgrade(&arc));
                port_arcs.push((
                    arc,
                    controller.pci_device.address,
                    port_idx,
                    controller.supports_ncq,
                    controller.num_command_slots,
                ));
            }
        }
    }

    // Now identify devices and initialize I/O pools via Arc (weak_self is set).
    let mut id = 0;
    let mut direct_ports: Vec<Arc<AhciPort>> = Vec::with_capacity(port_arcs.len());
    for (arc, pci_address, port_idx, supports_ncq_hba, num_command_slots) in port_arcs {
        match arc.identify_device() {
            Ok(device_info) => {
                device_info.print_info(port_idx);

                // NCQ enabled only if both HBA and device support it
                let supports_ncq = supports_ncq_hba
                    && device_info.supports_ncq
                    && arc.device_type == DeviceType::Ata;
                let ncq_depth = if supports_ncq {
                    device_info.ncq_queue_depth.min(num_command_slots)
                } else {
                    0
                };

                // Allocate per-slot DMA pools and command tables.
                if let Err(e) = arc.init_io_pools(ncq_depth, device_info.supports_fua) {
                    log!("Failed to init I/O pools for port {}: {:?}", port_idx, e);
                    continue;
                }

                detected_devices.push(DetectedDevice {
                    id,
                    controller_pci_address: pci_address,
                    port_idx,
                    device_info,
                    device_type: arc.device_type,
                    supports_ncq,
                    ncq_depth,
                });
                id += 1;
                direct_ports.push(arc);
            }
            Err(e) => {
                log!("Failed to identify device on port {}: {:?}", port_idx, e);
            }
        }
    }

    DETECTED_DEVICES.call_once(|| detected_devices.clone());

    // Publish ports for the IRQ dispatcher and ATAPI-skip predicate, and
    // register each AHCI port with the kernel-wide block-io registry so
    // fs layers can submit_read / submit_write / submit_flush via the
    // AsyncBlockDevice trait.
    for (id, port) in direct_ports.iter().enumerate() {
        crate::drivers::block_io::register(
            id as u64,
            port.clone() as Arc<dyn crate::drivers::block_io::AsyncBlockDevice>,
        );
    }
    AHCI_PORTS.call_once(|| direct_ports);

    let _ = queue_spawn_kthread_named(
        "ahci_watchdog",
        watchdog::watchdog_entry as *const () as u64,
    );

    // Interrupt dispatch loop.
    // MSI fires -> hardware ISR wakes this thread -> we dispatch per-slot wakeups.
    loop {
        sched().thread_park_while(|| {
            !controllers.iter().any(|c| {
                let hba_is = unsafe { ptr::read_volatile(&raw const (*c.hba).is) };
                hba_is != 0
            })
        });

        AHCI_DISPATCHER_PASSES.fetch_add(1, Ordering::Relaxed);

        for controller in &controllers {
            let hba_is = unsafe { ptr::read_volatile(&(*controller.hba).is) };
            if hba_is == 0 {
                continue;
            }

            let mut pending_ports = hba_is;
            while pending_ports != 0 {
                let port_idx = pending_ports.trailing_zeros() as usize;
                pending_ports &= pending_ports - 1;

                let port_regs = unsafe { &mut (*controller.hba).ports[port_idx] };
                let port_is = unsafe { ptr::read_volatile(&port_regs.is) };
                if port_is != 0 {
                    unsafe { ptr::write_volatile(&mut port_regs.is, port_is) };

                    // Hand off to the port's IRQ-side completion walker:
                    // completes any NCQ slot whose SACT bit cleared, fails
                    // all in-flight on TFES, and wakes legacy poll waiters.
                    if let Some(device) = detected_devices.iter().find(|d| {
                        d.controller_pci_address == controller.pci_device.address
                            && d.port_idx == port_idx
                    }) {
                        if let Some(port) = AHCI_PORTS.get().and_then(|p| p.get(device.id as usize))
                        {
                            port.on_port_irq(port_is);
                        }
                    }
                }
            }

            unsafe { ptr::write_volatile(&mut (*controller.hba).is, hba_is) };
        }
    }
}
