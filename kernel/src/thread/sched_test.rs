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
    // ping-pong test
    ping_tid: AtomicU64,
    pong_tid: AtomicU64,
    ping_pong_count: AtomicU32,
    // spawn-exit stress
    spawn_exit_done: AtomicU32,
    // multi-wake
    multi_wake_tids: [AtomicU64; 4],
    multi_wake_gate: AtomicBool,
    // abort-race
    abort_race_tid: AtomicU64,
    abort_race_condition: AtomicBool,
    abort_race_rounds: AtomicU32,
    // wake-before-park
    wbp_tid: AtomicU64,
    wbp_parked: AtomicBool,
    // sleep-interrupt
    sleep_int_tid: AtomicU64,
}

const TOTAL_TESTS: u32 = 30;

pub fn run_sched_tests() {
    println!("[sched-test] Starting scheduler tests ({TOTAL_TESTS} expected)...");
    let harness = Arc::new(TestHarness {
        done_count: AtomicU32::new(0),
        total: TOTAL_TESTS,
        wake_target_tid: AtomicU64::new(0),
        park_while_tid: AtomicU64::new(0),
        park_while_condition: AtomicBool::new(false),
        ping_tid: AtomicU64::new(0),
        pong_tid: AtomicU64::new(0),
        ping_pong_count: AtomicU32::new(0),
        spawn_exit_done: AtomicU32::new(0),
        multi_wake_tids: [
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
        ],
        multi_wake_gate: AtomicBool::new(false),
        abort_race_tid: AtomicU64::new(0),
        abort_race_condition: AtomicBool::new(false),
        abort_race_rounds: AtomicU32::new(0),
        wbp_tid: AtomicU64::new(0),
        wbp_parked: AtomicBool::new(false),
        sleep_int_tid: AtomicU64::new(0),
    });

    // --- Basic tests (8) ---
    spawn_test(&harness, "test-park-a", test_park_a);
    spawn_test(&harness, "test-park-b", test_park_b);
    spawn_test(&harness, "test-sleep", test_sleep);
    spawn_test(&harness, "test-yield", test_yield_stress);
    spawn_test(&harness, "test-pw-abort", test_park_while_abort);
    spawn_test(&harness, "test-pw-c", test_park_while_c);
    spawn_test(&harness, "test-pw-d", test_park_while_d);
    spawn_test(&harness, "test-ctxsaved", test_context_saved);

    // --- Stress tests ---
    // Rapid park/wake ping-pong between two threads (2 completions)
    spawn_test(&harness, "test-ping", test_ping);
    spawn_test(&harness, "test-pong", test_pong);
    // Spawn+exit storm: spawn 50 threads that immediately exit (1 completion)
    spawn_test(&harness, "test-spawn-exit", test_spawn_exit);
    // Multi-wake: 4 threads park, 1 waker wakes all at once (5 completions)
    for _ in 0..4 {
        spawn_test(&harness, "test-mw-sleeper", test_multi_wake_sleeper);
    }
    spawn_test(&harness, "test-mw-waker", test_multi_wake_waker);
    // Abort-race: waker fires immediately to hit the park_while abort path (2)
    spawn_test(&harness, "test-abort-parker", test_abort_race_parker);
    spawn_test(&harness, "test-abort-waker", test_abort_race_waker);
    // Wake-before-park: waker fires before thread parks (2)
    spawn_test(&harness, "test-wbp-parker", test_wbp_parker);
    spawn_test(&harness, "test-wbp-waker", test_wbp_waker);
    // Sleep interrupted by early wake (2)
    spawn_test(&harness, "test-sleep-int-s", test_sleep_interrupt_sleeper);
    spawn_test(&harness, "test-sleep-int-w", test_sleep_interrupt_waker);
    // Compute-across-yield: verify register/stack preservation (8)
    for i in 0..8u64 {
        let boxed = Box::into_raw(Box::new((harness.clone(), i))) as *mut u8;
        queue_spawn_kthread_named_arg(
            "test-compute",
            test_compute_across_yields as *const () as u64,
            boxed,
        );
    }

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

// ---------------------------------------------------------------------------
// Stress: rapid park/wake ping-pong (tests waker-CPU enqueue under contention)
// ---------------------------------------------------------------------------

const PING_PONG_ROUNDS: u32 = 500;

extern "C" fn test_ping(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    let tid = sched().current_thread_id().unwrap();
    h.ping_tid.store(tid.0, Ordering::Release);

    // Wait for pong to register
    while h.pong_tid.load(Ordering::Acquire) == 0 {
        sched().thread_yield();
    }
    let pong = ThreadId(h.pong_tid.load(Ordering::Acquire));

    for _ in 0..PING_PONG_ROUNDS {
        sched().thread_park();
        // Woken by pong. Wake pong back.
        sched().wake_thread(pong, WakePriority::Normal);
    }
    // Final park - pong will wake us one last time
    sched().thread_park();

    let count = h.ping_pong_count.load(Ordering::Acquire);
    assert!(
        count == PING_PONG_ROUNDS,
        "[sched-test] ping-pong count mismatch: {count} != {PING_PONG_ROUNDS}"
    );
    test_done(&h, "ping-pong-ping");
    sched().thread_exit(0);
}

extern "C" fn test_pong(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    let tid = sched().current_thread_id().unwrap();
    h.pong_tid.store(tid.0, Ordering::Release);

    // Wait for ping to register
    while h.ping_tid.load(Ordering::Acquire) == 0 {
        sched().thread_yield();
    }
    let ping = ThreadId(h.ping_tid.load(Ordering::Acquire));

    // Give ping time to enter its first park
    sched().thread_sleep(Duration::from_millis(5));

    for _ in 0..PING_PONG_ROUNDS {
        // Wake ping
        sched().wake_thread(ping, WakePriority::Normal);
        h.ping_pong_count.fetch_add(1, Ordering::AcqRel);
        // Park, wait for ping to wake us back
        sched().thread_park();
    }
    // Wake ping one final time so it can finish
    sched().wake_thread(ping, WakePriority::Normal);

    test_done(&h, "ping-pong-pong");
    sched().thread_exit(0);
}

// ---------------------------------------------------------------------------
// Stress: spawn 50 threads that immediately exit (tests reaper + thread_exit)
// ---------------------------------------------------------------------------

const SPAWN_EXIT_COUNT: u32 = 50;

extern "C" fn test_spawn_exit(arg: *mut u8) -> ! {
    let h = get_harness(arg);

    for _ in 0..SPAWN_EXIT_COUNT {
        let boxed = Box::into_raw(Box::new(h.clone())) as *mut u8;
        queue_spawn_kthread_named_arg(
            "test-ephemeral",
            test_ephemeral_thread as *const () as u64,
            boxed,
        );
    }

    // Wait for all ephemeral threads to finish
    let start = Instant::now();
    loop {
        let done = h.spawn_exit_done.load(Ordering::Acquire);
        if done >= SPAWN_EXIT_COUNT {
            break;
        }
        assert!(
            start.elapsed().as_secs() < 5,
            "[sched-test] spawn-exit timeout: {done}/{SPAWN_EXIT_COUNT} done"
        );
        sched().thread_sleep(Duration::from_millis(10));
    }

    test_done(&h, "spawn-exit");
    sched().thread_exit(0);
}

extern "C" fn test_ephemeral_thread(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    // Do a yield to exercise the scheduler, then exit
    sched().thread_yield();
    h.spawn_exit_done.fetch_add(1, Ordering::AcqRel);
    sched().thread_exit(0);
}

// ---------------------------------------------------------------------------
// Stress: 4 threads park, 1 waker wakes all at once (tests multi-wake)
// ---------------------------------------------------------------------------

extern "C" fn test_multi_wake_sleeper(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    let tid = sched().current_thread_id().unwrap();

    // Register in the first free slot
    for slot in &h.multi_wake_tids {
        if slot
            .compare_exchange(0, tid.0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            break;
        }
    }

    // Park until the waker opens the gate
    sched().thread_park_while(|| !h.multi_wake_gate.load(Ordering::Acquire));

    assert!(
        h.multi_wake_gate.load(Ordering::Acquire),
        "[sched-test] multi-wake gate not open after resume"
    );
    test_done(&h, "multi-wake-sleeper");
    sched().thread_exit(0);
}

extern "C" fn test_multi_wake_waker(arg: *mut u8) -> ! {
    let h = get_harness(arg);

    // Wait for all 4 sleepers to register
    loop {
        let registered = h
            .multi_wake_tids
            .iter()
            .filter(|t| t.load(Ordering::Acquire) != 0)
            .count();
        if registered == 4 {
            break;
        }
        sched().thread_yield();
    }

    // Give sleepers time to actually park
    sched().thread_sleep(Duration::from_millis(20));

    // Open the gate and wake all 4
    h.multi_wake_gate.store(true, Ordering::Release);
    for slot in &h.multi_wake_tids {
        let tid = slot.load(Ordering::Acquire);
        if tid != 0 {
            sched().wake_thread(ThreadId(tid), WakePriority::Normal);
        }
    }

    test_done(&h, "multi-wake-waker");
    sched().thread_exit(0);
}

// ---------------------------------------------------------------------------
// Stress: park_while abort path race
// The waker fires with NO delay to maximize the chance of hitting the
// window between CAS Running->Parked and the condition check inside
// transition_park_while. When the waker wins the race, the parker's
// CAS Parked->Running fails and it must switch away (abort path).
// Repeated 200 times to increase the probability.
// ---------------------------------------------------------------------------

const ABORT_RACE_ROUNDS: u32 = 200;

extern "C" fn test_abort_race_parker(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    let tid = sched().current_thread_id().unwrap();
    h.abort_race_tid.store(tid.0, Ordering::Release);

    for _ in 0..ABORT_RACE_ROUNDS {
        h.abort_race_condition.store(true, Ordering::Release);
        // Park while condition is true. The waker will set it to false
        // and wake us. If the waker is fast enough, it wakes us before
        // we even check the condition, triggering the abort path.
        sched().thread_park_while(|| h.abort_race_condition.load(Ordering::Acquire));
    }

    let rounds = h.abort_race_rounds.load(Ordering::Acquire);
    assert!(
        rounds == ABORT_RACE_ROUNDS,
        "[sched-test] abort-race round mismatch: {rounds} != {ABORT_RACE_ROUNDS}"
    );
    test_done(&h, "abort-race-parker");
    sched().thread_exit(0);
}

extern "C" fn test_abort_race_waker(arg: *mut u8) -> ! {
    let h = get_harness(arg);

    // Wait for parker to register
    while h.abort_race_tid.load(Ordering::Acquire) == 0 {
        sched().thread_yield();
    }
    let parker = ThreadId(h.abort_race_tid.load(Ordering::Acquire));

    for _ in 0..ABORT_RACE_ROUNDS {
        // Spin until condition is set (parker is about to park or has parked)
        while !h.abort_race_condition.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        // Immediately flip condition and wake -- NO sleep, race the parker
        h.abort_race_condition.store(false, Ordering::Release);
        sched().wake_thread(parker, WakePriority::Normal);
        h.abort_race_rounds.fetch_add(1, Ordering::AcqRel);
        // Yield to give parker a chance to run its next iteration
        sched().thread_yield();
    }

    test_done(&h, "abort-race-waker");
    sched().thread_exit(0);
}

// ---------------------------------------------------------------------------
// Stress: wake-before-park
// The waker calls wake_thread while the target is still Running (hasn't
// parked yet). wake_thread_slow must handle this by spinning/retrying.
// ---------------------------------------------------------------------------

extern "C" fn test_wbp_parker(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    let tid = sched().current_thread_id().unwrap();
    h.wbp_tid.store(tid.0, Ordering::Release);

    // Signal that we're about to park, then park.
    // The waker will wake us BEFORE we actually enter park.
    h.wbp_parked.store(true, Ordering::Release);
    sched().thread_park();

    // If we get here, the wake eventually succeeded
    test_done(&h, "wake-before-park-parker");
    sched().thread_exit(0);
}

extern "C" fn test_wbp_waker(arg: *mut u8) -> ! {
    let h = get_harness(arg);

    // Wait for parker TID
    while h.wbp_tid.load(Ordering::Acquire) == 0 {
        sched().thread_yield();
    }
    let parker = ThreadId(h.wbp_tid.load(Ordering::Acquire));

    // Wait for the "about to park" signal, then wake IMMEDIATELY.
    // The parker might not have called thread_park yet -- wake_thread_slow
    // must handle the Running state by retrying.
    while !h.wbp_parked.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    // No sleep! Wake immediately while parker might still be Running.
    sched().wake_thread(parker, WakePriority::Normal);

    test_done(&h, "wake-before-park-waker");
    sched().thread_exit(0);
}

// ---------------------------------------------------------------------------
// Stress: sleep interrupted by early wake
// Thread sleeps for 10 seconds, waker wakes it after 5ms.
// Tests the Sleeping -> Waking transition and early wakeup path.
// ---------------------------------------------------------------------------

extern "C" fn test_sleep_interrupt_sleeper(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    let tid = sched().current_thread_id().unwrap();
    h.sleep_int_tid.store(tid.0, Ordering::Release);

    let start = Instant::now();
    // Sleep for 10 seconds (will be woken early)
    sched().thread_sleep(Duration::from_secs(10));
    let elapsed = start.elapsed();

    // Should have been woken well before 10 seconds
    assert!(
        elapsed.as_millis() < 1000,
        "[sched-test] sleep-interrupt: slept too long ({}ms), wake failed?",
        elapsed.as_millis()
    );
    test_done(&h, "sleep-interrupt-sleeper");
    sched().thread_exit(0);
}

extern "C" fn test_sleep_interrupt_waker(arg: *mut u8) -> ! {
    let h = get_harness(arg);

    while h.sleep_int_tid.load(Ordering::Acquire) == 0 {
        sched().thread_yield();
    }
    let sleeper = ThreadId(h.sleep_int_tid.load(Ordering::Acquire));

    // Give sleeper time to enter sleep
    sched().thread_sleep(Duration::from_millis(5));
    // Wake the sleeping thread early
    sched().wake_thread(sleeper, WakePriority::Normal);

    test_done(&h, "sleep-interrupt-waker");
    sched().thread_exit(0);
}

// ---------------------------------------------------------------------------
// Compute-across-yields: verify register and stack integrity
//
// Each thread computes a known-answer checksum across 500 yields. If a
// context switch corrupts any register or stack slot, the final checksum
// will be wrong. Uses multiple local variables to pressure the compiler
// into using callee-saved registers (rbx, r12-r15) and stack spills.
// 8 instances run concurrently to stress cross-CPU migration.
// ---------------------------------------------------------------------------

/// Simple non-inlineable hash step so the compiler can't constant-fold it away.
#[inline(never)]
fn hash_step(state: u64, input: u64) -> u64 {
    state.wrapping_mul(6364136223846793005).wrapping_add(input)
}

extern "C" fn test_compute_across_yields(arg: *mut u8) -> ! {
    let (h, seed) = unsafe { *Box::from_raw(arg as *mut (Arc<TestHarness>, u64)) };

    // Use many locals to force register pressure + stack spills.
    let mut a: u64 = seed.wrapping_add(1);
    let mut b: u64 = seed.wrapping_add(2);
    let mut c: u64 = seed.wrapping_add(3);
    let mut d: u64 = seed.wrapping_add(4);
    let mut e: u64 = seed.wrapping_add(5);
    let mut f: u64 = seed.wrapping_add(6);
    let mut checksum: u64 = 0;

    // Also use a stack-allocated array to detect stack corruption.
    let mut stack_canary: [u64; 8] = [0; 8];
    for i in 0..8 {
        stack_canary[i] = 0xDEAD_BEEF_0000_0000u64 + seed * 8 + i as u64;
    }

    for i in 0u64..500 {
        // Compute between yields - uses all locals
        a = hash_step(a, b);
        b = hash_step(b, c);
        c = hash_step(c, d);
        d = hash_step(d, e);
        e = hash_step(e, f);
        f = hash_step(f, a);
        checksum = checksum.wrapping_add(a ^ b ^ c ^ d ^ e ^ f);

        // Alternate between yield, sleep, and park_while to exercise
        // different context switch paths.
        match i % 4 {
            0 => sched().thread_yield(),
            1 => sched().thread_sleep(Duration::from_micros(1)),
            2 => {
                // park_while with immediate false -> abort path (no switch)
                sched().thread_park_while(|| false);
            }
            _ => sched().thread_yield(),
        }
    }

    // Verify stack canary wasn't corrupted
    for i in 0..8 {
        let expected = 0xDEAD_BEEF_0000_0000u64 + seed * 8 + i as u64;
        assert!(
            stack_canary[i] == expected,
            "[sched-test] compute: stack canary corrupted at [{i}]: got {:#x}, expected {:#x}",
            stack_canary[i],
            expected
        );
    }

    // Recompute the expected checksum from scratch (no yields)
    let mut ea = seed.wrapping_add(1);
    let mut eb = seed.wrapping_add(2);
    let mut ec = seed.wrapping_add(3);
    let mut ed = seed.wrapping_add(4);
    let mut ee = seed.wrapping_add(5);
    let mut ef = seed.wrapping_add(6);
    let mut expected_checksum: u64 = 0;

    for _ in 0u64..500 {
        ea = hash_step(ea, eb);
        eb = hash_step(eb, ec);
        ec = hash_step(ec, ed);
        ed = hash_step(ed, ee);
        ee = hash_step(ee, ef);
        ef = hash_step(ef, ea);
        expected_checksum = expected_checksum.wrapping_add(ea ^ eb ^ ec ^ ed ^ ee ^ ef);
    }

    assert!(
        checksum == expected_checksum,
        "[sched-test] compute: checksum mismatch for seed {seed}: got {checksum:#x}, expected {expected_checksum:#x}"
    );

    test_done(&h, "compute-across-yields");
    sched().thread_exit(0);
}

// ---------------------------------------------------------------------------
// Coordinator
// ---------------------------------------------------------------------------

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
