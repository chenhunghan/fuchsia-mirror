// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// mutex_spin_time tests
#[cfg(ktest)]
#[unittest::suite(name = "mutex_spin_time")]
mod mutex_spin_time_tests {
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicBool, Ordering};

    use crate::ksync_kernel::MutexSpinMaxDurationPolicy;
    use affine::{Ratio, Round};
    use kprint::kprintln;
    use pin_init::{pin_init, stack_pin_init};
    use unittest::{assert_ok, expect_ge, unwrap_ok};

    use crate::kconcurrent::{IRQ_SAVE_OPTION, clt_tag, single_chain_lock_guard_owned};
    use crate::kernel::auto_preempt_disabler::AutoPreemptDisabler;
    use crate::kernel::mp::get_online_mask;
    use crate::kernel::scheduler::BaseProfile;
    use crate::kernel::scheduler_state::ThreadStateKind;
    use crate::kernel::thread::{HIGH_PRIORITY, Thread, ThreadPtr, create_with_priority};
    use crate::kernel::types::cpu_mask_t;
    use crate::platform_rs::timer::{
        DurationMono, InstantMonoTicks, current_mono_time, timer_current_mono_ticks,
        timer_get_ticks_to_time_ratio, zx_sec,
    };

    /// Mutex spin timeouts
    #[test]
    fn mutex_spin_time_test() {
        // We cannot run this test unless there are at least 2 CPUs currently online.  Either find
        // two cores we can use, or just skip the test with a warning message.
        let mut timer_mask: cpu_mask_t;
        let mut spinner_mask: cpu_mask_t;
        {
            let avail_mask: cpu_mask_t = get_online_mask();
            let avail_count = avail_mask.count_ones();
            if avail_count < 2 {
                kprintln!(
                    "Insufficient cores online to run the mutex spin timeout tests.  Skipping!"
                );
                return true;
            }

            timer_mask = 0x1;
            while (timer_mask & avail_mask) == 0 {
                timer_mask <<= 1;
            }

            spinner_mask = timer_mask << 1;
            while (spinner_mask & avail_mask) == 0 {
                spinner_mask <<= 1;
            }
        }

        // No matter what happens from here on out, make sure we restore our main
        // thread's priority and cpu affinity.
        let _cleanup = zr::defer({
            // SAFETY: Kernel unit tests run after threading has been initialized.
            let current = unsafe { ThreadPtr::current() };
            // SAFETY: `current` is the currently running thread, which is alive.
            let affinity = unsafe { current.get_cpu_affinity() };
            // SAFETY: `current` is the currently running thread, which is alive.
            let bp = unsafe { current.as_ref() }.snapshot_base_profile();
            move || {
                // SAFETY: Kernel unit tests run after threading has been initialized.
                let current = unsafe { ThreadPtr::current() };
                // SAFETY: `current` is the currently running thread, which is alive.
                let _ = unsafe { current.set_cpu_affinity(affinity) };
                // SAFETY: `current` is the currently running thread, which is alive.
                unsafe { current.as_ref() }.set_base_profile(&bp);
            }
        });

        const TIMEOUTS: [DurationMono; 5] = [
            DurationMono::from_micros(0),
            DurationMono::from_micros(50),
            DurationMono::from_micros(250),
            DurationMono::from_micros(750),
            DurationMono::from_micros(5000),
        ];

        let ticks_to_time: Ratio = timer_get_ticks_to_time_ratio();

        #[ksync::guarded]
        struct Args {
            #[mutex]
            the_mutex: ksync::KMutex<crate::ksync_kernel::TrackedMutex>,
            spin_max_duration: DurationMono,
            interlock: AtomicBool,
        }
        stack_pin_init!(let args = pin_init!(Args {
            the_mutex <- ksync::KMutex::init(),
            spin_max_duration: DurationMono::ZERO,
            interlock: AtomicBool::new(false),
        }));

        /// Our test thunk is very simple.  One we are started, we disable preemption
        /// and then signal the timer thread via the interlock atomic.  Once the timer
        /// thread has ack'ed our signal, we just grab and release the test mutex with
        /// the specified spin timeout.
        extern "C" fn thunk(ctx: *mut c_void) -> i32 {
            // SAFETY: `ctx` points to the pinned `args` of `mutex_spin_time_test`, which joins this
            // thread before `args` goes out of scope, and does not write `spin_max_duration` while
            // this thread runs. Every other field is only accessed through shared references.
            let args = unsafe { &*ctx.cast::<Args>() };

            let _ap_disabler = AutoPreemptDisabler::new();
            args.interlock.store(true, Ordering::SeqCst);
            while args.interlock.load(Ordering::SeqCst) {
                libarch::r#yield();
            }

            ksync::lock!(
                let _guard = args.lock_the_mutex_policy_with::<MutexSpinMaxDurationPolicy>(
                    args.spin_max_duration.into_nanos()
                )
            );
            0
        }

        // Boost our thread priority and lock ourselves down to a specific CPU before
        // starting the test.
        // SAFETY: Kernel unit tests run after threading has been initialized.
        let current = unsafe { ThreadPtr::current() };
        // SAFETY: `current` is the currently running thread, which is alive.
        let _ = unsafe { current.set_cpu_affinity(timer_mask) };
        // SAFETY: `current` is the currently running thread, which is alive.
        unsafe { current.as_ref() }.set_base_profile(&BaseProfile::new_fair(HIGH_PRIORITY));

        for timeout in TIMEOUTS {
            let start: InstantMonoTicks;
            let end: InstantMonoTicks;

            // Setup the timeout and create the test thread (but don't start it yet).
            // Make sure that the thread runs on a different core from ours.
            *args.as_mut().project().spin_max_duration = timeout;
            // `thunk` only forms a shared reference from this pointer, so the read-only provenance
            // of `get_ref` suffices.
            let args_ptr: *mut c_void =
                core::ptr::from_ref(args.as_ref().get_ref()).cast_mut().cast();
            // SAFETY: `thunk` is a valid entry point, and `args` outlives the thread, which is
            // joined below.
            let test_thread = unwrap_ok!(
                unsafe {
                    create_with_priority(
                        c"mutex spin timeout".as_ptr(),
                        thunk,
                        args_ptr,
                        HIGH_PRIORITY,
                    )
                },
                "Failed to create test thread"
            );
            // SAFETY: `test_thread` was just created and has not been joined.
            let _ = unsafe { test_thread.set_cpu_affinity(spinner_mask) };

            // Hold onto the mutex while we create a thread and time how long it takes for the mutex
            // to become blocked.
            {
                let _ap_disabler = AutoPreemptDisabler::new();
                ksync::lock!(let _guard = args.lock_the_mutex());
                // SAFETY: `test_thread` was created above and has not been resumed or joined.
                unsafe { test_thread.resume() };

                // Wait until the spinner thread is ready to go, then mark the start time
                // and tell the spinner it is OK to proceed.
                while !args.interlock.load(Ordering::SeqCst) {
                    libarch::r#yield();
                }
                start = timer_current_mono_ticks();
                args.interlock.store(false, Ordering::SeqCst);

                // Spin until we notice that the thread is blocked.
                let get_thread_state = |t: &Thread| -> ThreadStateKind {
                    single_chain_lock_guard_owned!(
                        guard,
                        t,
                        IRQ_SAVE_OPTION,
                        clt_tag!("mutex_spin_time_test")
                    );
                    guard.proof().state()
                };

                // SAFETY: `test_thread` has not been joined, so it is alive.
                while get_thread_state(unsafe { test_thread.as_ref() }) != ThreadStateKind::Blocked
                {
                    libarch::r#yield();
                }

                end = timer_current_mono_ticks();
            }

            // Now that we are out of the lock, clean up the test thread and check our
            // timing.  We should have spun for at _least_ the time specified.  For the
            // benefit of a human test runner/observer, also print out how much over the
            // limit we ended up.  There is technically no upper bound to this number,
            // but we would like to observe the overshoot amount as being "reasonable"
            // in an unloaded manual test environment.
            // SAFETY: `test_thread` was created above and has not been joined yet.
            let status = unsafe { test_thread.join(current_mono_time() + zx_sec(30)) }.map(|_| ());
            assert_ok!(status, "test thread failed to exit!");

            let actual_spin_time =
                DurationMono(ticks_to_time.scale::<{ Round::DOWN }>(end.0 - start.0));
            expect_ge!(
                actual_spin_time.into_nanos(),
                timeout.into_nanos(),
                "Didn't spin for long enough!"
            );

            if timeout.into_nanos() > 0 {
                let overshoot: i64 =
                    ((actual_spin_time - timeout).into_nanos() * 10000) / timeout.into_nanos();
                kprintln!(
                    "Target {:7} nSec, Actual {:7} nSec.  Overshot by {}.{:02}%.",
                    timeout.into_nanos(),
                    actual_spin_time.into_nanos(),
                    overshoot / 100,
                    overshoot % 100
                );
            } else {
                kprintln!(
                    "\nTarget {:7} nSec, Actual {:7} nSec.",
                    timeout.into_nanos(),
                    actual_spin_time.into_nanos()
                );
            }
        }
    }
}
