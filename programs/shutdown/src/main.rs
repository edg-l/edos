use edos_lib::process::{REBOOT_HALT, REBOOT_POWER_OFF, REBOOT_RESTART, reboot};

fn usage() -> ! {
    eprintln!("usage: shutdown [-r | -H]");
    eprintln!("  (no option)  power the machine off");
    eprintln!("  -r           reboot");
    eprintln!("  -H           halt without powering off");
    std::process::exit(2);
}

fn main() {
    let mut cmd = REBOOT_POWER_OFF;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-r" | "--reboot" => cmd = REBOOT_RESTART,
            "-H" | "--halt" => cmd = REBOOT_HALT,
            "-h" | "--help" => usage(),
            _ => {
                eprintln!("shutdown: unknown option '{arg}'");
                usage();
            }
        }
    }

    // The kernel syncs the filesystems, so nothing to flush here. A return
    // means the machine refused to stop.
    reboot(cmd);
    eprintln!("shutdown: the machine did not stop");
    std::process::exit(1);
}
