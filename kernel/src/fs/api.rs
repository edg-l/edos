use x86_64::instructions::hlt;

use crate::drivers::ahci::api::list_devices;




pub fn fs_main_thread() -> ! {

    let devices = list_devices();

    loop {
        hlt();
    }
}
