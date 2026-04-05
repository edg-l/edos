use alloc::{boxed::Box, sync::Arc};
use core::{
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    time::Duration,
};

use crate::{
    println,
    thread::{
        scheduler::{WakePriority, sched},
        thread::ThreadId,
        util::queue_spawn_kthread_named_arg,
    },
    timer::Instant,
};

/// Exit QEMU via the isa-debug-exit device (port 0xf4).
/// The exit code seen by the host is `(code << 1) | 1`, so
/// writing 0x00 produces exit code 1 (success), 0x01 produces 3 (failure).
fn qemu_exit(code: u32) -> ! {
    unsafe {
        core::arch::asm!("out dx, al", in("dx") 0xf4u16, in("al") code as u8);
    }
    loop {
        x86_64::instructions::hlt();
    }
}

struct TestHarness {
    done_count: AtomicU32,
    total: u32,
    // park/wake test
    wake_target_tid: AtomicU64,
    // park-while test
    park_while_tid: AtomicU64,
    park_while_condition: AtomicBool,
}

pub fn run_sched_tests() {
    println!("[sched-test] Starting scheduler tests...");
    let harness = Arc::new(TestHarness {
        done_count: AtomicU32::new(0),
        total: 8,
        wake_target_tid: AtomicU64::new(0),
        park_while_tid: AtomicU64::new(0),
        park_while_condition: AtomicBool::new(false),
    });

    spawn_test(&harness, "test-park-a", test_park_a);
    spawn_test(&harness, "test-park-b", test_park_b);
    spawn_test(&harness, "test-sleep", test_sleep);
    spawn_test(&harness, "test-yield", test_yield_stress);
    spawn_test(&harness, "test-pw-abort", test_park_while_abort);
    spawn_test(&harness, "test-pw-c", test_park_while_c);
    spawn_test(&harness, "test-pw-d", test_park_while_d);
    spawn_test(&harness, "test-ctxsaved", test_context_saved);

    // Coordinator thread: waits for all tests, then exits QEMU.
    let boxed = Box::into_raw(Box::new(harness.clone())) as *mut u8;
    queue_spawn_kthread_named_arg(
        "test-coordinator",
        test_coordinator as *const () as u64,
        boxed,
    );
}

fn spawn_test(harness: &Arc<TestHarness>, name: &str, entry: extern "C" fn(*mut u8) -> !) {
    let boxed = Box::into_raw(Box::new(harness.clone())) as *mut u8;
    queue_spawn_kthread_named_arg(name, entry as *const () as u64, boxed);
}

fn get_harness(arg: *mut u8) -> Arc<TestHarness> {
    unsafe { *Box::from_raw(arg as *mut Arc<TestHarness>) }
}

fn test_done(harness: &TestHarness, name: &str) {
    let count = harness.done_count.fetch_add(1, Ordering::AcqRel) + 1;
    println!("[sched-test] PASS: {} ({}/{})", name, count, harness.total);
    if count == harness.total {
        println!("[sched-test] ALL {} TESTS PASSED", harness.total);
    }
}

extern "C" fn test_park_a(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    let tid = sched().current_thread_id().unwrap();
    h.wake_target_tid.store(tid.0, Ordering::Release);
    sched().thread_park();
    // Resumed - we were woken by B
    test_done(&h, "park-wake-a");
    sched().thread_exit(0);
}

extern "C" fn test_park_b(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    // Wait for A to store its TID
    while h.wake_target_tid.load(Ordering::Acquire) == 0 {
        sched().thread_yield();
    }
    // Give A time to actually park
    sched().thread_sleep(Duration::from_millis(10));
    let tid = ThreadId(h.wake_target_tid.load(Ordering::Acquire));
    sched().wake_thread(tid, WakePriority::Normal);
    test_done(&h, "park-wake-b");
    sched().thread_exit(0);
}

extern "C" fn test_sleep(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    let start = Instant::now();
    sched().thread_sleep(Duration::from_millis(50));
    let elapsed = Instant::now().duration_since(start);
    assert!(
        elapsed.as_millis() >= 45,
        "[sched-test] sleep too short: {}ms",
        elapsed.as_millis()
    );
    test_done(&h, "sleep");
    sched().thread_exit(0);
}

extern "C" fn test_yield_stress(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    for _ in 0..1000 {
        sched().thread_yield();
    }
    test_done(&h, "yield-stress");
    sched().thread_exit(0);
}

extern "C" fn test_park_while_abort(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    // Condition is already false, thread_park_while should return without switching
    sched().thread_park_while(|| false);
    test_done(&h, "park-while-abort");
    sched().thread_exit(0);
}

extern "C" fn test_park_while_c(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    let tid = sched().current_thread_id().unwrap();
    h.park_while_tid.store(tid.0, Ordering::Release);
    h.park_while_condition.store(true, Ordering::Release);
    sched().thread_park_while(|| h.park_while_condition.load(Ordering::Acquire));
    // Resumed - condition should now be false
    assert!(
        !h.park_while_condition.load(Ordering::Acquire),
        "[sched-test] park-while condition still true after wake"
    );
    test_done(&h, "park-while-c");
    sched().thread_exit(0);
}

extern "C" fn test_park_while_d(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    while h.park_while_tid.load(Ordering::Acquire) == 0 {
        sched().thread_yield();
    }
    sched().thread_sleep(Duration::from_millis(10));
    h.park_while_condition.store(false, Ordering::Release);
    let tid = ThreadId(h.park_while_tid.load(Ordering::Acquire));
    sched().wake_thread(tid, WakePriority::Normal);
    test_done(&h, "park-while-d");
    sched().thread_exit(0);
}

extern "C" fn test_context_saved(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    sched().thread_yield();
    // After resuming from yield, context_saved should be false
    // (cleared by context_switch_to when we were scheduled back)
    let cur = sched().current_thread().unwrap();
    assert!(
        !cur.context_saved.load(Ordering::Acquire),
        "[sched-test] context_saved should be false for running thread"
    );
    test_done(&h, "context-saved");
    sched().thread_exit(0);
}

const TIMEOUT_SECS: u64 = 10;

extern "C" fn test_coordinator(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    let start = Instant::now();
    loop {
        let done = h.done_count.load(Ordering::Acquire);
        if done >= h.total {
            println!("[sched-test] ALL {} TESTS PASSED", h.total);
            qemu_exit(0); // success (host sees exit code 1)
        }
        if start.elapsed().as_secs() >= TIMEOUT_SECS {
            println!(
                "[sched-test] TIMEOUT: {}/{} tests passed after {}s",
                done, h.total, TIMEOUT_SECS
            );
            qemu_exit(1); // failure (host sees exit code 3)
        }
        sched().thread_sleep(Duration::from_millis(100));
    }
}
