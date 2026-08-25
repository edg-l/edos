//! What `fork` owes the child: the address space, and the registers.
//!
//! The kernel's SYSCALL stub restores `rdi`, `rsi`, `rdx`, `r8`, `r9`, `r10`
//! and the callee-saved set before `sysretq`, so a program may keep a live
//! value in any of them across a `syscall` -- and the raw syscall stubs
//! declare only `rax`, `rcx` and `r11` as clobbered, which is what tells the
//! compiler it may. A child returns from the same instruction as its parent,
//! so it must see the same registers; anything the kernel drops there becomes
//! a null pointer or a garbage length in whichever function called `fork`,
//! with nothing at the call site to show for it.

use core::arch::asm;

use edos_lib::{process, sys};

/// Tagged so a mismatch names the register that lost its value rather than
/// just reporting a zero.
const MAGIC: u64 = 0x5EED_0000_0000;

/// The registers `syscall0` leaves live across the instruction, in the order
/// [`fork_carrying_registers`] loads them.
const NAMES: [&str; 10] = [
    "rdi", "rsi", "rdx", "r8", "r9", "r10", "r12", "r13", "r14", "r15",
];

/// Fork with a known value in every register the syscall convention preserves,
/// and read them all back on the far side.
///
/// Returns the fork result and what the registers held when it came back, for
/// whichever of the two processes is asking.
fn fork_carrying_registers() -> (i64, [u64; NAMES.len()]) {
    let ret: u64;
    let rdi: u64;
    let rsi: u64;
    let rdx: u64;
    let r8: u64;
    let r9: u64;
    let r10: u64;
    let r12: u64;
    let r13: u64;
    let r14: u64;
    let r15: u64;

    unsafe {
        asm!(
            "syscall",
            inout("rax") sys::SYS_FORK => ret,
            inout("rdi") MAGIC | 1 => rdi,
            inout("rsi") MAGIC | 2 => rsi,
            inout("rdx") MAGIC | 3 => rdx,
            inout("r8") MAGIC | 4 => r8,
            inout("r9") MAGIC | 5 => r9,
            inout("r10") MAGIC | 6 => r10,
            inout("r12") MAGIC | 7 => r12,
            inout("r13") MAGIC | 8 => r13,
            inout("r14") MAGIC | 9 => r14,
            inout("r15") MAGIC | 10 => r15,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }

    (ret as i64, [rdi, rsi, rdx, r8, r9, r10, r12, r13, r14, r15])
}

/// Name every register that came back wrong, and answer how many did.
fn wrong_registers(who: &str, regs: &[u64; NAMES.len()]) -> usize {
    let mut bad = 0;
    for (i, name) in NAMES.iter().enumerate() {
        let want = MAGIC | (i as u64 + 1);
        if regs[i] != want {
            println!(
                "forktest: {who} came back with {name}={:#x}, want {want:#x}",
                regs[i]
            );
            bad += 1;
        }
    }
    bad
}

/// Both sides of one fork see every preserved register they went in with.
fn test1() {
    let (pid, regs) = fork_carrying_registers();
    if pid < 0 {
        fail(1, &format!("fork failed: {pid}"));
    }

    if pid == 0 {
        std::process::exit(if wrong_registers("the child", &regs) == 0 {
            0
        } else {
            1
        });
    }

    let bad = wrong_registers("the parent", &regs);
    let code = process::waitpid(pid as u64);

    if bad != 0 {
        fail(1, "the syscall stub did not restore the caller's registers");
    }
    if code != 0 {
        fail(
            1,
            "the child returned from fork with registers its parent still had",
        );
    }
    pass(1, "every preserved register survives fork on both sides");
}

/// A write in the child stays in the child: the stack value and the heap
/// allocation the parent still holds are unchanged when it returns.
fn test2() {
    let x = 42u64;
    let v: Vec<u64> = (0..10).collect();

    let pid = process::fork().unwrap_or_else(|e| fail(2, &format!("fork failed: {e:?}")));

    if pid == 0 {
        let mut mine = x;
        let mut heap = v;
        mine = mine.wrapping_add(57);
        heap.push(99);
        std::process::exit(if mine == 99 && heap.len() == 11 && heap[10] == 99 {
            0
        } else {
            1
        });
    }

    let code = process::waitpid(pid);
    if code != 0 {
        fail(2, "the child could not write to its own copy");
    }
    if x != 42 {
        fail(2, &format!("the parent's stack value reads {x}, want 42"));
    }
    if v.len() != 10 || v.iter().sum::<u64>() != 45 {
        fail(2, "the parent's heap allocation changed under the child");
    }
    pass(2, "a child's writes leave the parent's copy alone");
}

/// A child may fork in turn, and each generation's exit code reaches the one
/// that waited for it.
fn test3() {
    let pid = process::fork().unwrap_or_else(|e| fail(3, &format!("fork failed: {e:?}")));

    if pid == 0 {
        let Ok(grandchild) = process::fork() else {
            std::process::exit(1);
        };
        if grandchild == 0 {
            std::process::exit(7);
        }
        let code = process::waitpid(grandchild);
        std::process::exit(if code == 7 { 0 } else { 1 });
    }

    let code = process::waitpid(pid);
    if code != 0 {
        fail(3, "a grandchild's exit code did not reach the child");
    }
    pass(3, "fork nests and exit codes propagate");
}

fn pass(n: u32, what: &str) {
    println!("PASS test {n}: {what}");
}

fn fail(n: u32, why: &str) -> ! {
    println!("FAIL test {n}: {why}");
    std::process::exit(1);
}

fn main() {
    println!("forktest: running, pid={}", process::getpid());
    test1();
    test2();
    test3();
    println!("forktest: all tests passed");
}
