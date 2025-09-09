use core::mem::ManuallyDrop;

use alloc::{boxed::Box, format, vec::Vec};
use x86_64::instructions::hlt;

use crate::{
    drivers::ahci::api::list_devices,
    fs::{
        fat32::Fat32fs,
        gpt::{Partition, parse_gpt, print_partitions},
    },
    println,
    thread::{
        scheduler::sched,
        util::{kthread_exit, queue_spawn_kthread_named, queue_spawn_kthread_named_arg},
    },
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
        sched().thread_park();
    }
}

extern "C" fn fs32_partition_thread(partition: *mut Partition) -> ! {
    let partition = unsafe { Box::from_raw(partition) };

    println!("Partition: {:#?}", partition);

    let Ok(fs) = Fat32fs::new((*partition).clone()) else {
        println!("Failed to create fat32");
        kthread_exit(-1)
    };

    let bytes = fs.boot_info.bytes_per_sector;
    println!("FAT32 bytes per sector: {}", bytes);

    let entries = fs.get_dir_entries(fs.boot_info.root_cluster).unwrap();

    println!("Showing root /");
    for entry in &entries {
        println!("Name: {}", entry.fat_name_to_string());
        println!("Is dir: {}", entry.is_directory());

        if entry.is_directory() {
            let entries = fs.get_dir_entries(entry.first_cluster()).unwrap();

            for entry in &entries {
                println!("Name: {}", entry.fat_name_to_string());
                println!("Is dir: {}", entry.is_directory());
            }
        } else {
            let content = fs.read_file(entry).unwrap();
            let x = core::str::from_utf8(&content);
            if let Ok(x) = x {
                println!("Content:\n{x:?}");
            }
        }
    }

    println!("{entries:#?}");

    loop {
        sched().thread_park();
    }
}
