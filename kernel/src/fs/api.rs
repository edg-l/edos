use core::{ffi::CStr, mem::ManuallyDrop};

use alloc::{boxed::Box, format, vec::Vec};
use x86_64::instructions::hlt;

use crate::{
    allocator::{ALLOCATOR, print_alloc_stats},
    drivers::ahci::api::list_devices,
    fs::{
        FileSystem,
        fat32::Fat32fs,
        gpt::{Partition, parse_gpt, print_partitions},
        path::Path,
    },
    log,
    thread::{
        scheduler::sched,
        util::{kthread_exit, queue_spawn_kthread_named, queue_spawn_kthread_named_arg},
    },
};

use super::gpt::FilesystemType;

pub extern "C" fn fs_main_thread() -> ! {
    let logger = sched().get_logger();
    let devices = list_devices();

    let mut partitions = Vec::new();

    for device in &devices {
        match parse_gpt(device.id) {
            Ok(found_partitions) => {
                print_partitions(&found_partitions, &logger);
                partitions.extend(found_partitions);
            }
            Err(err) => log!(logger, "Error parsing GPT: {err}"),
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
    let logger = sched().get_logger();
    let partition = unsafe { Box::from_raw(partition) };

    log!(logger, "Partition: {}({})", partition.index, partition.name);

    let Ok(mut fs) = Fat32fs::new((*partition).clone()) else {
        log!(logger, "Failed to create fat32");
        kthread_exit(-1)
    };

    let bytes = fs.boot_info.bytes_per_sector;
    log!(logger, "FAT32 bytes per sector: {}", bytes);
    log!(
        logger,
        "FAT32 sectors per cluster: {}",
        fs.boot_info.sectors_per_cluster
    );

    let entries = fs.get_dir_entries(fs.boot_info.root_cluster).unwrap();

    log!(logger, "Showing root /");
    for entry in &entries {
        log!(logger, "Name: {}", entry.fat_name_to_string());
        log!(logger, "Is dir: {}", entry.is_directory());

        if entry.is_directory() {
            let entries = fs.get_dir_entries(entry.first_cluster()).unwrap();

            for entry in &entries {
                log!(logger, "Name: {}", entry.fat_name_to_string());
                log!(logger, "Is dir: {}", entry.is_directory());
            }
        } else {
            let content = fs.read_file(entry).unwrap();
            let x = core::str::from_utf8(&content);
            if let Ok(x) = x {
                log!(logger, "Content:\n{x:?}");
            }
        }
    }

    log!(logger, "Using the api");

    let fs = (&mut fs) as &mut dyn FileSystem;

    let files = fs.list_files(&Path::parse_str("/").unwrap()).unwrap();

    for file in files {
        log!(logger, "Name: {}", file.name);
        log!(
            logger,
            "Created: {:?}",
            file.created.map(|x| x.to_datetime())
        );
    }

    let path = Path::parse_str("/edgar.txt").unwrap();
    fs.create_file(&path).unwrap();
    print_alloc_stats();

    log!(logger, "created file");

    fs.write_bytes(&path, 0, c"hello written".to_bytes_with_nul())
        .unwrap();

    log!(logger, "wrote bytes");

    let content = fs.read_bytes(&path, 0, 512).unwrap();

    let content = CStr::from_bytes_with_nul(&content);

    log!(logger, "Content: {content:?}");

    print_alloc_stats();

    loop {
        sched().thread_park();
    }
}
