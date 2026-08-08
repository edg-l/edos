//! Growth check for the userspace allocator: a `Vec` grown by repeated
//! `extend_from_slice` must still hold everything written into it.

const PATTERN: u8 = 0xc7;
const STEP: usize = 4096;
const LIMIT: usize = 2 << 20;

fn main() {
    let chunk = [PATTERN; STEP];
    let mut v: Vec<u8> = Vec::new();
    let mut failures = 0;

    while v.len() < LIMIT {
        let before = v.len();
        let cap_before = v.capacity();
        v.extend_from_slice(&chunk);

        if let Some(off) = v.iter().position(|&b| b != PATTERN) {
            failures += 1;
            println!(
                "lost data: len {} -> {}, cap {} -> {}, first bad offset {} = {:#04x}",
                before,
                v.len(),
                cap_before,
                v.capacity(),
                off,
                v[off]
            );
            if failures >= 4 {
                break;
            }
        }
    }

    if failures == 0 {
        println!("vectest: ok, {} bytes intact", v.len());
    } else {
        println!("vectest: {} growth steps lost data", failures);
        std::process::exit(1);
    }
}
