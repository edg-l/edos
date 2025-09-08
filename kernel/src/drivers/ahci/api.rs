use alloc::{collections::VecDeque, vec::Vec};
use crossbeam_queue::SegQueue;

use crate::{
    drivers::ahci::port::AhciPort,
    println,
    thread::{ThreadId, scheduler::sched},
};

#[derive(Debug)]
pub enum Command {
    Read {
        lba: u64,
        sectors: u16,
        result_sender: ThreadId,
    },
    Write {
        lba: u64,
        data: Vec<u8>,
        sectors: u16,
        result_sender: ThreadId,
    },
    Flush {
        result_sender: ThreadId,
    },
    Identify {
        result_sender: ThreadId,
    },
}
