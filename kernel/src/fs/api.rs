use x86_64::instructions::hlt;

use crate::{drivers::ahci::api::list_devices, fs::gpt::{parse_gpt, print_partitions}, println};




pub fn fs_main_thread() -> ! {

    let devices = list_devices();

    for device in &devices {
        match parse_gpt(device.id) {
            Ok(partitions) => {

                print_partitions(&partitions);
            },
            Err(err) => println!("Error parsing GPT: {err}"),
        }
    }

    loop {
        hlt();
    }
}
