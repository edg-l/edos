//! Does a thread's SSE state survive a context switch?
//!
//! Nothing else in the tree asks. `sched-test`'s compute-across-yields case
//! checks the general-purpose registers and the stack across a switch with
//! integer arithmetic, and the boot-time MMX check runs once before there is
//! anything to switch to — so the 512-byte `FXSAVE` area a user thread carries
//! was covered by nothing.
//!
//! It needs covering because the kernel skips the reload when the CPU's
//! registers already hold the incoming thread's state, and the cost of getting
//! that wrong is not a crash: a thread resumes with another thread's `XMM`
//! registers and computes quietly wrong answers.
//!
//! The load, the yield and the read-back are one `asm!` block per round, so
//! there is nowhere between them for the compiler to use an `XMM` register
//! itself and hide the failure. Each thread seeds its lanes from its own index,
//! so a lost restore shows up as another thread's pattern rather than as noise.

use std::env;
use std::thread;

/// `xmm0`-`xmm7`, two `u64` lanes each. Eight is enough to catch a wrong
/// restore, and keeps the block readable.
const REGS: usize = 8;
const LANES: usize = REGS * 2;

const DEFAULT_ROUNDS: usize = 20_000;
const DEFAULT_THREADS: usize = 4;

/// Fill `xmm0`-`xmm7` from `input`, yield, and read them back into `output`.
///
/// # Safety
/// Both buffers must hold [`LANES`] `u64`s. The syscall clobbers `rcx` and
/// `r11`, which are declared.
unsafe fn yield_through_xmm(input: &[u64; LANES], output: &mut [u64; LANES]) {
    unsafe {
        core::arch::asm!(
            "movups xmm0, [{i} + 0]",
            "movups xmm1, [{i} + 16]",
            "movups xmm2, [{i} + 32]",
            "movups xmm3, [{i} + 48]",
            "movups xmm4, [{i} + 64]",
            "movups xmm5, [{i} + 80]",
            "movups xmm6, [{i} + 96]",
            "movups xmm7, [{i} + 112]",
            "syscall",
            "movups [{o} + 0], xmm0",
            "movups [{o} + 16], xmm1",
            "movups [{o} + 32], xmm2",
            "movups [{o} + 48], xmm3",
            "movups [{o} + 64], xmm4",
            "movups [{o} + 80], xmm5",
            "movups [{o} + 96], xmm6",
            "movups [{o} + 112], xmm7",
            i = in(reg) input.as_ptr(),
            o = in(reg) output.as_mut_ptr(),
            inlateout("rax") SYS_SCHED_YIELD => _,
            out("rcx") _,
            out("r11") _,
            out("xmm0") _,
            out("xmm1") _,
            out("xmm2") _,
            out("xmm3") _,
            out("xmm4") _,
            out("xmm5") _,
            out("xmm6") _,
            out("xmm7") _,
            options(nostack),
        );
    }
}

const SYS_SCHED_YIELD: u64 = 282;

fn pattern(seed: u64) -> [u64; LANES] {
    let mut lanes = [0u64; LANES];
    for (i, lane) in lanes.iter_mut().enumerate() {
        // Distinct per thread and per lane, and nothing a zeroed or
        // default-initialised area could produce by accident.
        *lane = seed
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(0x0101_0101_0101_0101u64.wrapping_mul(i as u64 + 1));
    }
    lanes
}

/// Returns the round at which the registers came back wrong, if any.
fn run(seed: u64, rounds: usize) -> Option<(usize, [u64; LANES], [u64; LANES])> {
    let expected = pattern(seed);
    for round in 0..rounds {
        let mut seen = [0u64; LANES];
        unsafe { yield_through_xmm(&expected, &mut seen) };
        if seen != expected {
            return Some((round, expected, seen));
        }
    }
    None
}

fn main() {
    let mut args = env::args().skip(1);
    let threads: usize = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(DEFAULT_THREADS);
    let rounds: usize = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(DEFAULT_ROUNDS);

    println!("fputest: {threads} threads, {rounds} yields each through xmm0-xmm7");

    let handles: Vec<_> = (0..threads)
        .map(|i| thread::spawn(move || run(i as u64 + 1, rounds)))
        .collect();

    let mut failures = 0;
    for (i, handle) in handles.into_iter().enumerate() {
        match handle.join() {
            Ok(None) => {}
            Ok(Some((round, expected, seen))) => {
                failures += 1;
                println!("fputest: thread {i} lost its SSE state at round {round}");
                println!("  expected {:#018x} ...", expected[0]);
                println!("  got      {:#018x} ...", seen[0]);
                // Whose state came back instead says which way the accounting
                // went wrong, so name it when it is another thread's.
                for other in 0..threads {
                    if seen == pattern(other as u64 + 1) {
                        println!("  that is thread {other}'s pattern");
                    }
                }
            }
            Err(_) => {
                failures += 1;
                println!("fputest: thread {i} panicked");
            }
        }
    }

    if failures == 0 {
        println!("fputest: all threads kept their SSE state across every switch");
    } else {
        println!("fputest: {failures} of {threads} threads lost SSE state");
        std::process::exit(1);
    }
}
