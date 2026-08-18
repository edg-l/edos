//! NVMe block driver (NVMe Base Specification 2.0, NVM Command Set
//! Specification 1.0).
//!
//! Structured as a direct analogue of the AHCI driver: a named kthread
//! probes PCI, brings controllers up, and identifies every namespace they
//! report.

use alloc::vec::Vec;
use thiserror::Error;

use crate::{
    drivers::{
        nvme::{admin::NvmeController, identify::max_transfer_bytes},
        pci::{pci_manager, structures::PciDevice},
    },
    log,
    thread::{
        runqueue::IO_PRIORITY,
        scheduler::{current_thread, thread_park_while},
        util::queue_spawn_kthread_named,
    },
};

pub mod admin;
pub mod identify;
pub mod queue;
pub mod regs;

#[derive(Debug, Error, Clone, Copy)]
pub enum NvmeError {
    #[error("invalid device")]
    InvalidDevice,
    #[error(transparent)]
    DmaError(#[from] crate::drivers::dma::DmaError),
    #[error("controller timeout")]
    ControllerTimeout,
    /// The upper 16 bits of a failed completion's DW3 (Status Code Type,
    /// Status Code and related flags), as handed to `NvmeQueue::drain`.
    #[error("command failed, status={0:#x}")]
    CommandFailed(u16),
    #[error("unsupported controller")]
    Unsupported,
}

pub fn init() {
    queue_spawn_kthread_named("nvme", nvme_driver_main as *const () as u64);
}

pub extern "C" fn nvme_driver_main() -> ! {
    let thread = current_thread().unwrap();
    thread.set_priority(IO_PRIORITY);

    let devices: Vec<PciDevice> = pci_manager().read().get_devices().to_vec();

    let mut controllers = Vec::new();
    for device in devices {
        if device.header.class_code != 0x01
            || device.header.subclass != 0x08
            || device.header.prog_if != 0x02
        {
            continue;
        }
        match NvmeController::new(device) {
            Ok(controller) => controllers.push(controller),
            Err(e) => log!("nvme: failed to initialize controller: {:?}", e),
        }
    }

    for (controller_index, controller) in controllers.iter().enumerate() {
        let ident = match controller.identify_controller() {
            Ok(ident) => ident,
            Err(e) => {
                log!(
                    "nvme{}: identify controller failed: {:?}",
                    controller_index,
                    e
                );
                continue;
            }
        };
        let mdts_bytes = max_transfer_bytes(ident.mdts, regs::cap_mpsmin(controller.cap()));
        let vwc = ident.write_cache_present();
        let model = ident.model_trimmed();
        let serial = ident.serial_trimmed();

        let nsids = match controller.active_namespace_ids() {
            Ok(ids) => ids,
            Err(e) => {
                log!(
                    "nvme{}: active namespace list failed: {:?}",
                    controller_index,
                    e
                );
                continue;
            }
        };

        for nsid in nsids {
            match controller.identify_namespace(nsid) {
                Ok(ns) => {
                    log!(
                        "nvme{}n{}: {} sn={} {} LBAs of {} B, mdts={}, vwc={}",
                        controller_index,
                        nsid,
                        model,
                        serial,
                        ns.nsze,
                        ns.lba_size(),
                        mdts_bytes,
                        vwc
                    );
                }
                Err(e) => log!(
                    "nvme{}n{}: identify namespace failed: {:?}",
                    controller_index,
                    nsid,
                    e
                ),
            }
        }
    }

    thread_park_while(|| true);
    unreachable!("nvme kthread unparked with nothing to dispatch");
}
