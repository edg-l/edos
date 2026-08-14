use alloc::{boxed::Box, sync::Arc};
use core::{
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    time::Duration,
};

use crate::{
    println,
    thread::{
        mutex::BlockingMutex,
        preempt::{PreemptSpinlock, preempt_disable, preempt_enabled},
        runqueue::DEFAULT_PRIORITY,
        rwlock::RwLock as BlockingRwLock,
        scheduler::{
            SCHEDULERS, WakePriority, current_thread, current_thread_id, sched, thread_exit,
            thread_park, thread_park_while, thread_sleep, thread_yield,
        },
        thread::{ThreadId, get_thread_weak},
        util::{pick_sched_filtered, queue_spawn_kthread_affine, queue_spawn_kthread_named_arg},
        waitqueue::WaitQueue,
    },
    timer::Instant,
};

/// Test-only helper: wake a thread by TID via the Weak-handle API. Tests
/// still store raw TIDs in atomic cells because test threads are short-lived
/// and never recycle during a test run; this helper bridges that storage
/// back to the canonical `wake_thread` API.
fn wake_tid(tid: ThreadId, priority: WakePriority) {
    if let Some(handle) = get_thread_weak(tid) {
        sched().wake_thread(&handle, priority);
    }
}

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
    /// Whose turn it is, [`PING_TURN`] or [`PONG_TURN`]. The handshake is
    /// driven by this rather than by park/wake pairing up one for one, which
    /// `thread_park` does not promise.
    ping_pong_turn: AtomicU32,
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
    // blocking mutex / rwlock / waitqueue
    mutex_counter: BlockingMutex<u64>,
    mutex_workers_done: AtomicU32,
    rwlock_value: BlockingRwLock<u64>,
    rwlock_concurrent: AtomicU32,
    rwlock_max_concurrent: AtomicU32,
    rwlock_readers_done: AtomicU32,
    waitqueue: WaitQueue,
    waitqueue_arrived: AtomicU32,
    waitqueue_ready: AtomicBool,
    waitqueue_woken: AtomicU32,
    // priority starvation
    starvation_spinners: u32,
    starvation_started: AtomicU32,
    starvation_finished: AtomicU32,
    starvation_progress: AtomicU64,
    starvation_progress_saturated: AtomicU64,
    starvation_progress_end: AtomicU64,
    // affinity: the single CPU the pinned test thread is allowed on, plus the
    // handshake cell the waker reads its tid from
    affinity_target_cpu: u32,
    affinity_tid: AtomicU64,
    // load metric: how many pinned threads have reached their park, how many
    // pinned spinners are running, and the release the spinners wait for
    load_parkers_ready: AtomicU32,
    load_spinners_ready: AtomicU32,
    load_check_done: AtomicBool,
    // three-level starvation: the CPU all three are pinned to, and the same
    // start/finish/progress sampling the two-level case uses
    contend_cpu: u32,
    tri_started: AtomicU32,
    tri_finished: AtomicU32,
    tri_progress: AtomicU64,
    tri_progress_saturated: AtomicU64,
    tri_progress_end: AtomicU64,
    // weighted share: the window both threads count over, and their counts
    share_started: AtomicU32,
    share_deadline: AtomicU64,
    share_finished: AtomicU32,
    share_heavy: AtomicU64,
    share_light: AtomicU64,
}

const TOTAL_TESTS: u32 = 54;

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
        ping_pong_turn: AtomicU32::new(PONG_TURN),
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
        mutex_counter: BlockingMutex::new(0),
        mutex_workers_done: AtomicU32::new(0),
        rwlock_value: BlockingRwLock::new(0),
        rwlock_concurrent: AtomicU32::new(0),
        rwlock_max_concurrent: AtomicU32::new(0),
        rwlock_readers_done: AtomicU32::new(0),
        waitqueue: WaitQueue::new(),
        waitqueue_arrived: AtomicU32::new(0),
        waitqueue_ready: AtomicBool::new(false),
        waitqueue_woken: AtomicU32::new(0),
        starvation_spinners: SCHEDULERS.read().len() as u32,
        starvation_started: AtomicU32::new(0),
        starvation_finished: AtomicU32::new(0),
        starvation_progress: AtomicU64::new(0),
        starvation_progress_saturated: AtomicU64::new(0),
        starvation_progress_end: AtomicU64::new(0),
        // Highest registered lapic id, so the pin is a CPU other than the boot
        // one whenever the machine has more than one.
        affinity_target_cpu: SCHEDULERS.read().keys().copied().max().unwrap_or(0),
        affinity_tid: AtomicU64::new(0),
        load_parkers_ready: AtomicU32::new(0),
        load_spinners_ready: AtomicU32::new(0),
        load_check_done: AtomicBool::new(false),
        // The *lowest* registered lapic id. The affinity and load cases both
        // pin to the highest ids, and these two saturate whatever CPU they land
        // on, so they are put as far from those as the machine allows.
        contend_cpu: SCHEDULERS.read().keys().copied().min().unwrap_or(0),
        tri_started: AtomicU32::new(0),
        tri_finished: AtomicU32::new(0),
        tri_progress: AtomicU64::new(0),
        tri_progress_saturated: AtomicU64::new(0),
        tri_progress_end: AtomicU64::new(0),
        share_started: AtomicU32::new(0),
        share_deadline: AtomicU64::new(0),
        share_finished: AtomicU32::new(0),
        share_heavy: AtomicU64::new(0),
        share_light: AtomicU64::new(0),
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

    // Preemption counter semantics (1)
    spawn_test(&harness, "test-preempt-count", test_preempt_count);
    // BlockingMutex mutual exclusion under contention (MUTEX_WORKERS + 1)
    for _ in 0..MUTEX_WORKERS {
        spawn_test(&harness, "test-mutex-worker", test_mutex_worker);
    }
    spawn_test(&harness, "test-mutex-check", test_mutex_check);
    // BlockingRwLock: readers share, writer excludes (RWLOCK_READERS + 1)
    for _ in 0..RWLOCK_READERS {
        spawn_test(&harness, "test-rwlock-reader", test_rwlock_reader);
    }
    spawn_test(&harness, "test-rwlock-writer", test_rwlock_writer);
    // WaitQueue wake_all releases every waiter (WQ_WAITERS + 1)
    for _ in 0..WQ_WAITERS {
        spawn_test(&harness, "test-wq-waiter", test_wq_waiter);
    }
    spawn_test(&harness, "test-wq-waker", test_wq_waker);

    // HID report descriptors: the parser that decides what a pointing device's
    // reports mean, checked against the two QEMU speaks (1)
    spawn_test(&harness, "test-hid-report", test_hid_report);

    // The byte ring behind every pipe and PTY, where wrapping meets growth (1)
    spawn_test(&harness, "test-byte-ring", test_byte_ring);

    // Priority starvation: one busy spinner per CPU plus one victim below them (1)
    for _ in 0..harness.starvation_spinners {
        spawn_test(&harness, "test-starve-spin", test_starvation_spinner);
    }
    spawn_test(&harness, "test-starve-victim", test_starvation_victim);

    // Affinity: one thread pinned to a single CPU, asserting where it runs,
    // and an unpinned partner that wakes it from wherever it happens to be (2)
    spawn_test(&harness, "test-affinity-waker", test_affinity_waker);
    {
        let boxed = Box::into_raw(Box::new(harness.clone())) as *mut u8;
        queue_spawn_kthread_affine(
            "test-affinity",
            test_affinity_pinned as *const () as u64,
            boxed,
            1u32 << harness.affinity_target_cpu,
        );
    }

    // Load metric: threads parked on one CPU must not repel work from it (1)
    spawn_test(&harness, "test-load-parked", test_load_parked);

    // Three occupied priority levels on one CPU: the bottom one must run (1)
    let contend_mask = 1u32 << harness.contend_cpu;
    for prio in [TRI_HIGH_PRIORITY, TRI_MID_PRIORITY] {
        let boxed = Box::into_raw(Box::new((harness.clone(), prio as u64))) as *mut u8;
        queue_spawn_kthread_affine(
            "test-tri-spin",
            test_tri_spinner as *const () as u64,
            boxed,
            contend_mask,
        );
    }
    {
        let boxed = Box::into_raw(Box::new(harness.clone())) as *mut u8;
        queue_spawn_kthread_affine(
            "test-tri-victim",
            test_tri_victim as *const () as u64,
            boxed,
            contend_mask,
        );
    }

    // Weighted share: priority must buy CPU in proportion to weight (1)
    for prio in [SHARE_HEAVY_PRIORITY, SHARE_LIGHT_PRIORITY] {
        let boxed = Box::into_raw(Box::new((harness.clone(), prio as u64))) as *mut u8;
        queue_spawn_kthread_affine(
            "test-share",
            test_weighted_share as *const () as u64,
            boxed,
            contend_mask,
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

extern "C" fn test_hid_report(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    crate::drivers::usb::hid::report::tests::check();
    test_done(&h, "hid-report-descriptor");
    thread_exit(0);
}

extern "C" fn test_byte_ring(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    crate::util::ring::tests::check();
    test_done(&h, "byte-ring");
    thread_exit(0);
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
    let tid = current_thread_id().unwrap();
    h.wake_target_tid.store(tid.0, Ordering::Release);
    thread_park();
    // Resumed - we were woken by B
    test_done(&h, "park-wake-a");
    thread_exit(0);
}

extern "C" fn test_park_b(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    // Wait for A to store its TID
    while h.wake_target_tid.load(Ordering::Acquire) == 0 {
        thread_yield();
    }
    // Give A time to actually park
    thread_sleep(Duration::from_millis(10));
    let tid = ThreadId(h.wake_target_tid.load(Ordering::Acquire));
    wake_tid(tid, WakePriority::Normal);
    test_done(&h, "park-wake-b");
    thread_exit(0);
}

extern "C" fn test_sleep(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    let start = Instant::now();
    thread_sleep(Duration::from_millis(50));
    let elapsed = Instant::now().duration_since(start);
    assert!(
        elapsed.as_millis() >= 45,
        "[sched-test] sleep too short: {}ms",
        elapsed.as_millis()
    );
    test_done(&h, "sleep");
    thread_exit(0);
}

extern "C" fn test_yield_stress(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    for _ in 0..1000 {
        thread_yield();
    }
    test_done(&h, "yield-stress");
    thread_exit(0);
}

extern "C" fn test_park_while_abort(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    // Condition is already false, thread_park_while should return without switching
    thread_park_while(|| false);
    test_done(&h, "park-while-abort");
    thread_exit(0);
}

extern "C" fn test_park_while_c(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    let tid = current_thread_id().unwrap();
    h.park_while_tid.store(tid.0, Ordering::Release);
    h.park_while_condition.store(true, Ordering::Release);
    thread_park_while(|| h.park_while_condition.load(Ordering::Acquire));
    // Resumed - condition should now be false
    assert!(
        !h.park_while_condition.load(Ordering::Acquire),
        "[sched-test] park-while condition still true after wake"
    );
    test_done(&h, "park-while-c");
    thread_exit(0);
}

extern "C" fn test_park_while_d(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    while h.park_while_tid.load(Ordering::Acquire) == 0 {
        thread_yield();
    }
    thread_sleep(Duration::from_millis(10));
    h.park_while_condition.store(false, Ordering::Release);
    let tid = ThreadId(h.park_while_tid.load(Ordering::Acquire));
    wake_tid(tid, WakePriority::Normal);
    test_done(&h, "park-while-d");
    thread_exit(0);
}

extern "C" fn test_context_saved(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    thread_yield();
    // After resuming from yield, context_saved should be false
    // (cleared by context_switch_to when we were scheduled back)
    let cur = current_thread().unwrap();
    assert!(
        !cur.context_saved.load(Ordering::Acquire),
        "[sched-test] context_saved should be false for running thread"
    );
    test_done(&h, "context-saved");
    thread_exit(0);
}

// ---------------------------------------------------------------------------
// Stress: rapid park/wake ping-pong (tests waker-CPU enqueue under contention)
// ---------------------------------------------------------------------------

const PING_PONG_ROUNDS: u32 = 500;
const PING_TURN: u32 = 0;
const PONG_TURN: u32 = 1;

/// Block until it is `turn`'s go.
///
/// The loop is the point. `thread_park` may return without a matching wake --
/// a wake-pending token published while the thread was still running is
/// consumed by the transition and short-circuits the park -- so a park is not
/// a receipt for one wake. Pairing them one for one desynchronises the
/// handshake by a round the first time it happens, and the two sides then
/// disagree about how many rounds have run.
fn await_turn(h: &TestHarness, turn: u32) {
    while h.ping_pong_turn.load(Ordering::Acquire) != turn {
        thread_park();
    }
}

extern "C" fn test_ping(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    let tid = current_thread_id().unwrap();
    h.ping_tid.store(tid.0, Ordering::Release);

    // Wait for pong to register
    while h.pong_tid.load(Ordering::Acquire) == 0 {
        thread_yield();
    }
    let pong = ThreadId(h.pong_tid.load(Ordering::Acquire));

    for _ in 0..PING_PONG_ROUNDS {
        await_turn(&h, PING_TURN);
        h.ping_pong_turn.store(PONG_TURN, Ordering::Release);
        wake_tid(pong, WakePriority::Normal);
    }

    // Pong hands the turn over only after incrementing, and ping's last round
    // ended on such a handover, so the count is already final here.
    let count = h.ping_pong_count.load(Ordering::Acquire);
    assert!(
        count == PING_PONG_ROUNDS,
        "[sched-test] ping-pong count mismatch: {count} != {PING_PONG_ROUNDS}"
    );
    test_done(&h, "ping-pong-ping");
    thread_exit(0);
}

extern "C" fn test_pong(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    let tid = current_thread_id().unwrap();
    h.pong_tid.store(tid.0, Ordering::Release);

    // Wait for ping to register
    while h.ping_tid.load(Ordering::Acquire) == 0 {
        thread_yield();
    }
    let ping = ThreadId(h.ping_tid.load(Ordering::Acquire));

    // No sleep to let ping park first: the turn is what synchronises them, so
    // starting before ping has parked exercises the wake-before-park path
    // rather than breaking the handshake.
    for _ in 0..PING_PONG_ROUNDS {
        await_turn(&h, PONG_TURN);
        // Count before handing the turn over, so ping cannot be released into
        // a window where the round is done but not yet counted.
        h.ping_pong_count.fetch_add(1, Ordering::AcqRel);
        h.ping_pong_turn.store(PING_TURN, Ordering::Release);
        wake_tid(ping, WakePriority::Normal);
    }

    test_done(&h, "ping-pong-pong");
    thread_exit(0);
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
        thread_sleep(Duration::from_millis(10));
    }

    test_done(&h, "spawn-exit");
    thread_exit(0);
}

extern "C" fn test_ephemeral_thread(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    // Do a yield to exercise the scheduler, then exit
    thread_yield();
    h.spawn_exit_done.fetch_add(1, Ordering::AcqRel);
    thread_exit(0);
}

// ---------------------------------------------------------------------------
// Stress: 4 threads park, 1 waker wakes all at once (tests multi-wake)
// ---------------------------------------------------------------------------

extern "C" fn test_multi_wake_sleeper(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    let tid = current_thread_id().unwrap();

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
    thread_park_while(|| !h.multi_wake_gate.load(Ordering::Acquire));

    assert!(
        h.multi_wake_gate.load(Ordering::Acquire),
        "[sched-test] multi-wake gate not open after resume"
    );
    test_done(&h, "multi-wake-sleeper");
    thread_exit(0);
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
        thread_yield();
    }

    // Give sleepers time to actually park
    thread_sleep(Duration::from_millis(20));

    // Open the gate and wake all 4
    h.multi_wake_gate.store(true, Ordering::Release);
    for slot in &h.multi_wake_tids {
        let tid = slot.load(Ordering::Acquire);
        if tid != 0 {
            wake_tid(ThreadId(tid), WakePriority::Normal);
        }
    }

    test_done(&h, "multi-wake-waker");
    thread_exit(0);
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
    let tid = current_thread_id().unwrap();
    h.abort_race_tid.store(tid.0, Ordering::Release);

    for _ in 0..ABORT_RACE_ROUNDS {
        h.abort_race_condition.store(true, Ordering::Release);
        // Park while condition is true. The waker will set it to false
        // and wake us. If the waker is fast enough, it wakes us before
        // we even check the condition, triggering the abort path.
        //
        // `thread_park_while` may return spuriously, so the round is over
        // only once the condition itself is observed false.
        while h.abort_race_condition.load(Ordering::Acquire) {
            thread_park_while(|| h.abort_race_condition.load(Ordering::Acquire));
        }
    }

    let rounds = h.abort_race_rounds.load(Ordering::Acquire);
    assert!(
        rounds == ABORT_RACE_ROUNDS,
        "[sched-test] abort-race round mismatch: {rounds} != {ABORT_RACE_ROUNDS}"
    );
    test_done(&h, "abort-race-parker");
    thread_exit(0);
}

extern "C" fn test_abort_race_waker(arg: *mut u8) -> ! {
    let h = get_harness(arg);

    // Wait for parker to register
    while h.abort_race_tid.load(Ordering::Acquire) == 0 {
        thread_yield();
    }
    let parker = ThreadId(h.abort_race_tid.load(Ordering::Acquire));

    for _ in 0..ABORT_RACE_ROUNDS {
        // Spin until condition is set (parker is about to park or has parked)
        while !h.abort_race_condition.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        // Count the round before releasing the parker, so a parker that
        // observes the condition false has necessarily observed the count.
        h.abort_race_rounds.fetch_add(1, Ordering::AcqRel);
        // Immediately flip condition and wake -- NO sleep, race the parker
        h.abort_race_condition.store(false, Ordering::Release);
        wake_tid(parker, WakePriority::Normal);
        // Yield to give parker a chance to run its next iteration
        thread_yield();
    }

    test_done(&h, "abort-race-waker");
    thread_exit(0);
}

// ---------------------------------------------------------------------------
// Preemption counter
// `preempt_disable` is what makes a spin lock bounded, so its nesting and its
// balance on guard drop are load-bearing: a leaked count silently disables
// preemption on that CPU for good.
// ---------------------------------------------------------------------------

extern "C" fn test_preempt_count(arg: *mut u8) -> ! {
    let h = get_harness(arg);

    assert!(preempt_enabled(), "[sched-test] preempt: disabled on entry");
    {
        let _outer = preempt_disable();
        assert!(
            !preempt_enabled(),
            "[sched-test] preempt: still enabled inside a guard"
        );
        {
            let _inner = preempt_disable();
            assert!(
                !preempt_enabled(),
                "[sched-test] preempt: still enabled inside a nested guard"
            );
        }
        assert!(
            !preempt_enabled(),
            "[sched-test] preempt: inner guard re-enabled preemption for the outer one"
        );
    }
    assert!(
        preempt_enabled(),
        "[sched-test] preempt: count leaked after the outermost guard dropped"
    );

    // A lock guard must restore the count too, including on the read path.
    let lock: PreemptSpinlock<u32> = PreemptSpinlock::new(0);
    {
        let mut g = lock.lock();
        *g += 1;
        assert!(
            !preempt_enabled(),
            "[sched-test] preempt: PreemptSpinlock guard did not suppress preemption"
        );
    }
    assert!(
        preempt_enabled(),
        "[sched-test] preempt: PreemptSpinlock guard leaked the count"
    );

    test_done(&h, "preempt-count");
    thread_exit(0);
}

// ---------------------------------------------------------------------------
// BlockingMutex: mutual exclusion under contention
// Every worker does read-modify-write through the guard, so a lost update or a
// torn increment shows up as a final count below the expected total.
// ---------------------------------------------------------------------------

const MUTEX_WORKERS: u32 = 4;
const MUTEX_INCREMENTS: u64 = 500;

extern "C" fn test_mutex_worker(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    for _ in 0..MUTEX_INCREMENTS {
        let mut guard = h.mutex_counter.lock();
        let seen = *guard;
        // Yielding under the guard widens the window for a broken mutex to
        // interleave two writers; a correct one still serialises them.
        thread_yield();
        *guard = seen + 1;
    }
    h.mutex_workers_done.fetch_add(1, Ordering::AcqRel);
    test_done(&h, "mutex-worker");
    thread_exit(0);
}

extern "C" fn test_mutex_check(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    while h.mutex_workers_done.load(Ordering::Acquire) < MUTEX_WORKERS {
        thread_yield();
    }
    let total = *h.mutex_counter.lock();
    let expected = MUTEX_WORKERS as u64 * MUTEX_INCREMENTS;
    assert!(
        total == expected,
        "[sched-test] mutex: {total} increments survived, expected {expected}"
    );
    test_done(&h, "mutex-exclusion");
    thread_exit(0);
}

// ---------------------------------------------------------------------------
// BlockingRwLock: readers share, a writer excludes
// ---------------------------------------------------------------------------

const RWLOCK_READERS: u32 = 4;

extern "C" fn test_rwlock_reader(arg: *mut u8) -> ! {
    let h = get_harness(arg);

    // Hold the read guard long enough to overlap with the other readers, and
    // record the high-water mark of concurrent holders.
    let guard = h.rwlock_value.read();
    let live = h.rwlock_concurrent.fetch_add(1, Ordering::AcqRel) + 1;
    h.rwlock_max_concurrent.fetch_max(live, Ordering::AcqRel);
    // Hold until every reader is inside, so the overlap the writer asserts on
    // is produced deterministically rather than by racing. Bounded, so a
    // rwlock that wrongly serialises readers fails the assert instead of
    // deadlocking the suite.
    for _ in 0..2000 {
        if h.rwlock_concurrent.load(Ordering::Acquire) >= RWLOCK_READERS {
            break;
        }
        thread_yield();
    }
    h.rwlock_max_concurrent.fetch_max(
        h.rwlock_concurrent.load(Ordering::Acquire),
        Ordering::AcqRel,
    );
    assert!(
        *guard == 0,
        "[sched-test] rwlock: writer mutated the value while a reader held it"
    );
    h.rwlock_concurrent.fetch_sub(1, Ordering::AcqRel);
    drop(guard);

    h.rwlock_readers_done.fetch_add(1, Ordering::AcqRel);
    test_done(&h, "rwlock-reader");
    thread_exit(0);
}

extern "C" fn test_rwlock_writer(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    while h.rwlock_readers_done.load(Ordering::Acquire) < RWLOCK_READERS {
        thread_yield();
    }

    {
        let mut guard = h.rwlock_value.write();
        assert!(
            h.rwlock_concurrent.load(Ordering::Acquire) == 0,
            "[sched-test] rwlock: write guard acquired while readers were live"
        );
        *guard = 1;
    }

    let observed = h.rwlock_max_concurrent.load(Ordering::Acquire);
    assert!(
        observed > 1,
        "[sched-test] rwlock: readers never overlapped (max {observed}), \
         so the lock is serialising them like a mutex"
    );
    test_done(&h, "rwlock-writer");
    thread_exit(0);
}

// ---------------------------------------------------------------------------
// WaitQueue: wake_all releases every waiter
// A waiter left behind is the missed-wakeup shape from
// doc/bugs/2026-04-13-sched-park-wake-missed-wakeup.md.
// ---------------------------------------------------------------------------

const WQ_WAITERS: u32 = 4;

extern "C" fn test_wq_waiter(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    // Announce arrival before waiting. A waiter that has not reached the queue
    // by the time the waker fires still observes the published condition on
    // `wait_until`'s entry check, so no handshake window is left open.
    h.waitqueue_arrived.fetch_add(1, Ordering::AcqRel);
    h.waitqueue
        .wait_until(|| h.waitqueue_ready.load(Ordering::Acquire));
    h.waitqueue_woken.fetch_add(1, Ordering::AcqRel);
    test_done(&h, "waitqueue-waiter");
    thread_exit(0);
}

extern "C" fn test_wq_waker(arg: *mut u8) -> ! {
    let h = get_harness(arg);

    // Wait for every waiter, not merely for the queue to become non-empty:
    // waking after only the first has enrolled leaves the rest to park against
    // a condition nobody will publish again.
    while h.waitqueue_arrived.load(Ordering::Acquire) < WQ_WAITERS {
        thread_yield();
    }

    h.waitqueue_ready.store(true, Ordering::Release);
    h.waitqueue.wake_all();

    // Every waiter must make it out; a lost wake leaves one parked forever.
    while h.waitqueue_woken.load(Ordering::Acquire) < WQ_WAITERS {
        thread_yield();
    }
    test_done(&h, "waitqueue-wake-all");
    thread_exit(0);
}

// ---------------------------------------------------------------------------
// Priority starvation
// One CPU-bound spinner per CPU, all above DEFAULT_PRIORITY, occupy every
// runqueue without ever parking or yielding. A thread below them must still
// reach the CPU: strict priority order that never services a lower level turns
// any lock shared across priorities into a deadlock, because the holder sits
// Ready while the spinner above it waits on the lock it holds.
// ---------------------------------------------------------------------------

const STARVATION_SPIN_PRIORITY: u8 = DEFAULT_PRIORITY + 3;
const STARVATION_SPIN_MS: u64 = 400;

extern "C" fn test_starvation_spinner(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    current_thread()
        .unwrap()
        .set_priority(STARVATION_SPIN_PRIORITY);

    // The last spinner to start marks the point where every CPU is claimed.
    if h.starvation_started.fetch_add(1, Ordering::AcqRel) + 1 == h.starvation_spinners {
        h.starvation_progress_saturated.store(
            h.starvation_progress.load(Ordering::Acquire),
            Ordering::Release,
        );
    }

    let deadline = Instant::now() + Duration::from_millis(STARVATION_SPIN_MS);
    while Instant::now() < deadline {
        core::hint::spin_loop();
    }

    // The first spinner to finish marks the point where a CPU frees up, so
    // the sampled interval covers only the fully saturated window.
    if h.starvation_finished.fetch_add(1, Ordering::AcqRel) == 0 {
        h.starvation_progress_end.store(
            h.starvation_progress.load(Ordering::Acquire),
            Ordering::Release,
        );
    }
    thread_exit(0);
}

extern "C" fn test_starvation_victim(arg: *mut u8) -> ! {
    let h = get_harness(arg);

    // Stay runnable for the whole window at the default priority. Progress is
    // sampled by the spinners, so a victim that is never picked shows the two
    // samples equal rather than a timestamp taken after starvation ended.
    while h.starvation_finished.load(Ordering::Acquire) < h.starvation_spinners {
        h.starvation_progress.fetch_add(1, Ordering::Relaxed);
        core::hint::spin_loop();
    }

    let saturated = h.starvation_progress_saturated.load(Ordering::Acquire);
    let end = h.starvation_progress_end.load(Ordering::Acquire);
    assert!(
        end > saturated,
        "[sched-test] starvation: victim made no progress while {} spinners held every CPU",
        h.starvation_spinners
    );

    test_done(&h, "starvation-victim");
    thread_exit(0);
}

// ---------------------------------------------------------------------------
// Affinity: a pinned thread only ever runs on a CPU its mask allows.
//
// Covers all three points where affinity is enforced: the initial placement in
// spawn_thread, the wake placement in complete_wake, and the check that stops
// another CPU from stealing the thread.
//
// The park/wake rounds are what make this test discriminating. Yields alone are
// not: they re-enqueue on the CPU the thread is already on, so a thread that
// reached the right CPU by luck stays there and the test passes with affinity
// disabled entirely. A wake, by contrast, enqueues on the *waker's* CPU for
// cache locality unless affinity overrides it, and the waker is somewhere else
// most rounds.
// ---------------------------------------------------------------------------

const AFFINITY_YIELDS: u32 = 200;
const AFFINITY_ROUNDS: u32 = 20;

/// The CPU this thread is running on, read with preemption off so a migration
/// cannot land between the read and the caller's comparison.
fn current_cpu() -> u32 {
    let _g = preempt_disable();
    sched().cpu
}

extern "C" fn test_affinity_pinned(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    let target = h.affinity_target_cpu;
    let tid = current_thread_id().unwrap();

    let check = |where_: &str, round: u32| {
        let cpu = current_cpu();
        assert_eq!(
            cpu, target,
            "[sched-test] affinity: pinned to cpu {target}, ran on cpu {cpu} after {where_} {round}"
        );
    };

    check("spawn", 0);

    for round in 0..AFFINITY_ROUNDS {
        h.affinity_tid.store(tid.0, Ordering::Release);
        thread_park();
        check("wake", round);
    }

    for round in 0..AFFINITY_YIELDS {
        thread_yield();
        check("yield", round);
    }

    h.affinity_tid.store(u64::MAX, Ordering::Release);
    test_done(&h, "affinity-pinned");
    thread_exit(0);
}

extern "C" fn test_affinity_waker(arg: *mut u8) -> ! {
    let h = get_harness(arg);

    // Driven by the sentinel rather than by a round count. A park whose
    // wake-pending token is already set returns without blocking, so the pinned
    // thread can burn a round without needing a wake; a waker counting its own
    // rounds would then outlive the sentinel and block forever on an empty cell.
    loop {
        let tid = h.affinity_tid.swap(0, Ordering::AcqRel);
        if tid == u64::MAX {
            break;
        }
        if tid == 0 {
            thread_yield();
            continue;
        }
        // Let it reach the park. Waking earlier is handled by the wake-pending
        // token (see the wake-before-park test), so this only keeps the rounds
        // representative of a real wake.
        thread_sleep(Duration::from_millis(2));
        wake_tid(ThreadId(tid), WakePriority::Normal);
    }

    test_done(&h, "affinity-waker");
    thread_exit(0);
}

// ---------------------------------------------------------------------------
// Stress: wake-before-park
// The waker calls wake_thread while the target is still Running (hasn't
// parked yet). wake_thread_slow must handle this by spinning/retrying.
// ---------------------------------------------------------------------------

extern "C" fn test_wbp_parker(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    let tid = current_thread_id().unwrap();
    h.wbp_tid.store(tid.0, Ordering::Release);

    // Signal that we're about to park, then park.
    // The waker will wake us BEFORE we actually enter park.
    h.wbp_parked.store(true, Ordering::Release);
    thread_park();

    // If we get here, the wake eventually succeeded
    test_done(&h, "wake-before-park-parker");
    thread_exit(0);
}

extern "C" fn test_wbp_waker(arg: *mut u8) -> ! {
    let h = get_harness(arg);

    // Wait for parker TID
    while h.wbp_tid.load(Ordering::Acquire) == 0 {
        thread_yield();
    }
    let parker = ThreadId(h.wbp_tid.load(Ordering::Acquire));

    // Wait for the "about to park" signal, then wake IMMEDIATELY.
    // The parker might not have called thread_park yet -- wake_thread_slow
    // must handle the Running state by retrying.
    while !h.wbp_parked.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    // No sleep! Wake immediately while parker might still be Running.
    wake_tid(parker, WakePriority::Normal);

    test_done(&h, "wake-before-park-waker");
    thread_exit(0);
}

// ---------------------------------------------------------------------------
// Stress: sleep interrupted by early wake
// Thread sleeps for 10 seconds, waker wakes it after 5ms.
// Tests the Sleeping -> Waking transition and early wakeup path.
// ---------------------------------------------------------------------------

extern "C" fn test_sleep_interrupt_sleeper(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    let tid = current_thread_id().unwrap();
    h.sleep_int_tid.store(tid.0, Ordering::Release);

    let start = Instant::now();
    // Sleep for 10 seconds (will be woken early)
    thread_sleep(Duration::from_secs(10));
    let elapsed = start.elapsed();

    // Should have been woken well before 10 seconds
    assert!(
        elapsed.as_millis() < 1000,
        "[sched-test] sleep-interrupt: slept too long ({}ms), wake failed?",
        elapsed.as_millis()
    );
    test_done(&h, "sleep-interrupt-sleeper");
    thread_exit(0);
}

extern "C" fn test_sleep_interrupt_waker(arg: *mut u8) -> ! {
    let h = get_harness(arg);

    while h.sleep_int_tid.load(Ordering::Acquire) == 0 {
        thread_yield();
    }
    let sleeper = ThreadId(h.sleep_int_tid.load(Ordering::Acquire));

    // Give sleeper time to enter sleep
    thread_sleep(Duration::from_millis(5));
    // Wake the sleeping thread early
    wake_tid(sleeper, WakePriority::Normal);

    test_done(&h, "sleep-interrupt-waker");
    thread_exit(0);
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
            0 => thread_yield(),
            1 => thread_sleep(Duration::from_micros(1)),
            2 => {
                // park_while with immediate false -> abort path (no switch)
                thread_park_while(|| false);
            }
            _ => thread_yield(),
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
    thread_exit(0);
}

// ---------------------------------------------------------------------------
// Load: a parked thread is not load
//
// A CPU's load has to describe the work waiting on it, not the threads that
// happen to call it home. `LOAD_PARKERS` threads pinned to one CPU and parked
// there are runnable nowhere and cost that CPU nothing, so it is a *better*
// place for new work than a CPU with a handful of threads actually running.
//
// The comparison is between two CPUs this test owns the contents of, which is
// what makes it deterministic: asking instead whether the parked CPU wins
// placements outright depends on what the rest of the suite is doing on every
// other CPU at that moment, and it legitimately loses to an idle one.
// ---------------------------------------------------------------------------

/// Enough that a metric counting membership cannot mistake the parked CPU for
/// the emptier of the two.
const LOAD_PARKERS: u32 = 32;
/// Real, runnable work on the CPU the parked one is compared against.
const LOAD_SPINNERS: u32 = 6;
/// Ceiling on how long a spinner holds its CPU if the checker never releases
/// it, so a failure elsewhere cannot leave the suite burning a core.
const LOAD_SPIN_LIMIT_MS: u64 = 5_000;

extern "C" fn test_load_parked(arg: *mut u8) -> ! {
    let h = get_harness(arg);

    // Two CPUs to compare. `parked_cpu` is the highest lapic id and `busy_cpu`
    // the next one down; a single-CPU boot has no comparison to make and
    // checks only the quantity.
    let mut cpus: heapless::Vec<u32, 128> = SCHEDULERS.read().keys().copied().collect();
    cpus.sort_unstable();
    let parked_cpu = cpus.pop().expect("load: no schedulers registered");
    let busy_cpu = cpus.pop();

    for _ in 0..LOAD_PARKERS {
        let boxed = Box::into_raw(Box::new(h.clone())) as *mut u8;
        queue_spawn_kthread_affine(
            "test-load-parker",
            test_load_parker as *const () as u64,
            boxed,
            1u32 << parked_cpu,
        );
    }

    while h.load_parkers_ready.load(Ordering::Acquire) < LOAD_PARKERS {
        thread_yield();
    }
    // Readiness is published before the park, so give the last of them time to
    // reach it. A parker still on its way is runnable and does weigh 1.
    thread_sleep(Duration::from_millis(20));

    // The quantity itself. The bound is the parker count rather than a small
    // number because the suite's own threads land on that CPU too; what it must
    // never report is load that scales with the threads parked on it.
    let parked_load = cpu_load(parked_cpu);
    assert!(
        parked_load < LOAD_PARKERS as u64,
        "[sched-test] load: cpu {parked_cpu} reports load {parked_load} with {LOAD_PARKERS} \
         threads parked on it -- load is counting membership, not runnable work"
    );

    if let Some(busy_cpu) = busy_cpu {
        for _ in 0..LOAD_SPINNERS {
            let boxed = Box::into_raw(Box::new(h.clone())) as *mut u8;
            queue_spawn_kthread_affine(
                "test-load-spinner",
                test_load_spinner as *const () as u64,
                boxed,
                1u32 << busy_cpu,
            );
        }
        while h.load_spinners_ready.load(Ordering::Acquire) < LOAD_SPINNERS {
            thread_yield();
        }

        let parked_load = cpu_load(parked_cpu);
        let busy_load = cpu_load(busy_cpu);
        assert!(
            parked_load < busy_load,
            "[sched-test] load: cpu {parked_cpu} with {LOAD_PARKERS} threads parked reports \
             {parked_load}, cpu {busy_cpu} with {LOAD_SPINNERS} running reports {busy_load}"
        );

        // And the consumer: given only those two, placement must choose the one
        // with nothing runnable on it.
        let chosen = pick_sched_filtered(|cpu| cpu == parked_cpu || cpu == busy_cpu)
            .expect("load: neither cpu is registered")
            .cpu;
        assert_eq!(
            chosen, parked_cpu,
            "[sched-test] load: placement chose cpu {chosen} over cpu {parked_cpu}, whose \
             {LOAD_PARKERS} threads are all parked"
        );

        h.load_check_done.store(true, Ordering::Release);
    }

    test_done(&h, "load-parked-is-not-load");
    thread_exit(0);
}

/// A CPU's runnable load, straight from its scheduler.
fn cpu_load(cpu: u32) -> u64 {
    SCHEDULERS
        .read()
        .get(&cpu)
        .expect("load: no scheduler for cpu")
        .load()
}

/// Pinned to one CPU and parked there for the rest of the run.
extern "C" fn test_load_parker(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    h.load_parkers_ready.fetch_add(1, Ordering::AcqRel);
    loop {
        thread_park_while(|| true);
    }
}

/// Pinned to the CPU the parked one is compared against, and runnable for as
/// long as the comparison takes.
extern "C" fn test_load_spinner(arg: *mut u8) -> ! {
    let h = get_harness(arg);
    h.load_spinners_ready.fetch_add(1, Ordering::AcqRel);
    let deadline = Instant::now() + Duration::from_millis(LOAD_SPIN_LIMIT_MS);
    while !h.load_check_done.load(Ordering::Acquire) && Instant::now() < deadline {
        core::hint::spin_loop();
    }
    thread_exit(0);
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
        thread_sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------------
// Three occupied priority levels on one CPU
//
// The two-level starvation case above cannot reach this. Strict levels with an
// anti-starvation escape serviced the highest non-empty level *below* the top,
// so with three levels occupied on one runqueue the escape always went to the
// middle one and the bottom waited on an empty promise. Under EEVDF a passed
// over thread falls behind V, becomes eligible, and its deadline is already in
// the past — there is no level to be behind.
//
// All three are pinned to one CPU, which is what makes it three levels in one
// runqueue rather than three runqueues with one level each.
// ---------------------------------------------------------------------------

const TRI_HIGH_PRIORITY: u8 = DEFAULT_PRIORITY + 3;
const TRI_MID_PRIORITY: u8 = DEFAULT_PRIORITY + 2;
const TRI_SPIN_MS: u64 = 300;
const TRI_SPINNERS: u32 = 2;

extern "C" fn test_tri_spinner(arg: *mut u8) -> ! {
    let (h, priority) = unsafe { *Box::from_raw(arg as *mut (Arc<TestHarness>, u64)) };
    current_thread().unwrap().set_priority(priority as u8);

    // The last spinner up marks the point where both upper levels are occupied.
    if h.tri_started.fetch_add(1, Ordering::AcqRel) + 1 == TRI_SPINNERS {
        h.tri_progress_saturated
            .store(h.tri_progress.load(Ordering::Acquire), Ordering::Release);
    }

    let deadline = Instant::now() + Duration::from_millis(TRI_SPIN_MS);
    while Instant::now() < deadline {
        core::hint::spin_loop();
    }

    // The first to finish ends the window, so the samples bracket only the
    // stretch where every level above the victim was busy.
    if h.tri_finished.fetch_add(1, Ordering::AcqRel) == 0 {
        h.tri_progress_end
            .store(h.tri_progress.load(Ordering::Acquire), Ordering::Release);
    }
    thread_exit(0);
}

extern "C" fn test_tri_victim(arg: *mut u8) -> ! {
    let h = get_harness(arg);

    while h.tri_finished.load(Ordering::Acquire) < TRI_SPINNERS {
        h.tri_progress.fetch_add(1, Ordering::Relaxed);
        core::hint::spin_loop();
    }

    let saturated = h.tri_progress_saturated.load(Ordering::Acquire);
    let end = h.tri_progress_end.load(Ordering::Acquire);
    assert!(
        end > saturated,
        "[sched-test] tri-starvation: the bottom of three occupied levels made no progress \
         on cpu {} while {TRI_SPINNERS} spinners above it ran",
        h.contend_cpu
    );

    test_done(&h, "starvation-three-levels");
    thread_exit(0);
}

// ---------------------------------------------------------------------------
// Weighted share
//
// Two CPU-bound threads pinned to one CPU, seven priority levels apart, each
// counting its own loop iterations over one window. The counts are the share.
//
// This is the thing the priority buckets could not express, and it is bounded
// on both sides because they failed on both. In isolation the escape hatch gave
// the upper thread two picks in three *whatever the gap*, so the answer was 2.0
// for every pair and a 16-level dial had two settings — the lower bound catches
// that. On a real machine it was worse: this CPU also carries suite threads at
// IO_PRIORITY, and an escape that only ever serviced the highest level below
// the top never reached the bottom one at all. Measured against the bucket
// scheme, seven levels bought **58.55x** (heavy 35383060, light 604304), which
// is not a share at all — the upper bound catches that.
//
// Seven levels is 1.25^7 = 4.77x of weight, and both bounds sit clear of it.
//
// The ratio is asserted rather than either count, because the rest of the suite
// is running too. Interference costs both threads roughly in proportion and so
// leaves the ratio alone, where it would move an absolute count freely.
// ---------------------------------------------------------------------------

const SHARE_HEAVY_PRIORITY: u8 = DEFAULT_PRIORITY + 7;
const SHARE_LIGHT_PRIORITY: u8 = DEFAULT_PRIORITY;
const SHARE_WINDOW_MS: u64 = 300;
const SHARE_THREADS: u32 = 2;

extern "C" fn test_weighted_share(arg: *mut u8) -> ! {
    let (h, priority) = unsafe { *Box::from_raw(arg as *mut (Arc<TestHarness>, u64)) };
    let priority = priority as u8;
    let heavy = priority == SHARE_HEAVY_PRIORITY;
    current_thread().unwrap().set_priority(priority);

    // The second thread up opens the window, so both count over the same one.
    if h.share_started.fetch_add(1, Ordering::AcqRel) + 1 == SHARE_THREADS {
        h.share_deadline.store(
            (Instant::now() + Duration::from_millis(SHARE_WINDOW_MS)).as_nanos(),
            Ordering::Release,
        );
    }
    while h.share_deadline.load(Ordering::Acquire) == 0 {
        core::hint::spin_loop();
    }

    let deadline = h.share_deadline.load(Ordering::Acquire);
    let counter = if heavy {
        &h.share_heavy
    } else {
        &h.share_light
    };
    while Instant::now().as_nanos() < deadline {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    // One of the two checks, after both have stopped counting.
    if h.share_finished.fetch_add(1, Ordering::AcqRel) + 1 != SHARE_THREADS {
        thread_exit(0);
    }

    let heavy_count = h.share_heavy.load(Ordering::Acquire);
    let light_count = h.share_light.load(Ordering::Acquire).max(1);
    // Scaled rather than floating point: the kernel is built soft-float.
    let ratio_x100 = heavy_count.saturating_mul(100) / light_count;
    assert!(
        ratio_x100 > 300,
        "[sched-test] weighted-share: {} levels apart bought {}.{:02}x of the CPU \
         (heavy {heavy_count}, light {light_count}); 2.00x is priority as a pick order \
         rather than a share",
        SHARE_HEAVY_PRIORITY - SHARE_LIGHT_PRIORITY,
        ratio_x100 / 100,
        ratio_x100 % 100,
    );
    assert!(
        ratio_x100 < 900,
        "[sched-test] weighted-share: {}.{:02}x is past the {}x the weight table asks for \
         (heavy {heavy_count}, light {light_count}); the lower thread is being starved",
        ratio_x100 / 100,
        ratio_x100 % 100,
        4,
    );

    println!(
        "[sched-test] weighted-share: {} levels bought {}.{:02}x of the CPU \
         (heavy {heavy_count}, light {light_count}), weight table asks 4.77x",
        SHARE_HEAVY_PRIORITY - SHARE_LIGHT_PRIORITY,
        ratio_x100 / 100,
        ratio_x100 % 100,
    );
    test_done(&h, "weighted-share");
    thread_exit(0);
}
