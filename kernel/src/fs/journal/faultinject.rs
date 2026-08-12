//! Runtime control over journal checkpointing, for recovery testing.
//!
//! Journal recovery is invisible to every ordinary test: a clean unmount leaves
//! nothing to replay, and writeback checkpoints so promptly that an unclean cut
//! taken at an arbitrary moment finds the journal empty too. Reaching replay at
//! all therefore needs committed transactions to be held in the ring on purpose,
//! which is what pausing checkpointing does.
//!
//! Checkpointing is the only thing that reclaims ring space, so a paused journal
//! wedges writes once the ring fills: a commit that cannot find room checkpoints
//! and re-checks a bounded number of times and then fails. That is the intended
//! shape. Pause late, immediately before the cut, rather than for the length of
//! a workload.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::{
    fs::{DevFsDevice, DevFsError, register_device_str},
    log,
};

/// Whether writeback is currently forbidden from checkpointing journalled
/// blocks to their home locations.
static CHECKPOINT_PAUSED: AtomicBool = AtomicBool::new(false);

/// Read by the writeback path for every journalled page it considers.
///
/// `Relaxed` is deliberate: this gates a debugging behaviour, and a pause that
/// takes effect one page later than the write that requested it changes nothing
/// a test can observe.
pub fn checkpoint_paused() -> bool {
    CHECKPOINT_PAUSED.load(Ordering::Relaxed)
}

/// `/dev/journal-ctl`: write `pause` or `resume`, read back the current state.
struct JournalCtl;

impl DevFsDevice for JournalCtl {
    fn read(&self, offset: usize, count: usize) -> Result<Vec<u8>, DevFsError> {
        let state: &[u8] = if checkpoint_paused() {
            b"checkpoint: paused\n"
        } else {
            b"checkpoint: running\n"
        };
        let start = offset.min(state.len());
        let end = (start + count).min(state.len());
        Ok(state[start..end].to_vec())
    }

    fn write(&self, _offset: usize, data: &[u8]) -> Result<usize, DevFsError> {
        let cmd = core::str::from_utf8(data)
            .map_err(|_| DevFsError::Unsupported)?
            .trim();
        // A writer that emits its terminating newline as its own call is
        // ordinary; `write(1)` does exactly that. Rejecting the empty write
        // reports a failure for a command that was already carried out.
        if cmd.is_empty() {
            return Ok(data.len());
        }
        match cmd {
            "pause" => CHECKPOINT_PAUSED.store(true, Ordering::Relaxed),
            "resume" => CHECKPOINT_PAUSED.store(false, Ordering::Relaxed),
            _ => return Err(DevFsError::Unsupported),
        }
        log!("journal-ctl: checkpointing {}", cmd);
        // Report the whole write as consumed: the caller wrote a command, not
        // a byte range, and a short count would make a shell redirect retry.
        Ok(data.len())
    }

    fn size(&self) -> u64 {
        0
    }
}

pub fn init() {
    let device: Arc<dyn DevFsDevice> = Arc::new(JournalCtl);
    if let Err(err) = register_device_str("/journal-ctl", device) {
        log!("journal-ctl: failed to register: {:?}", err);
    }
}
