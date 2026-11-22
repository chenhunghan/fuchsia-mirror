// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel timer diagnostics and stress tests.

use core::ffi::{c_int, c_void};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use kprint::{kformat, kprintln};
use pin_init::stack_pin_init;
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use zx_status::Status;
use zx_types::ZX_TIME_INFINITE;

use crate::arch_rs::{
    CpuAlignMarker, InterruptDisableGuard, MAX_CACHE_LINE, curr_cpu_num, max_num_cpus,
};
use crate::console::CmdArgs;
use crate::crypto::global_prng;
use crate::kernel::deadline::{Deadline, SlackMode, TimerSlack};
use crate::kernel::event::Event;
use crate::kernel::mp::{SMP_MAX_CPUS, cpu_num_to_mask, get_online_mask};
use crate::kernel::scheduler_state::SchedulerStateBaseProfile;
use crate::kernel::thread::{
    DEFAULT_PRIORITY, Interruptible, ThreadPtr, create_etc, create_with_profile,
    preempt_set_pending, sleep_relative,
};
use crate::kernel::timer::{Timer, ZX_CLOCK_BOOT, ZX_CLOCK_MONOTONIC};
use crate::kernel::types::cpu_num_t;
use crate::platform_rs::timer::{
    DurationBoot, DurationMono, DurationUnknown, InstantMono, InstantUnknown, current_mono_time,
};

/// Timer callback that signals the [`Event`] passed in `arg`.
///
/// Expects no `ChainLockTransaction` to be active on the current CPU (checked by `Event::signal`).
///
/// # Safety
///
/// The caller must ensure that `arg` is non-null, properly aligned for [`Event`], and points to
/// an initialized [`Event`] that remains live for the duration of the callback with no active
/// mutable references.
unsafe extern "C" fn timer_diag_cb(_timer: *mut Timer, _now: i64, arg: *mut c_void) {
    // SAFETY: The caller's `# Safety` contract guarantees `arg` is non-null, properly aligned,
    // and points to a live, initialized `Event` with no active mutable references for the
    // duration of this callback.
    let event = unsafe { &*arg.cast::<Event>() };
    event.signal();
}

extern "C" fn timer_do_one_thread(_arg: *mut c_void) -> c_int {
    stack_pin_init!(let event = Event::init_unsignaled());
    let event = event.into_ref();
    stack_pin_init!(let timer = Timer::init(ZX_CLOCK_MONOTONIC));
    let timer = timer.into_ref();

    let deadline = Deadline::after_mono(DurationMono::from_millis(10), TimerSlack::none());
    // SAFETY:
    // - `timer` is pinned on the stack (`Pin<&Timer>`) and canceled below before `timer` and
    //   `event` go out of scope.
    // - No `&mut Timer` or `Pin<&mut Timer>` references to `timer` exist or are used while `timer`
    //   is scheduled or executing its callback (`timer` was converted via `Pin::into_ref`).
    // - Neither `set_deadline`, `set_oneshot`, nor `cancel` is called concurrently on `timer`.
    // - `event` (`Pin<&Event>`) remains live, initialized, and pinned with no active mutable
    //   references until after `timer.cancel()` returns, satisfying `timer_diag_cb`'s
    //   `# Safety` contract.
    unsafe {
        timer.set_deadline(
            &deadline,
            timer_diag_cb,
            ptr::from_ref(event.get_ref()).cast_mut().cast(),
        );
    }
    let _ = event.wait_infinite();

    kprintln!("got timer on cpu {}", curr_cpu_num());

    // Make sure the timer has fully completed before going out of scope.
    timer.cancel();

    0
}

/// Spawns one timer test thread pinned to each CPU up to `max_num_cpus()` and joins them.
///
/// # Safety
///
/// The caller must ensure this is called from a normal, blockable kernel thread context after
/// `LK_INIT_LEVEL_THREADING`, with no spinlocks or `Thread::get_list_lock()` held and no
/// `ChainLockTransaction` active (which entails that neither scheduler queue locks nor thread
/// locks are held).
unsafe fn timer_diag_all_cpus() {
    let mut timer_threads: [Option<ThreadPtr>; SMP_MAX_CPUS] = [None; SMP_MAX_CPUS];
    let max = max_num_cpus() as usize;

    let profile = SchedulerStateBaseProfile::fair(DEFAULT_PRIORITY, true);
    for (i, slot) in timer_threads.iter_mut().take(max).enumerate() {
        let mut name = [0u8; 16];
        let name = kformat!(&mut name, "timer {}\n", i);

        // SAFETY: `timer_do_one_thread` ignores its argument and is safe to run on a new thread,
        // and the caller guarantees a blockable kernel thread context with no active
        // `ChainLockTransaction`.
        let thread =
            unsafe { create_with_profile(name, timer_do_one_thread, ptr::null_mut(), &profile) };
        debug_assert!(thread.is_ok(), "failed to create thread for cpu {}", i);
        let Ok(thread) = thread else {
            break;
        };
        // SAFETY: `thread` was just created above and is live; the caller's "no spinlocks" and
        // "no `ChainLockTransaction` active" preconditions entail that neither scheduler queue
        // locks nor thread locks are held.
        unsafe { thread.set_cpu_affinity(cpu_num_to_mask(i as cpu_num_t)) };
        // SAFETY: `thread` was just created above, is non-idle and unjoined, and the caller
        // guarantees no spinlocks are held and no `ChainLockTransaction` is active.
        unsafe { thread.resume() };
        *slot = Some(thread);
    }
    for (i, slot) in timer_threads.iter_mut().take(max).enumerate() {
        if let Some(thread) = slot.take() {
            // SAFETY: `thread` was resumed above, is not detached or the current thread, has no
            // `&Thread` references live, is joined once here, and the caller guarantees a
            // blockable context without holding `thread`'s lock, `Thread::get_list_lock()`, or
            // an active `ChainLockTransaction`.
            let status = unsafe { thread.join(InstantMono::INFINITE) }.map(|_| ());
            debug_assert!(
                status.is_ok(),
                "failed to join thread for cpu {}: {}",
                i,
                Status::result_into_raw(status)
            );
        }
    }
}

/// Timer callback that increments the [`AtomicUsize`] counter in `arg` and marks preemption
/// pending.
///
/// Expects to run in interrupt context with interrupts and preemption disabled (debug-asserted by
/// `preempt_set_pending`).
///
/// # Safety
///
/// The caller must ensure that `arg` is non-null, properly aligned for [`AtomicUsize`], and points
/// to an initialized [`AtomicUsize`] that remains live for the duration of the callback with no
/// active mutable references.
unsafe extern "C" fn timer_diag_cb2(_timer: *mut Timer, _now: i64, arg: *mut c_void) {
    // SAFETY: The caller's `# Safety` contract guarantees `arg` is non-null, properly aligned,
    // and points to a live, initialized `AtomicUsize` with no active mutable references for the
    // duration of this callback.
    let timer_count = unsafe { &*arg.cast::<AtomicUsize>() };
    timer_count.fetch_add(1, Ordering::SeqCst);
    preempt_set_pending();
}

fn timer_diag_coalescing(
    slack: TimerSlack,
    deadline: &[InstantMono],
    expected_adj: &[DurationMono],
) {
    assert_eq!(deadline.len(), expected_adj.len());

    kprintln!("testing coalescing mode {}", slack.mode() as u32);

    let timer_count = AtomicUsize::new(0);
    let Ok(timers) =
        fbl::Array::try_new_pin_init(deadline.len(), || Timer::init(ZX_CLOCK_MONOTONIC))
    else {
        kprintln!("\n!! failed to allocate {} timers", deadline.len());
        return;
    };
    // Cancel all the timers prior to going out of scope.
    let _cancel_timers = zr::defer(|| {
        for timer in timers.iter() {
            timer.cancel();
        }
    });

    kprintln!("       orig         new       adjustment");
    for ix in 0..timers.len() {
        let dl = Deadline::new(deadline[ix].into(), slack);
        // SAFETY: `timers` is `Pin<fbl::Array<Timer>>`, so its elements are structurally pinned
        // in place for the lifetime of the array, and the closure receives `&[Timer]` by shared
        // reference (via `Pin::as_ref`) without moving out of `timers`.
        let timer = unsafe { timers.as_ref().map_unchecked(|timers| &timers[ix]) };
        // SAFETY:
        // - `timers[ix]` stays pinned in place and is canceled by `_cancel_timers` before `timers`
        //   and `timer_count` go out of scope.
        // - No `&mut Timer` or `Pin<&mut Timer>` references to `timers[ix]` exist or are used while
        //   it is scheduled or executing its callback (`timers` is only shared via `Pin::as_ref`).
        // - Neither `set_deadline`, `set_oneshot`, nor `cancel` is called concurrently on
        //   `timers[ix]`.
        // - `&timer_count` (`&AtomicUsize`) is non-null, properly aligned for `AtomicUsize`,
        //   initialized, and remains live with no active mutable references until `_cancel_timers`
        //   cancels all timers before `timer_count` is dropped, and `Timer::set_deadline`
        //   guarantees `timer_diag_cb2` runs in interrupt context with interrupts and preemption
        //   disabled, satisfying `timer_diag_cb2`'s `# Safety` contract.
        unsafe {
            timer.set_deadline(&dl, timer_diag_cb2, ptr::from_ref(&timer_count).cast_mut().cast());
        }
        let (scheduled_time, slack_adj) =
            timers[ix].scheduled_time_and_slack_for_test(ZX_CLOCK_MONOTONIC);
        kprintln!("[{}] {}  -> {}, {}", ix, dl.when().0, scheduled_time.0, slack_adj.0);

        if slack_adj.0 != expected_adj[ix].0 {
            kprintln!("\n!! unexpected adjustment! expected {}", expected_adj[ix].0);
        }
    }

    // Wait for the timers to fire.
    while timer_count.load(Ordering::SeqCst) != timers.len() {
        let _ = sleep_relative(DurationMono::from_millis(5));
    }
}

fn timer_diag_coalescing_center() {
    let when = current_mono_time() + DurationMono::from_millis(1);
    let off = DurationMono::from_micros(10);
    let slack = TimerSlack::new(DurationUnknown((off * 2).0), SlackMode::Center);

    let deadline = [
        when + (off * 6), // non-coalesced, adjustment = 0
        when,             // non-coalesced, adjustment = 0
        when - off,       // coalesced with [1], adjustment = 10u
        when - (off * 3), // non-coalesced, adjustment = 0
        when + off,       // coalesced with [1], adjustment = -10u
        when + (off * 3), // non-coalesced, adjustment = 0
        when + (off * 5), // coalesced with [0], adjustment = 10u
        when - (off * 3), // non-coalesced, same as [3], adjustment = 0
    ];

    let expected_adj = [
        DurationMono::ZERO,
        DurationMono::ZERO,
        DurationMono::from_micros(10),
        DurationMono::ZERO,
        DurationMono::from_micros(-10),
        DurationMono::ZERO,
        DurationMono::from_micros(10),
        DurationMono::ZERO,
    ];

    timer_diag_coalescing(slack, &deadline, &expected_adj);
}

fn timer_diag_coalescing_late() {
    let when = current_mono_time() + DurationMono::from_millis(1);
    let off = DurationMono::from_micros(10);
    let slack = TimerSlack::new(DurationUnknown((off * 3).0), SlackMode::Late);

    let deadline = [
        when + off,       // non-coalesced, adjustment = 0
        when + (off * 2), // non-coalesced, adjustment = 0
        when - off,       // coalesced with [0], adjustment = 20u
        when - (off * 3), // non-coalesced, adjustment = 0
        when + (off * 3), // non-coalesced, adjustment = 0
        when + (off * 2), // non-coalesced, same as [1]
        when - (off * 4), // coalesced with [3], adjustment = 10u
    ];

    let expected_adj = [
        DurationMono::ZERO,
        DurationMono::ZERO,
        DurationMono::from_micros(20),
        DurationMono::ZERO,
        DurationMono::ZERO,
        DurationMono::ZERO,
        DurationMono::from_micros(10),
    ];

    timer_diag_coalescing(slack, &deadline, &expected_adj);
}

fn timer_diag_coalescing_early() {
    let when = current_mono_time() + DurationMono::from_millis(1);
    let off = DurationMono::from_micros(10);
    let slack = TimerSlack::new(DurationUnknown((off * 3).0), SlackMode::Early);

    let deadline = [
        when,             // non-coalesced, adjustment = 0
        when + (off * 2), // coalesced with [0], adjustment = -20u
        when - off,       // non-coalesced, adjustment = 0
        when - (off * 3), // non-coalesced, adjustment = 0
        when + (off * 4), // non-coalesced, adjustment = 0
        when + (off * 5), // coalesced with [4], adjustment = -10u
        when - (off * 2), // coalesced with [3], adjustment = -10u
    ];

    let expected_adj = [
        DurationMono::ZERO,
        DurationMono::from_micros(-20),
        DurationMono::ZERO,
        DurationMono::ZERO,
        DurationMono::ZERO,
        DurationMono::from_micros(-10),
        DurationMono::from_micros(-10),
    ];

    timer_diag_coalescing(slack, &deadline, &expected_adj);
}

fn timer_far_deadline() {
    stack_pin_init!(let event = Event::init_unsignaled());
    let event = event.into_ref();
    stack_pin_init!(let timer = Timer::init(ZX_CLOCK_MONOTONIC));
    let timer = timer.into_ref();

    let deadline = Deadline::no_slack(InstantUnknown(ZX_TIME_INFINITE - 5));
    // SAFETY:
    // - `timer` is pinned on the stack (`Pin<&Timer>`) and canceled below before `timer` and
    //   `event` go out of scope.
    // - No `&mut Timer` or `Pin<&mut Timer>` references to `timer` exist or are used while `timer`
    //   is scheduled or executing its callback (`timer` was converted via `Pin::into_ref`).
    // - Neither `set_deadline`, `set_oneshot`, nor `cancel` is called concurrently on `timer`.
    // - `event` (`Pin<&Event>`) remains live, initialized, and pinned with no active mutable
    //   references until after `timer.cancel()` returns, satisfying `timer_diag_cb`'s
    //   `# Safety` contract.
    unsafe {
        timer.set_deadline(
            &deadline,
            timer_diag_cb,
            ptr::from_ref(event.get_ref()).cast_mut().cast(),
        );
    }
    let st = event
        .wait_deadline(current_mono_time() + DurationMono::from_millis(100), Interruptible::No);
    if st != Err(Status::TIMED_OUT) {
        kprintln!("error: unexpected timer fired!");
    }
    timer.cancel();
}

/// Print timer diagnostics for manual review.
///
/// # Safety
///
/// The caller must ensure that this is called from a normal, blockable kernel thread context
/// after `LK_INIT_LEVEL_THREADING` with interrupts enabled, no spinlocks or
/// `Thread::get_list_lock()` held, and no `ChainLockTransaction` active (as guaranteed by
/// `CMD_AVAIL_NORMAL` console dispatch). `_argv` is not dereferenced.
pub(super) unsafe extern "C" fn timer_diag(
    _argc: c_int,
    _argv: *const CmdArgs,
    _flags: u32,
) -> c_int {
    timer_diag_coalescing_center();
    timer_diag_coalescing_late();
    timer_diag_coalescing_early();
    // SAFETY: The caller's `# Safety` contract guarantees a normal, blockable kernel thread
    // context after `LK_INIT_LEVEL_THREADING` with no spinlocks, `Thread::get_list_lock()`, or
    // `ChainLockTransaction` active.
    unsafe { timer_diag_all_cpus() };
    timer_far_deadline();
    0
}

#[repr(C)]
struct TimerStressArgs {
    _align_done: CpuAlignMarker,
    timer_stress_done: AtomicBool,
    _align_num_set: CpuAlignMarker,
    num_set: AtomicU64,
    _align_num_fired: CpuAlignMarker,
    num_fired: AtomicU64,
}

impl Default for TimerStressArgs {
    fn default() -> Self {
        Self {
            _align_done: CpuAlignMarker,
            timer_stress_done: AtomicBool::new(false),
            _align_num_set: CpuAlignMarker,
            num_set: AtomicU64::new(0),
            _align_num_fired: CpuAlignMarker,
            num_fired: AtomicU64::new(0),
        }
    }
}

zr::static_assert!(core::mem::align_of::<TimerStressArgs>() == MAX_CACHE_LINE);
zr::static_assert!(core::mem::offset_of!(TimerStressArgs, num_set) >= MAX_CACHE_LINE);
zr::static_assert!(core::mem::offset_of!(TimerStressArgs, num_fired) >= 2 * MAX_CACHE_LINE);

/// Timer callback that increments the [`AtomicU64`] counter passed in `void_arg`.
///
/// # Safety
///
/// The caller must ensure that `void_arg` is non-null, properly aligned for [`AtomicU64`], and
/// points to an initialized [`AtomicU64`] that remains live for the duration of the callback with
/// no active mutable references.
unsafe extern "C" fn timer_stress_cb(_t: *mut Timer, _now: i64, void_arg: *mut c_void) {
    // SAFETY: The caller's `# Safety` contract guarantees `void_arg` is non-null, properly
    // aligned, and points to a live, initialized `AtomicU64` with no active mutable references for
    // the duration of this callback.
    let num_fired = unsafe { &*void_arg.cast::<AtomicU64>() };
    num_fired.fetch_add(1, Ordering::SeqCst);
}

/// Returns a random duration between 0 and `max` (inclusive).
fn rand_duration(rng: &mut SmallRng, max: DurationMono) -> DurationMono {
    DurationMono(rng.random_range(0..=max.0))
}

/// Draws a 64-bit RNG seed from the kernel global PRNG.
///
/// Outlined with `#[inline(never)]` so the seed buffer does not remain in
/// `timer_stress_worker`'s stack frame and so returning `u64` avoids a 32-byte `SmallRng` `sret`
/// slot on `timer_stress_worker`'s SafeStack.
#[inline(never)]
fn make_worker_seed() -> u64 {
    let mut seed = [0u8; 8];
    global_prng::draw(&mut seed);
    u64::from_ne_bytes(seed)
}

/// Worker thread entry point that repeatedly schedules and cancels random monotonic/boot timers
/// on another CPU until `void_arg` signals completion.
///
/// # Safety
///
/// The caller must ensure that:
/// - This runs on a live, blockable worker thread after `LK_INIT_LEVEL_THREADING`, with no
///   spinlocks, thread lock, scheduler queue lock, or `ChainLockTransaction` active.
/// - `void_arg` is non-null, properly aligned for [`TimerStressArgs`], and points to an
///   initialized [`TimerStressArgs`] that remains live with no active mutable references until this
///   thread exits.
unsafe extern "C" fn timer_stress_worker(void_arg: *mut c_void) -> c_int {
    // SAFETY: The caller's `# Safety` contract guarantees `void_arg` is non-null, properly aligned,
    // and points to a live, initialized `TimerStressArgs` with no active mutable references until
    // this thread exits.
    let args = unsafe { &*void_arg.cast::<TimerStressArgs>() };
    let mut rng = SmallRng::seed_from_u64(make_worker_seed());
    while !args.timer_stress_done.load(Ordering::SeqCst) {
        // Create a timer on either the monotonic or boot timeline.
        // The timeline will be chosen randomly.
        let timer_duration = rand_duration(&mut rng, DurationMono::from_millis(5));
        let (timeline, deadline) = if rng.random_ratio(1, 2) {
            (ZX_CLOCK_MONOTONIC, Deadline::after_mono(timer_duration, TimerSlack::none()))
        } else {
            (
                ZX_CLOCK_BOOT,
                Deadline::after_boot(DurationBoot(timer_duration.0), TimerSlack::none()),
            )
        };
        stack_pin_init!(let t = Timer::init(timeline));
        let t = t.into_ref();

        // Set a timer, then switch to a different CPU to ensure we race with it.
        {
            let _block_interrupts = InterruptDisableGuard::new();
            let timer_cpu: cpu_num_t = curr_cpu_num();
            // SAFETY:
            // - `t` stays pinned (`Pin<&Timer>`) for this loop iteration and is canceled below
            //   before the iteration ends.
            // - No `&mut Timer` or `Pin<&mut Timer>` references to `t` exist or are used while `t`
            //   is scheduled or executing its callback (`t` was converted via `Pin::into_ref`).
            // - Neither `set_deadline`, `set_oneshot`, nor `cancel` is called concurrently on `t`.
            // - `timer_stress_cb` is a valid callback for `&args.num_fired` (`&AtomicU64`), which
            //   remains live, aligned, and shared with no active mutable references until
            //   `t.cancel()` returns and all worker threads are joined.
            unsafe {
                t.set_deadline(
                    &deadline,
                    timer_stress_cb,
                    ptr::from_ref(&args.num_fired).cast_mut().cast(),
                );
            }
            // SAFETY: The caller's `# Safety` contract guarantees a live worker thread after
            // `LK_INIT_LEVEL_THREADING`.
            let current = unsafe { ThreadPtr::current() };
            // SAFETY: `current` is the live current thread, and the caller's `# Safety` contract
            // guarantees no thread lock, scheduler queue lock, or `ChainLockTransaction` is active.
            unsafe {
                current.set_cpu_affinity(!cpu_num_to_mask(timer_cpu));
            }
            debug_assert!(curr_cpu_num() != timer_cpu);
        }

        // We're now running on something other than timer_cpu.

        args.num_set.fetch_add(1, Ordering::SeqCst);

        // Sleep for the timer duration so that this thread's `Timer::cancel` races with the
        // timer callback. We want to race to ensure there are no synchronization or memory
        // visibility issues. Note that we will not race if the system suspends while we sleep,
        // so we must ensure that we do not suspend.
        let _ = sleep_relative(timer_duration);
        t.cancel();
    }
    0
}

/// `timer_stress` is a simple stress test intended to flush out bugs in kernel timers.
///
/// # Safety
///
/// The caller must ensure that:
/// - This is called from a normal, blockable kernel thread context after
///   `LK_INIT_LEVEL_THREADING` with interrupts enabled, no spinlocks, `pool_lock_`, or
///   `Thread::get_list_lock()` held, and no `ChainLockTransaction` active (as guaranteed by
///   `CMD_AVAIL_NORMAL` console dispatch).
/// - `argc <= 0` or `argv` is non-null, properly aligned for [`CmdArgs`], and valid for reads of
///   `argc` consecutive initialized [`CmdArgs`] elements within a single live allocation of at
///   most `isize::MAX` bytes that is not mutated for the duration of this call.
/// - When `argc >= 1`, `argv[0].arg_str` is either null or points to a valid NUL-terminated C
///   string within a single live allocation of at most `isize::MAX` bytes that is not mutated for
///   the duration of this call.
pub(super) unsafe extern "C" fn timer_stress(
    argc: c_int,
    argv: *const CmdArgs,
    _flags: u32,
) -> c_int {
    let Ok(argc_len) = usize::try_from(argc) else {
        return Status::INTERNAL.into_raw();
    };
    if argc_len > 0 && argv.is_null() {
        return Status::INTERNAL.into_raw();
    }
    // SAFETY: If `argc_len > 0`, `argv` is non-null and the caller's `# Safety` contract guarantees
    // `argv` is properly aligned and valid for reads of `argc_len` consecutive initialized
    // `CmdArgs` elements within a single live unmutated allocation of at most `isize::MAX` bytes;
    // the `&[CmdArgs]` slice produced here does not outlive this `timer_stress` call.
    let argv = unsafe { zr::slice_from_raw_parts(argv, argc_len) };
    if argv.len() < 2 {
        kprintln!("not enough args");
        kprintln!(
            "usage: {:s} <num seconds>",
            argv.first().map_or("timer_stress", CmdArgs::as_str)
        );
        return Status::INTERNAL.into_raw();
    }

    // We need 2 or more CPUs for this test.
    if get_online_mask().count_ones() < 2 {
        kprintln!("not enough online cpus");
        return Status::INTERNAL.into_raw();
    }

    let args = &TimerStressArgs::default();

    // Heap-allocate the 256-slot thread array to avoid placing a 2 KiB array on the 8 KiB kernel
    // stack, returning `Status::NO_MEMORY` on allocation failure.
    let Ok(mut threads) = fbl::Array::<Option<ThreadPtr>>::try_new(256) else {
        kprintln!("failed to allocate thread array");
        return Status::NO_MEMORY.into_raw();
    };
    let profile = SchedulerStateBaseProfile::fair(DEFAULT_PRIORITY, true);
    for (i, slot) in threads.iter_mut().enumerate() {
        // SAFETY:
        // - `t` is null so `create_etc` allocates the `Thread`.
        // - `timer_stress`'s `# Safety` contract guarantees a normal, blockable kernel thread
        //   context after `LK_INIT_LEVEL_THREADING` (following `global_prng::EarlyBootSeed` and
        //   `global_prng::BecomeThreadSafe`) with no `ChainLockTransaction` active.
        // - `create_etc` starts `timer_stress_worker` on a newly created, blockable thread with no
        //   spinlocks, `pool_lock_`, thread lock, scheduler queue lock, or `ChainLockTransaction`
        //   active, and `args: &TimerStressArgs` guarantees `void_arg` is non-null, properly
        //   aligned, initialized, and has no active mutable references until all worker threads
        //   are joined below before `args` goes out of scope, satisfying `timer_stress_worker`'s
        //   `# Safety` contract.
        let thread = unsafe {
            create_etc(
                ptr::null_mut(),
                b"timer-stress-worker",
                Some(timer_stress_worker),
                ptr::from_ref(args).cast_mut().cast(),
                &profile,
                None,
            )
        };
        match thread {
            Ok(thread) => *slot = Some(thread),
            Err(status) => {
                kprintln!("failed to create timer stress worker {}: {}", i, status.into_raw());
                for created_slot in threads.iter_mut() {
                    if let Some(created) = created_slot.take() {
                        // SAFETY: `created` was initialized by `create_etc` above, has never been
                        // resumed or added to any wait queue, is not the current thread or
                        // referenced elsewhere, and is not used after this call.
                        unsafe { created.forget() };
                    }
                }
                return status.into_raw();
            }
        }
    }

    kprintln!("running for {} seconds", argv[1].arg_uint);
    for thread in threads.iter().flatten() {
        // SAFETY: Each `thread` was just created above and has not yet been resumed or joined, and
        // the caller guarantees no spinlocks are held and no `ChainLockTransaction` is active.
        unsafe { thread.resume() };
    }

    let secs = i64::try_from(argv[1].arg_uint).unwrap_or(i64::MAX);
    let _ = sleep_relative(DurationMono::from_seconds(secs));
    args.timer_stress_done.store(true, Ordering::SeqCst);

    for slot in threads.iter_mut() {
        if let Some(thread) = slot.take() {
            // SAFETY: Each `thread` was resumed above, is not detached or the current thread, has
            // no `&Thread` references live, is joined once here without holding `thread`'s lock or
            // `Thread::get_list_lock()`, and the caller guarantees a blockable kernel thread
            // context with no active `ChainLockTransaction`.
            let status = unsafe { thread.join(InstantMono::INFINITE) }.map(|_| ());
            // Soundness requires `assert!`: a failed `join` could leave a worker thread still
            // running and referencing the stack-allocated `args` after `timer_stress` returns and
            // drops it.
            assert!(
                status.is_ok(),
                "failed to join timer stress worker: {}",
                Status::result_into_raw(status)
            );
        }
    }

    kprintln!(
        "timer stress done; timer set {}, timer fired {}",
        args.num_set.load(Ordering::SeqCst),
        args.num_fired.load(Ordering::SeqCst)
    );
    0
}
