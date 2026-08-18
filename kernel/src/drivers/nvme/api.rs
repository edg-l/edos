//! The barrier and the registry the rest of the kernel reads NVMe through.
//!
//! Everything here is callable before the driver kthread has run: the probe
//! publishes both `Once` cells exactly once, whether or not it found a
//! controller, so a waiter cannot hang on a machine with no NVMe hardware.

use alloc::{sync::Arc, vec::Vec};

use crate::drivers::nvme::{NVME_NAMESPACES, NVME_PROBE_DONE, namespace::NvmeNamespace};

/// Block until the NVMe probe has registered every namespace it accepted
/// with `block_io`. The AHCI analogue is `ahci::api::list_devices`.
pub fn wait_probe_complete() {
    NVME_PROBE_DONE.wait();
}

/// Every namespace the probe accepted, in registration order. Empty when
/// the probe found no controller or refused every namespace it saw.
pub fn namespaces() -> &'static Vec<Arc<NvmeNamespace>> {
    NVME_NAMESPACES.wait()
}
