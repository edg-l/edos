use core::mem::ManuallyDrop;

use alloc::{boxed::Box, format, vec::Vec};
use x86_64::instructions::hlt;

use crate::{
    drivers::ahci::api::list_devices,
    fs::gpt::{ParsedPartition, parse_gpt, print_partitions},
    println,
    thread::util::{queue_spawn_kthread_named, queue_spawn_kthread_named_arg},
};

use super::gpt::FilesystemType;

pub extern "C" fn fs_main_thread() -> ! {
    let devices = list_devices();

    let mut partitions = Vec::new();

    for device in &devices {
        match parse_gpt(device.id) {
            Ok(found_partitions) => {
                print_partitions(&found_partitions);
                partitions.extend(found_partitions);
            }
            Err(err) => println!("Error parsing GPT: {err}"),
        }
    }

    // Maybe for each partition create a thread, and use this thread to route requests?

    for partition in &partitions {
        if let Some(filesystem) = &partition.filesystem {
            match filesystem {
                FilesystemType::Fat32 => {
                    let part = Box::new(partition.clone());
                    let part = &raw mut *Box::leak(part);
                    queue_spawn_kthread_named_arg(
                        &format!("fat32-fs-{}", partition.index),
                        fs32_partition_thread as u64,
                        part.cast(),
                    );
                }
                FilesystemType::Unknown => {}
            }
        }
    }

    loop {
        hlt();
    }
}

extern "C" fn fs32_partition_thread(partition: *mut ParsedPartition) -> ! {
    let partition = unsafe { Box::from_raw(partition) };

    println!("Partition: {:#?}", partition);
    loop {
        hlt();
    }
}
