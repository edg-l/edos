//! Install the running live system onto a disk.
//!
//! Partitions the target, creates an ESP and an EFS root, copies the live root
//! across, and writes a Limine configuration naming the new root's partition
//! GUID. Every step logs what it is about to do before doing it, so a failed
//! install can be read off the serial log.

mod copy;
mod fat32;
mod gpt;
mod guid;
mod klog;

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::process;

use edos_lib::io::ioctl;

/// Mirrors `kernel/src/fs/devfs/block.rs`.
const BLOCK_IOCTL_FLUSH: u64 = 0x424B_0001;
const BLOCK_IOCTL_RESCAN: u64 = 0x424B_0003;
const BLOCK_IOCTL_IS_MOUNTED: u64 = 0x424B_0004;
const BLOCK_IOCTL_DEVICE_ID: u64 = 0x424B_0005;

const SECTOR: u64 = 512;
/// 1 MiB in, the alignment every partitioning tool uses.
const FIRST_PARTITION_LBA: u64 = 2048;
const DEFAULT_ESP_BYTES: u64 = 512 * 1024 * 1024;

/// Where the new root and ESP are mounted while the install runs.
const ROOT_MOUNT: &str = "/mnt/target";
const ESP_MOUNT: &str = "/mnt/esp";

/// Directories that are mount points or live-only state in the running system.
const SKIP_TOP_LEVEL: &[&str] = &["dev", "proc", "tmp", "mnt", "sys"];

fn usage() -> ! {
    eprintln!(
        "Usage: edos-install [OPTIONS] <device>

Arguments:
  <device>  Block device to install onto, e.g. /dev/sda or /dev/nvme0n1

Options:
  --esp-size <SIZE>  EFI System Partition size (default 512M)
  --yes              Do not ask for confirmation
  --help             Show this help
"
    );
    process::exit(1);
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()? {
        'G' | 'g' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        'M' | 'm' => (&s[..s.len() - 1], 1024 * 1024),
        'K' | 'k' => (&s[..s.len() - 1], 1024),
        _ => (s, 1),
    };
    num.parse::<u64>().ok().map(|v| v * mult)
}

fn fail(msg: &str) -> ! {
    eprintln!("edos-install: {msg}");
    process::exit(1);
}

fn device_ioctl(dev: &File, request: u64) -> i64 {
    use std::os::fd::AsRawFd;
    ioctl(dev.as_raw_fd() as u64, request, 0)
}

/// Mount a partition, creating the mount point first.
fn mount(device_id: u64, partition_idx: u64, at: &str, fs_type: &str) {
    let _ = std::fs::create_dir_all(at);
    let path = format!("{at}\0");
    let fs = format!("{fs_type}\0");
    let ret = unsafe {
        edos_lib::sys::syscall4(
            edos_lib::sys::SYS_MOUNT,
            device_id,
            partition_idx,
            path.as_ptr() as u64,
            fs.as_ptr() as u64,
        )
    } as i64;
    if ret < 0 {
        fail(&format!(
            "failed to mount device {device_id} partition {partition_idx} at {at}"
        ));
    }
}

/// Forced writeback passes that could not write every dirty page, from
/// `/proc/block_cache`. Zero when the file cannot be read: a missing counter
/// must not be the reason an install fails.
fn failed_sync_passes() -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/block_cache") else {
        return 0;
    };
    text.lines()
        .find_map(|l| l.strip_prefix("failed_sync_passes:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// The partition table as the kernel now sees it: (index, guid) for `device_id`.
fn partitions_of(device_id: u64) -> Vec<(u64, [u8; 16])> {
    let mut buf = vec![0u8; 4096];
    let ret = unsafe {
        edos_lib::sys::syscall2(
            edos_lib::sys::SYS_LIST_PARTITIONS,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        )
    } as i64;
    if ret < 0 {
        fail("failed to list partitions");
    }

    let len = ret as usize;
    let mut out = Vec::new();
    for base in (0..len).step_by(56).take(len / 56) {
        let index = u64::from_le_bytes(buf[base..base + 8].try_into().unwrap());
        let dev = u64::from_le_bytes(buf[base + 32..base + 40].try_into().unwrap());
        if dev != device_id {
            continue;
        }
        let mut g = [0u8; 16];
        g.copy_from_slice(&buf[base + 40..base + 56]);
        out.push((index, g));
    }
    out
}

/// Device id behind an open `/dev` node, asked of the node itself.
///
/// The name cannot be parsed back into an id: devfs derives the name from the
/// id, and that derivation is not invertible. `sd*` numbering continues from
/// the AHCI device count into USB storage, so a USB stick's letter says
/// nothing about its id, and NVMe encodes a controller and a namespace number
/// in a name whose id base is not in the name at all.
fn device_id_for(dev: &File, path: &str) -> u64 {
    let ret = device_ioctl(dev, BLOCK_IOCTL_DEVICE_ID);
    if ret < 0 {
        fail(&format!(
            "{path} did not answer BLOCK_IOCTL_DEVICE_ID ({ret}); it is not a block device node"
        ));
    }
    ret as u64
}

fn confirm(plan: &str) {
    println!("{plan}");
    print!("Type 'yes' to continue: ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() || answer.trim() != "yes" {
        println!("Aborted.");
        process::exit(1);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut device: Option<String> = None;
    let mut esp_bytes = DEFAULT_ESP_BYTES;
    let mut assume_yes = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => usage(),
            "--yes" | "-y" => assume_yes = true,
            "--esp-size" => {
                let val = args.next().unwrap_or_else(|| usage());
                esp_bytes = parse_size(&val).unwrap_or_else(|| fail("invalid --esp-size"));
            }
            s if s.starts_with('-') => usage(),
            _ => {
                if device.is_some() {
                    usage();
                }
                device = Some(arg);
            }
        }
    }

    let device_path = device.unwrap_or_else(|| usage());

    let mut dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&device_path)
        .unwrap_or_else(|e| fail(&format!("cannot open {device_path}: {e}")));

    let device_id = device_id_for(&dev, &device_path);

    if device_ioctl(&dev, BLOCK_IOCTL_IS_MOUNTED) == 1 {
        fail(&format!(
            "{device_path} backs a mounted filesystem; refusing to touch it"
        ));
    }

    let disk_bytes = dev
        .seek(SeekFrom::End(0))
        .unwrap_or_else(|e| fail(&format!("cannot size {device_path}: {e}")));
    let disk_sectors = disk_bytes / SECTOR;

    // Layout: ESP first so firmware finds it early, EFS root over the rest.
    let esp_sectors = esp_bytes / SECTOR;
    let esp_first = FIRST_PARTITION_LBA;
    let esp_last = esp_first + esp_sectors - 1;
    let root_first = esp_last + 1;
    let root_last = gpt::last_usable_lba(disk_sectors);

    if root_last <= root_first {
        fail("disk is too small for an ESP plus a root partition");
    }

    let root_guid = guid::random();
    let root_bytes = (root_last - root_first + 1) * SECTOR;

    if !assume_yes {
        confirm(&format!(
            "About to install onto {device_path} ({} MiB).\n\
             \n\
               ESP   LBA {esp_first}..{esp_last} ({} MiB, FAT32)\n\
               root  LBA {root_first}..{root_last} ({} MiB, EFS, GUID {})\n\
             \n\
             EVERYTHING ON {device_path} WILL BE DESTROYED.",
            disk_bytes / (1024 * 1024),
            (esp_last - esp_first + 1) * SECTOR / (1024 * 1024),
            root_bytes / (1024 * 1024),
            guid::format(&root_guid),
        ));
    }

    let started = std::time::Instant::now();
    let mut phase = started;
    let lap = |label: &str, phase: &mut std::time::Instant| {
        klog::trace(&format!("{label} in {:.1}s", phase.elapsed().as_secs_f32()));
        println!("  {label} in {:.1}s", phase.elapsed().as_secs_f32());
        *phase = std::time::Instant::now();
    };

    println!("Writing partition table...");
    gpt::write(
        &mut dev,
        disk_sectors,
        &[
            gpt::PartitionSpec {
                type_guid: guid::parse(guid::ESP_TYPE).unwrap(),
                unique_guid: guid::random(),
                first_lba: esp_first,
                last_lba: esp_last,
                name: "EFI System",
            },
            gpt::PartitionSpec {
                type_guid: guid::parse(guid::BASIC_DATA_TYPE).unwrap(),
                unique_guid: root_guid,
                first_lba: root_first,
                last_lba: root_last,
                name: "EDOS_DATA",
            },
        ],
    )
    .unwrap_or_else(|e| fail(&format!("failed to write GPT: {e}")));

    lap("partitioned", &mut phase);
    println!("Formatting the ESP (FAT32)...");
    let mut volume_id = [0u8; 4];
    edos_lib::getrandom(&mut volume_id);
    fat32::format(
        &mut dev,
        esp_first,
        (esp_last - esp_first + 1) as u32,
        u32::from_le_bytes(volume_id),
    )
    .unwrap_or_else(|e| fail(&format!("failed to format the ESP: {e}")));

    // Push everything to the disk before the formatter reopens the device.
    if device_ioctl(&dev, BLOCK_IOCTL_FLUSH) < 0 {
        fail("failed to flush the device");
    }
    drop(dev);

    lap("ESP formatted", &mut phase);
    println!("Formatting the root filesystem (EFS)...");
    efs_mkfs::format(&efs_mkfs::Format {
        target: Path::new(&device_path),
        partition_offset: root_first * SECTOR,
        partition_size: Some(root_bytes),
        block_size: 4096,
        label: Some("EDOS"),
        journal_size_mib: 16,
        populate: None,
    })
    .unwrap_or_else(|e| fail(&format!("failed to format the root filesystem: {e}")));

    let dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&device_path)
        .unwrap_or_else(|e| fail(&format!("cannot reopen {device_path}: {e}")));
    if device_ioctl(&dev, BLOCK_IOCTL_FLUSH) < 0 {
        fail("failed to flush the device");
    }

    lap("root formatted", &mut phase);
    println!("Re-reading the partition table...");
    let found = device_ioctl(&dev, BLOCK_IOCTL_RESCAN);
    if found < 0 {
        fail("failed to re-read the partition table");
    }
    if found != 2 {
        fail(&format!(
            "expected 2 partitions after partitioning, kernel found {found}"
        ));
    }

    // Mount by GUID rather than by index: the kernel's ordering is its own.
    let parts = partitions_of(device_id);
    let root_idx = parts
        .iter()
        .find(|(_, g)| *g == root_guid)
        .map(|(i, _)| *i)
        .unwrap_or_else(|| fail("the new root partition is not in the partition table"));
    let esp_idx = parts
        .iter()
        .find(|(i, _)| *i != root_idx)
        .map(|(i, _)| *i)
        .unwrap_or_else(|| fail("the new ESP is not in the partition table"));

    println!("Mounting the new filesystems...");
    mount(device_id, root_idx, ROOT_MOUNT, "efs");
    mount(device_id, esp_idx, ESP_MOUNT, "fat32");

    lap("mounted", &mut phase);
    println!("Copying the system...");
    let copied = copy::copy_root("/", ROOT_MOUNT, SKIP_TOP_LEVEL)
        .unwrap_or_else(|e| fail(&format!("failed to copy the system: {e}")));
    println!("  {copied} files");

    lap("system copied", &mut phase);
    println!("Installing the bootloader...");
    install_boot_files(&guid::format(&root_guid));

    lap("bootloader installed", &mut phase);
    // Sync first, flush second. `sync` is what puts the two new filesystems'
    // dirty pages on the wire; a device flush issued before it commits an
    // empty write cache and leaves everything `sync` then submits sitting in
    // the drive's own cache, which a prompt reboot loses.
    //
    // `SYS_SYNC` returns no error -- a forced writeback pass that could not
    // write every dirty page only logs and counts -- so the counter is read
    // either side of it. An install that reports success over a partial
    // flush is a disk that mounts and then does not boot, which costs far
    // more to diagnose than it costs to notice here.
    let passes_before = failed_sync_passes();
    unsafe { edos_lib::sys::syscall0(edos_lib::sys::SYS_SYNC) };
    if failed_sync_passes() > passes_before {
        fail("the kernel could not write every dirty page; the install is not durable");
    }
    if device_ioctl(&dev, BLOCK_IOCTL_FLUSH) < 0 {
        fail("failed to flush the device");
    }
    lap("flushed", &mut phase);
    println!("Total: {:.1}s", started.elapsed().as_secs_f32());

    println!(
        "\nDone. Remove the installation media and reboot.\n\
         The new system is still mounted at {ROOT_MOUNT} until then."
    );
}

/// Populate the ESP: the firmware entry point, the kernel, and a Limine
/// configuration naming the root partition we just created.
fn install_boot_files(root_guid: &str) {
    let efi_dir = format!("{ESP_MOUNT}/EFI/BOOT");
    let limine_dir = format!("{ESP_MOUNT}/boot/limine");
    for dir in [
        format!("{ESP_MOUNT}/EFI"),
        efi_dir.clone(),
        format!("{ESP_MOUNT}/boot"),
        limine_dir.clone(),
    ] {
        if let Err(e) = std::fs::create_dir(&dir) {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                fail(&format!("cannot create {dir}: {e}"));
            }
        }
    }

    for (from, to) in [
        ("/boot/BOOTX64.EFI", format!("{efi_dir}/BOOTX64.EFI")),
        ("/boot/kernel", format!("{ESP_MOUNT}/boot/kernel")),
    ] {
        copy::copy_file(Path::new(from), Path::new(&to))
            .unwrap_or_else(|e| fail(&format!("cannot copy {from}: {e}")));
        println!("  {to}");
    }

    let config = format!(
        "timeout: 1\n\
         \n\
         /edos\n\
         \x20   protocol: limine\n\
         \x20   resolution: 1920x1080\n\
         \x20   kernel_path: boot():/boot/kernel\n\
         \x20   cmdline: root=UUID={root_guid} rootfstype=efs\n"
    );
    let path = format!("{limine_dir}/limine.conf");
    std::fs::write(&path, config).unwrap_or_else(|e| fail(&format!("cannot write {path}: {e}")));
    println!("  {path}");
}
