// Copyright 2016, 2018 The Fuchsia Authors
// Copyright (c) 2008-2015 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// These tests take much longer than, and are more intensive on the scheduler than,
// the //zircon/kernel/kernel/thread.cc tests.

/// intensive thread tests
#[unittest::suite(name = "intensive_thread")]
mod intensive_thread_tests {
    use crate::arch_rs::{curr_cpu_num, ints_disabled};
    use crate::kernel::auto_preempt_disabler::AutoPreemptDisabler;
    use crate::kernel::event::{AutounsignalEvent, Event};
    use crate::kernel::mp::is_cpu_online;
    use crate::kernel::mutex::Mutex;
    use crate::kernel::scheduler::{
        BaseProfile, SchedDeadlineParams, SchedDiscipline, SchedulerState, peek_active_mask,
        sched_ms,
    };
    use crate::kernel::scheduler_state::{DEFAULT_PRIORITY, HIGH_PRIORITY, LOW_PRIORITY};
    use crate::kernel::spinlock::SpinLock;
    use crate::kernel::thread::{
        Interruptible, ThreadPtr, create_with_priority, current_get, reschedule,
        sleep_interruptible, sleep_relative, r#yield,
    };
    use crate::kernel::types::{
        INVALID_CPU, cpu_mask_t, cpu_num_t, cpu_num_to_mask, highest_cpu_set,
    };
    use crate::platform_rs::timer::{DurationMono, InstantMono, current_mono_time};
    use crate::top::debug::spin;
    use core::cell::UnsafeCell;
    use core::ffi::c_void;
    use core::pin::Pin;
    use core::ptr;
    use core::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
    use kprint::kprintln;
    use ksync::LockPolicy;
    use lazy_init::LazyInit;
    use pin_init::{pin_data, pin_init, stack_pin_init};
    use rand::rngs::SmallRng;
    use rand::{RngExt, SeedableRng};
    use unittest::{
        assert_false, assert_true, expect_eq, expect_false, expect_gt, expect_ok, expect_true,
        unwrap_ok,
    };
    use zx_status::Status;
    const ZX_ERR_INTERNAL_INTR_KILLED: Status =
        Status::err_from_raw(zx_types::ZX_ERR_INTERNAL_INTR_KILLED);

    unsafe extern "C" {
        fn cpp_global_prng_draw(buffer: *mut u8, len: usize);
    }

    /// Returns a private PRNG seeded from the kernel's global PRNG.
    ///
    /// Each worker thread owns its own `SmallRng` so the threads do not synchronize with each
    /// other through shared PRNG state.
    fn new_rng() -> SmallRng {
        let mut seed = [0u8; 32];
        // SAFETY: `cpp_global_prng_draw` writes `seed.len()` bytes into `seed`.
        unsafe {
            cpp_global_prng_draw(seed.as_mut_ptr(), seed.len());
        }
        SmallRng::from_seed(seed)
    }

    extern "C" fn mutex_thread(arg: *mut c_void) -> i32 {
        const ITERATIONS: i32 = 100000;

        /// Address of the thread currently holding `m`, or 0 if `m` is not held.
        ///
        /// `AtomicUsize` with `Ordering::Relaxed` avoids a data race if mutex exclusion ever
        /// fails; mutex `m` provides the happens-before synchronization.
        static SHARED: AtomicUsize = AtomicUsize::new(0);

        // SAFETY: `arg` points to `m` on `mutex_test`'s stack, which outlives all worker threads.
        let m = unsafe { &*arg.cast::<Mutex>() };
        let mut rng = new_rng();

        for _i in 0..ITERATIONS {
            // SAFETY: `m` is initialized, we are in a blockable thread context and do not already
            // hold `m`, and we release `m` below on the same thread.
            unsafe { <Mutex as LockPolicy<Mutex>>::acquire(m, &mut (), ()) };

            if SHARED.load(Ordering::Relaxed) != 0 {
                panic!("someone else has messed with the shared data");
            }

            SHARED.store(current_get().addr(), Ordering::Relaxed);
            if rng.random_ratio(1, 5) {
                r#yield();
            }

            SHARED.store(0, Ordering::Relaxed);

            // SAFETY: The current thread acquired `m` above.
            unsafe { <Mutex as LockPolicy<Mutex>>::release(m, &mut ()) };
            if rng.random_ratio(1, 5) {
                r#yield();
            }
        }

        0
    }

    /// Stress-tests mutex acquisition and release across 5 concurrent threads.
    #[test]
    fn mutex_test() {
        let m = Mutex::new();
        let m_ptr: *mut c_void = ptr::from_ref(&m).cast_mut().cast();

        let mut threads = [None; 5];

        for thread in &mut threads {
            // SAFETY: `mutex_thread` is a valid thread entry point and `m` outlives all threads
            // joined below.
            let t = unwrap_ok!(unsafe {
                create_with_priority(
                    c"mutex tester".as_ptr(),
                    mutex_thread,
                    m_ptr,
                    DEFAULT_PRIORITY,
                )
            });
            // SAFETY: `t` was just created and has not been resumed or joined yet.
            unsafe { t.resume() };
            *thread = Some(t);
        }

        for thread in threads.into_iter().flatten() {
            // SAFETY: `thread` is valid and has not been joined yet.
            let _ = unsafe { thread.join(InstantMono::INFINITE) };
        }

        let _ = sleep_relative(DurationMono::from_millis(100));
    }

    /// Stress-tests mutex priority inheritance across multiple threads and mutexes.
    #[test]
    fn mutex_inherit_test() {
        const INHERIT_TEST_MUTEX_COUNT: usize = 4;
        const INHERIT_TEST_THREAD_COUNT: usize = 5;

        {
            // Explicit scope to control when the destruction of |args| happens
            // working variables to pass the working thread
            #[pin_data]
            struct Args {
                #[pin]
                test_blocker: Event,
                test_mutex: [Mutex; INHERIT_TEST_MUTEX_COUNT],
            }
            stack_pin_init!(let args = pin_init!(Args {
                test_blocker <- Event::init_unsignaled(),
                test_mutex: [const { Mutex::new() }; INHERIT_TEST_MUTEX_COUNT],
            }));

            // worker thread to stress the priority inheritance mechanism
            extern "C" fn inherit_worker(arg: *mut c_void) -> i32 {
                // SAFETY: `arg` points to `args` on `mutex_inherit_test`'s stack, which outlives
                // all worker threads.
                let args = unsafe { &*arg.cast::<Args>() };
                let mut rng = new_rng();

                for _count in 0..10000 {
                    let r = rng.random_range(1..=INHERIT_TEST_MUTEX_COUNT);

                    // pick a random priority
                    // SAFETY: `current_get()` returns a valid pointer to the current `Thread`.
                    unsafe { &*current_get() }.set_base_profile(&BaseProfile::new_fair(
                        rng.random_range(DEFAULT_PRIORITY - 4..=DEFAULT_PRIORITY + 4),
                    ));

                    // grab a random number of mutexes
                    for mutex in args.test_mutex.iter().take(r) {
                        // SAFETY: We acquire `test_mutex[0..r]` in ascending order in a blockable
                        // thread context and release them in reverse order below.
                        unsafe { <Mutex as LockPolicy<Mutex>>::acquire(mutex, &mut (), ()) };
                    }

                    // wait on an event for a period of time, to try to have other grabber threads
                    // need to tweak our priority in either one of the mutexes we hold or the
                    // blocking event
                    let delay_usec: i64 = rng.random_range(0..10);
                    let _ = args.test_blocker.wait_deadline(
                        current_mono_time() + DurationMono::from_micros(delay_usec),
                        Interruptible::Yes,
                    );

                    // release in reverse order
                    for mutex in args.test_mutex.iter().take(r).rev() {
                        // SAFETY: The current thread acquired `mutex` in the loop above.
                        unsafe { <Mutex as LockPolicy<Mutex>>::release(mutex, &mut ()) };
                    }
                }

                0
            }

            // create a stack of mutexes and a few threads
            let args_ptr: *mut c_void = ptr::from_ref(&*args).cast_mut().cast();
            let mut test_thread = [None; INHERIT_TEST_THREAD_COUNT];
            for t in &mut test_thread {
                // SAFETY: `inherit_worker` is a valid thread entry point and `args` outlives all
                // threads joined below.
                let thread = unwrap_ok!(unsafe {
                    create_with_priority(
                        c"mutex tester".as_ptr(),
                        inherit_worker,
                        args_ptr,
                        DEFAULT_PRIORITY,
                    )
                });
                // SAFETY: `thread` was just created and has not been resumed or joined yet.
                unsafe { thread.resume() };
                *t = Some(thread);
            }

            for t in test_thread.into_iter().flatten() {
                // SAFETY: `t` is valid and has not been joined yet.
                let _ = unsafe { t.join(InstantMono::INFINITE) };
            }
        }

        let _ = sleep_relative(DurationMono::from_millis(100));
    }

    extern "C" fn event_signaler(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` points to a valid `Event` on `event_test`'s stack, which outlives this
        // thread.
        let event = unsafe { &*arg.cast::<Event>() };

        // event signaler pausing
        let _ = sleep_relative(DurationMono::from_seconds(1));

        event.signal();
        r#yield();

        0
    }

    #[derive(Clone, Copy)]
    struct WaiterArgs {
        event: *const Event,
        count: usize,
    }

    extern "C" fn event_waiter(arg: *mut c_void) -> i32 {
        // Copy our arguments here so we can mutate the count.
        // SAFETY: `arg` points to a valid `WaiterArgs` on `event_test`'s stack, which outlives
        // this thread.
        let mut args: WaiterArgs = unsafe { *arg.cast::<WaiterArgs>() };

        while args.count > 0 {
            // SAFETY: `args.event` points to a valid `Event` on `event_test`'s stack, which
            // outlives this thread.
            let status =
                unsafe { (*args.event).wait_deadline(InstantMono::INFINITE, Interruptible::Yes) };
            if status.is_err() {
                return -1;
            }
            r#yield();
            args.count -= 1;
        }

        0
    }

    /// Tests `Event` (broadcast wakeup) and `AutounsignalEvent` (single-waiter wakeup).
    #[test]
    fn event_test() {
        let mut threads: [ThreadPtr; 5];

        {
            // make sure signaling the event wakes up all the threads and stays signaled
            stack_pin_init!(let event = Event::init_unsignaled());
            let event_ptr: *const Event = &*event;
            let mut args = WaiterArgs { event: event_ptr, count: 2 };
            let args_ptr: *mut c_void = (&raw mut args).cast();

            // SAFETY: `event_signaler` and `event_waiter` are valid entry points, and `event` and
            // `args` outlive all 5 threads (joined before the end of this block).
            threads = unsafe {
                [
                    unwrap_ok!(create_with_priority(
                        c"event signaler".as_ptr(),
                        event_signaler,
                        event_ptr.cast_mut().cast(),
                        DEFAULT_PRIORITY,
                    )),
                    unwrap_ok!(create_with_priority(
                        c"event waiter 0".as_ptr(),
                        event_waiter,
                        args_ptr,
                        DEFAULT_PRIORITY,
                    )),
                    unwrap_ok!(create_with_priority(
                        c"event waiter 1".as_ptr(),
                        event_waiter,
                        args_ptr,
                        DEFAULT_PRIORITY,
                    )),
                    unwrap_ok!(create_with_priority(
                        c"event waiter 2".as_ptr(),
                        event_waiter,
                        args_ptr,
                        DEFAULT_PRIORITY,
                    )),
                    unwrap_ok!(create_with_priority(
                        c"event waiter 3".as_ptr(),
                        event_waiter,
                        args_ptr,
                        DEFAULT_PRIORITY,
                    )),
                ]
            };

            for thread in &threads {
                // SAFETY: `thread` was just created and has not been resumed or joined yet.
                unsafe { thread.resume() };
            }

            for thread in &threads {
                // SAFETY: `thread` is valid and has not been joined yet.
                let _ = unsafe { thread.join(InstantMono::INFINITE) };
            }

            let _ = sleep_relative(DurationMono::from_seconds(2));
            // destroying event by going out of scope
        }

        {
            stack_pin_init!(let event = AutounsignalEvent::init_unsignaled());
            let event_ptr: *const Event = event.as_event();
            let mut args = WaiterArgs { event: event_ptr, count: 99 };
            let args_ptr: *mut c_void = (&raw mut args).cast();
            // make sure signaling the event wakes up precisely one thread
            // SAFETY: `event_signaler` and `event_waiter` are valid entry points, and `event` and
            // `args` outlive all 5 threads (joined before the end of this block).
            threads = unsafe {
                [
                    unwrap_ok!(create_with_priority(
                        c"event signaler".as_ptr(),
                        event_signaler,
                        event_ptr.cast_mut().cast(),
                        DEFAULT_PRIORITY,
                    )),
                    unwrap_ok!(create_with_priority(
                        c"event waiter 0".as_ptr(),
                        event_waiter,
                        args_ptr,
                        DEFAULT_PRIORITY,
                    )),
                    unwrap_ok!(create_with_priority(
                        c"event waiter 1".as_ptr(),
                        event_waiter,
                        args_ptr,
                        DEFAULT_PRIORITY,
                    )),
                    unwrap_ok!(create_with_priority(
                        c"event waiter 2".as_ptr(),
                        event_waiter,
                        args_ptr,
                        DEFAULT_PRIORITY,
                    )),
                    unwrap_ok!(create_with_priority(
                        c"event waiter 3".as_ptr(),
                        event_waiter,
                        args_ptr,
                        DEFAULT_PRIORITY,
                    )),
                ]
            };

            for thread in &threads {
                // SAFETY: `thread` was just created and has not been resumed or joined yet.
                unsafe { thread.resume() };
            }

            let _ = sleep_relative(DurationMono::from_seconds(2));

            for thread in &threads {
                // SAFETY: `thread` is valid and has not been joined yet.
                unsafe {
                    thread.kill();
                    let _ = thread.join(InstantMono::INFINITE);
                }
            }
        }
    }

    static CONTEXT_SWITCH_EVENT: LazyInit<Event> = LazyInit::uninit();
    static CONTEXT_SWITCH_DONE_EVENT: LazyInit<Event> = LazyInit::uninit();

    fn init_context_switch_events(_level: init::LkInitLevel) {
        // SAFETY: Single-threaded initialization during early boot before any test runs.
        unsafe {
            let _ = Pin::static_ref(&CONTEXT_SWITCH_EVENT).init_pin(Event::init_unsignaled());
            let _ = Pin::static_ref(&CONTEXT_SWITCH_DONE_EVENT).init_pin(Event::init_unsignaled());
        }
    }

    init::lk_init_hook!(
        context_switch_events_init,
        init_context_switch_events,
        init::LK_INIT_LEVEL_EARLIEST
    );

    extern "C" fn context_switch_tester(_arg: *mut c_void) -> i32 {
        const ITER: i32 = 100000;

        let _ = CONTEXT_SWITCH_EVENT.wait_infinite();

        for _i in 0..ITER {
            r#yield();
        }
        let _ = sleep_relative(DurationMono::from_seconds(1));

        CONTEXT_SWITCH_DONE_EVENT.signal();

        0
    }

    /// Tests context switching across 1, 2, and 4 concurrent yielding threads.
    #[test]
    fn context_switch_test() {
        // SAFETY: `context_switch_tester` is a valid thread entry point that ignores its argument,
        // and the newly created thread has not been resumed, detached, or joined yet.
        unsafe {
            let _ = unwrap_ok!(create_with_priority(
                c"context switch idle".as_ptr(),
                context_switch_tester,
                ptr::without_provenance_mut(1),
                DEFAULT_PRIORITY,
            ))
            .detach_and_resume();
        }
        let _ = sleep_relative(DurationMono::from_millis(100));
        CONTEXT_SWITCH_EVENT.signal();
        let _ = CONTEXT_SWITCH_DONE_EVENT.wait_infinite();
        let _ = sleep_relative(DurationMono::from_millis(100));

        let _ = CONTEXT_SWITCH_EVENT.unsignal();
        let _ = CONTEXT_SWITCH_DONE_EVENT.unsignal();
        // SAFETY: `context_switch_tester` is a valid thread entry point and each created thread is
        // detached and resumed once.
        unsafe {
            let _ = unwrap_ok!(create_with_priority(
                c"context switch 2a".as_ptr(),
                context_switch_tester,
                ptr::without_provenance_mut(2),
                DEFAULT_PRIORITY,
            ))
            .detach_and_resume();
            let _ = unwrap_ok!(create_with_priority(
                c"context switch 2b".as_ptr(),
                context_switch_tester,
                ptr::without_provenance_mut(2),
                DEFAULT_PRIORITY,
            ))
            .detach_and_resume();
        }
        let _ = sleep_relative(DurationMono::from_millis(100));
        CONTEXT_SWITCH_EVENT.signal();
        let _ = CONTEXT_SWITCH_DONE_EVENT.wait_infinite();
        let _ = sleep_relative(DurationMono::from_millis(100));

        let _ = CONTEXT_SWITCH_EVENT.unsignal();
        let _ = CONTEXT_SWITCH_DONE_EVENT.unsignal();
        // SAFETY: `context_switch_tester` is a valid thread entry point and each created thread is
        // detached and resumed once.
        unsafe {
            let _ = unwrap_ok!(create_with_priority(
                c"context switch 4a".as_ptr(),
                context_switch_tester,
                ptr::without_provenance_mut(4),
                DEFAULT_PRIORITY,
            ))
            .detach_and_resume();
            let _ = unwrap_ok!(create_with_priority(
                c"context switch 4b".as_ptr(),
                context_switch_tester,
                ptr::without_provenance_mut(4),
                DEFAULT_PRIORITY,
            ))
            .detach_and_resume();
            let _ = unwrap_ok!(create_with_priority(
                c"context switch 4c".as_ptr(),
                context_switch_tester,
                ptr::without_provenance_mut(4),
                DEFAULT_PRIORITY,
            ))
            .detach_and_resume();
            let _ = unwrap_ok!(create_with_priority(
                c"context switch 4d".as_ptr(),
                context_switch_tester,
                ptr::without_provenance_mut(4),
                DEFAULT_PRIORITY,
            ))
            .detach_and_resume();
        }
        let _ = sleep_relative(DurationMono::from_millis(100));
        CONTEXT_SWITCH_EVENT.signal();
        let _ = CONTEXT_SWITCH_DONE_EVENT.wait_infinite();
        let _ = sleep_relative(DurationMono::from_millis(100));
    }

    static ATOMIC_VAR: AtomicI32 = AtomicI32::new(0);
    static ATOMIC_COUNT: AtomicI32 = AtomicI32::new(0);

    extern "C" fn atomic_tester(arg: *mut c_void) -> i32 {
        let add = (arg.addr() as isize) as i32;

        const ITER: i32 = 10000000;

        for _i in 0..ITER {
            let _ = ATOMIC_VAR.fetch_add(add, Ordering::SeqCst);
        }

        let _ = ATOMIC_COUNT.fetch_sub(1, Ordering::SeqCst);

        0
    }

    /// Stress-tests concurrent atomic `fetch_add` and `fetch_sub` across 8 threads.
    #[test]
    fn atomic_test() {
        ATOMIC_VAR.store(0, Ordering::SeqCst);
        ATOMIC_COUNT.store(8, Ordering::SeqCst);

        // SAFETY: `atomic_tester` is a valid thread entry point and interprets its `arg` as an
        // integer delta without dereferencing it.
        let threads = unsafe {
            [
                unwrap_ok!(create_with_priority(
                    c"atomic tester 1".as_ptr(),
                    atomic_tester,
                    ptr::without_provenance_mut(1),
                    LOW_PRIORITY,
                )),
                unwrap_ok!(create_with_priority(
                    c"atomic tester 1".as_ptr(),
                    atomic_tester,
                    ptr::without_provenance_mut(1),
                    LOW_PRIORITY,
                )),
                unwrap_ok!(create_with_priority(
                    c"atomic tester 1".as_ptr(),
                    atomic_tester,
                    ptr::without_provenance_mut(1),
                    LOW_PRIORITY,
                )),
                unwrap_ok!(create_with_priority(
                    c"atomic tester 1".as_ptr(),
                    atomic_tester,
                    ptr::without_provenance_mut(1),
                    LOW_PRIORITY,
                )),
                unwrap_ok!(create_with_priority(
                    c"atomic tester 2".as_ptr(),
                    atomic_tester,
                    ptr::without_provenance_mut(-1isize as usize),
                    LOW_PRIORITY,
                )),
                unwrap_ok!(create_with_priority(
                    c"atomic tester 2".as_ptr(),
                    atomic_tester,
                    ptr::without_provenance_mut(-1isize as usize),
                    LOW_PRIORITY,
                )),
                unwrap_ok!(create_with_priority(
                    c"atomic tester 2".as_ptr(),
                    atomic_tester,
                    ptr::without_provenance_mut(-1isize as usize),
                    LOW_PRIORITY,
                )),
                unwrap_ok!(create_with_priority(
                    c"atomic tester 2".as_ptr(),
                    atomic_tester,
                    ptr::without_provenance_mut(-1isize as usize),
                    LOW_PRIORITY,
                )),
            ]
        };

        // start all the threads
        for thread in &threads {
            // SAFETY: `thread` was just created and has not been resumed or joined yet.
            unsafe { thread.resume() };
        }

        // wait for them to all stop
        for thread in &threads {
            // SAFETY: `thread` is valid and has not been joined yet.
            let _ = unsafe { thread.join(InstantMono::INFINITE) };
        }
    }

    static PREEMPT_COUNT: AtomicI32 = AtomicI32::new(0);

    extern "C" fn preempt_tester(_arg: *mut c_void) -> i32 {
        spin(1000000);

        let _ = PREEMPT_COUNT.fetch_sub(1, Ordering::SeqCst);

        0
    }

    /// Tests timer preemption across 5 CPU-spinning threads.
    #[test]
    fn preempt_test() {
        // create 5 threads, let them run. If the system is properly timer preempting,
        // the threads should interleave each other at a fine enough granularity so
        // that they complete at roughly the same time.

        PREEMPT_COUNT.store(5, Ordering::SeqCst);

        for _i in 0..PREEMPT_COUNT.load(Ordering::SeqCst) {
            // SAFETY: `preempt_tester` is a valid thread entry point that ignores its argument,
            // and the newly created thread has not been resumed, detached, or joined yet.
            unsafe {
                let _ = unwrap_ok!(create_with_priority(
                    c"preempt tester".as_ptr(),
                    preempt_tester,
                    ptr::null_mut(),
                    LOW_PRIORITY,
                ))
                .detach_and_resume();
            }
        }

        while PREEMPT_COUNT.load(Ordering::SeqCst) > 0 {
            let _ = sleep_relative(DurationMono::from_seconds(1));
        }
    }

    extern "C" fn join_tester(arg: *mut c_void) -> i32 {
        let val = arg.addr() as i32;

        let _ = sleep_relative(DurationMono::from_millis(500));

        val
    }

    extern "C" fn join_tester_server(_arg: *mut c_void) -> i32 {
        // SAFETY: `join_tester` is a valid thread entry point that interprets its argument as an
        // integer value. Each created thread is either joined exactly once (sub-cases 1 and 2) or
        // detached exactly once (sub-cases 3 and 4), and is not used after it is joined or after
        // it is detached once it may have exited: in sub-case 3 the thread is detached while still
        // in its initial state, so it cannot exit and be freed until after `resume()`; in sub-case
        // 4 `detach()` frees the already-exited thread, and `t` is not used afterwards.
        unsafe {
            let t = create_with_priority(
                c"join tester".as_ptr(),
                join_tester,
                ptr::without_provenance_mut(1),
                DEFAULT_PRIORITY,
            )
            .unwrap();
            t.resume();
            t.as_ref().assert_canary();
            let res = t.join(InstantMono::INFINITE);
            assert!(res.is_ok());

            let t = create_with_priority(
                c"join tester".as_ptr(),
                join_tester,
                ptr::without_provenance_mut(2),
                DEFAULT_PRIORITY,
            )
            .unwrap();
            t.resume();
            // wait until thread is already dead
            let _ = sleep_relative(DurationMono::from_seconds(1));
            t.as_ref().assert_canary();
            let res = t.join(InstantMono::INFINITE);
            assert!(res.is_ok());

            // creating a thread, detaching it, let it exit on its own
            let t = create_with_priority(
                c"join tester".as_ptr(),
                join_tester,
                ptr::without_provenance_mut(3),
                DEFAULT_PRIORITY,
            )
            .unwrap();
            let _ = t.detach();
            t.resume();
            // wait until the thread should be dead
            let _ = sleep_relative(DurationMono::from_seconds(1));

            // creating a thread, detaching it after it should be dead
            let t = create_with_priority(
                c"join tester".as_ptr(),
                join_tester,
                ptr::without_provenance_mut(4),
                DEFAULT_PRIORITY,
            )
            .unwrap();
            t.resume();
            // wait until thread is already dead
            let _ = sleep_relative(DurationMono::from_seconds(1));
            t.as_ref().assert_canary();
            let _ = t.detach();
        }

        // exiting join tester server

        55
    }

    /// Tests joining and detaching threads before and after thread exit.
    #[test]
    fn join_test() {
        // SAFETY: `join_tester_server` is a valid thread entry point that ignores its argument,
        // and `t` is valid until joined.
        unsafe {
            let t = unwrap_ok!(create_with_priority(
                c"join tester server".as_ptr(),
                join_tester_server,
                ptr::without_provenance_mut(1),
                DEFAULT_PRIORITY,
            ));
            t.resume();
            let _ = unwrap_ok!(t.join(InstantMono::INFINITE));
        }
    }

    struct LockPair {
        first: SpinLock,
        second: SpinLock,
    }

    /// Acquires lock on "second" and holds it until it sees that "first" is released.
    extern "C" fn hold_and_release(arg: *mut c_void) -> i32 {
        assert!(!arg.is_null());
        // SAFETY: `arg` points to `pair` on `spinlock_test`'s stack, which outlives this thread.
        let pair = unsafe { &*arg.cast::<LockPair>() };
        // SAFETY: `pair.second` is initialized, not already held by this CPU, and released with
        // the saved state below before returning.
        unsafe {
            let state = pair.second.acquire_irq_save();
            while pair.first.holder_cpu() != INVALID_CPU {
                libarch::r#yield();
            }
            pair.second.release_irq_restore(state);
        }
        0
    }

    /// Tests single-core and cross-CPU spinlock acquisition, `is_held`, and `holder_cpu`.
    #[test]
    fn spinlock_test() {
        let lock = SpinLock::new();

        // Verify basic functionality (single core).

        // Note that it is invalid to call lock.is_held() with interrupts enabled.

        assert_false!(ints_disabled());
        // SAFETY: `lock` is initialized, not held by this CPU, and released immediately below.
        unsafe {
            let state = lock.acquire_irq_save();
            expect_true!(ints_disabled());
            expect_true!(lock.is_held());
            expect_eq!(curr_cpu_num(), lock.holder_cpu());
            lock.release_irq_restore(state);
        }
        assert_false!(ints_disabled());

        // Verify slightly more advanced functionality that requires multiple cores.
        let active: cpu_mask_t = peek_active_mask();
        if active == 0 || active.is_power_of_two() {
            kprintln!("skipping rest of spinlock_test, not enough active cpus");

            return true;
        }

        let pair = LockPair { first: SpinLock::new(), second: SpinLock::new() };
        let pair_ptr: *mut c_void = ptr::from_ref(&pair).cast_mut().cast();
        // SAFETY: `hold_and_release` is a valid thread entry point and `pair` outlives
        // `holder_thread` (joined below).
        let holder_thread = unwrap_ok!(unsafe {
            create_with_priority(
                c"hold_and_release".as_ptr(),
                hold_and_release,
                pair_ptr,
                DEFAULT_PRIORITY,
            )
        });

        {
            // Disable preemption for the duration the we hold the spinlock to ensure
            // we do not trigger a local reschedule as it would be an error to do so
            // while holding a spinlock.
            let _preempt_disable = AutoPreemptDisabler::new();

            // Acquire the lock before resuming the thread.
            // SAFETY: `pair.first` is initialized, not held by this CPU, and released at the end
            // of this block; `holder_thread` is valid and has not been resumed or joined yet.
            unsafe {
                let state = pair.first.acquire_irq_save();

                // Right now we have suspended IRQs and so we will not be moved off this cpu. To
                // prevent any poor decisions by the scheduler that could cause deadlock we set the
                // affinity of the holder_thread to not include our cpu.
                let _ = holder_thread.set_cpu_affinity(active ^ cpu_num_to_mask(curr_cpu_num()));
                holder_thread.resume();
                while pair.second.holder_cpu() == INVALID_CPU {
                    libarch::r#yield();
                }

                // See that from our perspective "second" is not held.
                expect_false!(pair.second.is_held());
                pair.first.release_irq_restore(state);
            }
        }
        // SAFETY: `holder_thread` is valid and has not been joined yet.
        let _ = unsafe { holder_thread.join(InstantMono::INFINITE) };
    }

    extern "C" fn sleeper_kill_thread_infinite_wait(_arg: *mut c_void) -> i32 {
        let _ = sleep_relative(DurationMono::from_millis(100));

        kprintln!("sleeper_kill_thread_infinite_wait: waiting until killed");
        let err = sleep_interruptible(InstantMono::INFINITE);
        assert!(err == Err(ZX_ERR_INTERNAL_INTR_KILLED));

        0
    }

    extern "C" fn waiter_kill_thread_infinite_wait(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` points to a valid `Event` on `kill_test`'s stack, which outlives this
        // thread.
        let e = unsafe { &*arg.cast::<Event>() };

        let _ = sleep_relative(DurationMono::from_millis(100));

        kprintln!("waiter_kill_thread_infinite_wait: waiting until killed");
        let err = e.wait_deadline(InstantMono::INFINITE, Interruptible::Yes);
        assert!(err == Err(ZX_ERR_INTERNAL_INTR_KILLED));

        0
    }

    /// Tests killing threads blocked in interruptible sleep or event wait across lifecycle stages.
    #[test]
    fn kill_test() {
        // Starting sleeper thread, then killing it while it sleeps.
        // SAFETY: `sleeper_kill_thread_infinite_wait` is a valid thread entry point that ignores
        // its argument.
        let t = unwrap_ok!(unsafe {
            create_with_priority(
                c"sleeper".as_ptr(),
                sleeper_kill_thread_infinite_wait,
                ptr::null_mut(),
                LOW_PRIORITY,
            )
        });
        // SAFETY: `t` was just created and is joined before the end of this section.
        unsafe {
            t.resume();
            let _ = sleep_relative(DurationMono::from_millis(200));
            t.kill();
            let _ = t.join(InstantMono::INFINITE);
        }

        // Starting sleeper thread, then killing it before it wakes up.
        // SAFETY: `sleeper_kill_thread_infinite_wait` is a valid thread entry point that ignores
        // its argument.
        let t = unwrap_ok!(unsafe {
            create_with_priority(
                c"sleeper".as_ptr(),
                sleeper_kill_thread_infinite_wait,
                ptr::null_mut(),
                LOW_PRIORITY,
            )
        });
        // SAFETY: `t` was just created and is joined before the end of this section.
        unsafe {
            t.resume();
            t.kill();
            let _ = t.join(InstantMono::INFINITE);
        }

        // Starting sleeper thread, then killing it before it is unsuspended.
        // SAFETY: `sleeper_kill_thread_infinite_wait` is a valid thread entry point that ignores
        // its argument.
        let t = unwrap_ok!(unsafe {
            create_with_priority(
                c"sleeper".as_ptr(),
                sleeper_kill_thread_infinite_wait,
                ptr::null_mut(),
                LOW_PRIORITY,
            )
        });
        // SAFETY: `t` was just created and is joined before the end of this section.
        unsafe {
            t.kill(); // kill it before it is resumed
            t.resume();
            let _ = t.join(InstantMono::INFINITE);
        }

        {
            // Starting waiter thread that waits forever, then killing it while it blocks.
            stack_pin_init!(let e = Event::init_unsignaled());
            let e_ptr: *mut c_void = ptr::from_ref(&*e).cast_mut().cast();
            // SAFETY: `waiter_kill_thread_infinite_wait` is a valid entry point and `e` outlives
            // `t` (joined below).
            let t = unwrap_ok!(unsafe {
                create_with_priority(
                    c"waiter".as_ptr(),
                    waiter_kill_thread_infinite_wait,
                    e_ptr,
                    LOW_PRIORITY,
                )
            });
            // SAFETY: `t` was just created and is joined before `e` goes out of scope.
            unsafe {
                t.resume();
                let _ = sleep_relative(DurationMono::from_millis(200));
                t.kill();
                let _ = t.join(InstantMono::INFINITE);
            }
        }

        {
            // Starting waiter thread that waits forever, then killing it before it wakes up.
            stack_pin_init!(let e = Event::init_unsignaled());
            let e_ptr: *mut c_void = ptr::from_ref(&*e).cast_mut().cast();
            // SAFETY: `waiter_kill_thread_infinite_wait` is a valid entry point and `e` outlives
            // `t` (joined below).
            let t = unwrap_ok!(unsafe {
                create_with_priority(
                    c"waiter".as_ptr(),
                    waiter_kill_thread_infinite_wait,
                    e_ptr,
                    LOW_PRIORITY,
                )
            });
            // SAFETY: `t` was just created and is joined before `e` goes out of scope.
            unsafe {
                t.resume();
                t.kill();
                let _ = t.join(InstantMono::INFINITE);
            }
        }
    }

    struct AffinityTestState {
        threads: [Option<ThreadPtr>; 16],
        /// Set to stop the worker threads.
        ///
        /// A standalone flag polled across threads with no dependent memory accesses, so
        /// `Ordering::Relaxed` is sufficient.
        shutdown: AtomicBool,
    }

    fn spin_while(t: DurationMono, mut func: impl FnMut()) {
        let start = current_mono_time();

        while (current_mono_time() - start) < t {
            func();
        }
    }

    /// Returns a random non-zero value less than `active`.
    fn random_mask(rng: &mut SmallRng, active: cpu_mask_t) -> cpu_mask_t {
        debug_assert!(active > 1);
        rng.random_range(1..active)
    }

    extern "C" fn affinity_test_thread(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` points to `state` on `affinity_test`'s stack, which is fully initialized
        // before any worker thread is resumed and outlives all worker threads.
        let state = unsafe { &*arg.cast::<AffinityTestState>() };
        let active: cpu_mask_t = peek_active_mask();
        let mut rng = new_rng();

        while !state.shutdown.load(Ordering::Relaxed) {
            let which = rng.random_range(0..state.threads.len());
            match rng.random_range(0..5) {
                0 => {
                    // set affinity
                    let t = state.threads[which].unwrap();
                    // SAFETY: `t` is valid until all worker threads exit and are joined in
                    // `affinity_test`.
                    let _ = unsafe { t.set_cpu_affinity(random_mask(&mut rng, active)) };
                }
                1 => {
                    // sleep for a bit
                    let _ = sleep_relative(DurationMono::from_micros(rng.random_range(0..100)));
                }
                2 => {
                    // spin for a bit
                    spin(rng.random_range(0..100));
                }
                3 => {
                    // yield
                    spin_while(DurationMono::from_micros(rng.random_range(0..100)), r#yield);
                }
                4 => {
                    // reschedule
                    spin_while(DurationMono::from_micros(rng.random_range(0..100)), reschedule);
                }
                _ => {}
            }
        }

        0
    }

    // start a bunch of threads that randomly set the affinity of the other threads
    // to random masks while doing various work.
    // a successful pass is one where it completes the run without tripping over any asserts
    // in the scheduler code.
    /// Stress-tests random CPU affinity changes across concurrent worker threads.
    #[test]
    #[inline(never)]
    fn affinity_test() {
        let active: cpu_mask_t = peek_active_mask();
        if active == 0 || active.is_power_of_two() {
            kprintln!("aborting test, not enough active cpus");
            return true;
        }

        let mut state = AffinityTestState { threads: [None; 16], shutdown: AtomicBool::new(false) };
        let num_threads = state.threads.len();
        let state_ptr: *mut AffinityTestState = &raw mut state;

        for i in 0..num_threads {
            // SAFETY: `affinity_test_thread` is a valid thread entry point, `state` is not read by
            // any worker until `resume()` below, and `state` outlives all workers.
            let t = unwrap_ok!(unsafe {
                create_with_priority(
                    c"affinity_tester".as_ptr(),
                    affinity_test_thread,
                    state_ptr.cast(),
                    LOW_PRIORITY,
                )
            });
            // SAFETY: `state_ptr` is uniquely accessed on this thread before any worker thread is
            // resumed; writing through `state_ptr` preserves its Stacked Borrows provenance.
            unsafe {
                (*state_ptr).threads[i] = Some(t);
            }
        }

        // SAFETY: `state_ptr` points to initialized `state`, which is only shared immutably from
        // this point until all worker threads are joined.
        let state = unsafe { &*state_ptr };

        for t in state.threads.iter().flatten() {
            // SAFETY: `t` was just created and has not been resumed or joined yet.
            unsafe { t.resume() };
        }

        const DURATION: i32 = 30;
        for _i in 0..DURATION {
            let _ = sleep_relative(DurationMono::from_millis(250));
        }
        state.shutdown.store(true, Ordering::Relaxed);
        let _ = sleep_relative(DurationMono::from_seconds(1));

        for t in state.threads.iter().flatten() {
            // SAFETY: `t` is valid and has not been joined yet.
            let _ = unsafe { t.join(InstantMono::INFINITE) };
        }
    }

    extern "C" fn prio_test_thread(arg: *mut c_void) -> i32 {
        // SAFETY: `current_get()` returns the valid current `Thread`.
        let t = unsafe { &*current_get() };
        let mut bp: BaseProfile = t.snapshot_base_profile();
        assert!(bp.discipline() == SchedDiscipline::Fair);
        assert!(bp.fair_weight() == SchedulerState::convert_priority_to_weight(LOW_PRIORITY));

        // SAFETY: `arg` points to `ev` on `priority_test`'s stack, which outlives this thread.
        let ev = unsafe { &*arg.cast::<Event>() };
        ev.signal();

        // Busy loop until our priority changes.
        let mut count: i32 = 0;
        loop {
            bp = t.snapshot_base_profile();
            assert!(bp.discipline() == SchedDiscipline::Fair);
            if bp.fair_weight() == SchedulerState::convert_priority_to_weight(DEFAULT_PRIORITY) {
                break;
            }
            count = count.wrapping_add(1);
        }

        ev.signal();

        // And then when it changes again.
        loop {
            bp = t.snapshot_base_profile();
            assert!(bp.discipline() == SchedDiscipline::Fair);
            if bp.fair_weight() == SchedulerState::convert_priority_to_weight(HIGH_PRIORITY) {
                break;
            }
            count = count.wrapping_add(1);
        }

        count
    }

    /// Tests base profile priority changes on the current thread and across CPUs.
    #[test]
    #[inline(never)]
    fn priority_test() {
        // SAFETY: `current_get()` returns the valid current `Thread`.
        let t = unsafe { &*current_get() };
        let mut bp: BaseProfile = t.snapshot_base_profile();

        if !bp.is_fair()
            || bp.fair_weight() != SchedulerState::convert_priority_to_weight(DEFAULT_PRIORITY)
        {
            kprintln!("unexpected initial state, aborting test");
            return true;
        }

        let _restore_profile = zr::defer(|| {
            t.set_base_profile(&BaseProfile::new_fair(DEFAULT_PRIORITY));
        });

        t.set_base_profile(&BaseProfile::new_fair(DEFAULT_PRIORITY + 2));
        let _ = sleep_relative(DurationMono::from_millis(1));
        bp = t.snapshot_base_profile();
        assert_true!(bp.is_fair());
        assert_true!(
            bp.fair_weight() == SchedulerState::convert_priority_to_weight(DEFAULT_PRIORITY + 2)
        );

        t.set_base_profile(&BaseProfile::new_fair(DEFAULT_PRIORITY - 2));
        let _ = sleep_relative(DurationMono::from_millis(1));
        bp = t.snapshot_base_profile();
        assert_true!(bp.is_fair());
        assert_true!(
            bp.fair_weight() == SchedulerState::convert_priority_to_weight(DEFAULT_PRIORITY - 2)
        );

        let active: cpu_mask_t = peek_active_mask();
        if active == 0 || active.is_power_of_two() {
            kprintln!("skipping rest, not enough active cpus");

            return true;
        }

        stack_pin_init!(let ev = AutounsignalEvent::init_unsignaled());
        let ev_ptr: *mut c_void = ptr::from_ref(ev.as_event()).cast_mut().cast();

        // SAFETY: `prio_test_thread` is a valid thread entry point and `ev` outlives `nt` (joined
        // below).
        let nt = unwrap_ok!(unsafe {
            create_with_priority(c"prio-test".as_ptr(), prio_test_thread, ev_ptr, LOW_PRIORITY)
        });

        let curr: cpu_num_t = curr_cpu_num();
        let other: cpu_num_t = if is_cpu_online(curr.wrapping_add(1)) {
            curr.wrapping_add(1)
        } else if is_cpu_online(curr.wrapping_sub(1)) {
            curr.wrapping_sub(1)
        } else {
            assert_true!(false);
            return false;
        };

        // SAFETY: `nt` is valid and has not been joined yet.
        unsafe {
            let _ = nt.set_cpu_affinity(cpu_num_to_mask(other));
            nt.resume();
        }

        expect_ok!(ev.wait_deadline(InstantMono::INFINITE, Interruptible::Yes));
        // SAFETY: `nt` is valid and has not been joined yet.
        unsafe { nt.as_ref() }.set_base_profile(&BaseProfile::new_fair(DEFAULT_PRIORITY));

        expect_ok!(ev.wait_deadline(InstantMono::INFINITE, Interruptible::Yes));
        // SAFETY: `nt` is valid and has not been joined yet.
        unsafe { nt.as_ref() }.set_base_profile(&BaseProfile::new_fair(HIGH_PRIORITY));

        // SAFETY: `nt` is valid and has not been joined yet.
        let _count = unsafe { nt.join(InstantMono::INFINITE) };
    }

    struct SpinControl {
        done: AtomicBool,
        count: UnsafeCell<u64>,
    }

    impl SpinControl {
        const fn new() -> Self {
            Self { done: AtomicBool::new(false), count: UnsafeCell::new(0) }
        }
    }

    extern "C" fn oversubscription_spin_thread(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` points to a valid `SpinControl` on `critical_oversubscription_test`'s
        // stack that outlives this thread.
        let control = unsafe { &*arg.cast::<SpinControl>() };
        while !control.done.load(Ordering::SeqCst) {
            // SAFETY: This thread has exclusive access to `control.count` until it exits and is
            // joined by `critical_oversubscription_test`.
            unsafe {
                *control.count.get() += 1;
            }
        }
        0
    }

    /// Tests critical deadline thread precedence over a normal deadline thread on a shared CPU.
    #[test]
    #[inline(never)]
    fn critical_oversubscription_test() {
        let active_mask: cpu_mask_t = peek_active_mask();
        if active_mask == 0 || active_mask.is_power_of_two() {
            kprintln!("skipping rest, not enough active cpus");
            return true;
        }

        let current_mask: cpu_mask_t = cpu_num_to_mask(curr_cpu_num());
        // SAFETY: `ThreadPtr::current()` is valid in thread context.
        let previous_affinity = unsafe { ThreadPtr::current().set_cpu_affinity(current_mask) };
        let _restore_affinity = zr::defer(|| {
            // SAFETY: `ThreadPtr::current()` is valid in thread context.
            let _ = unsafe { ThreadPtr::current().set_cpu_affinity(previous_affinity) };
        });

        // Pick a target CPU to bind our threads to.
        let target_cpu: cpu_num_t = highest_cpu_set(active_mask & !current_mask);
        debug_assert!(target_cpu != curr_cpu_num());
        let target_mask: cpu_mask_t = cpu_num_to_mask(target_cpu);

        // Both threads demand 100% of the CPU (10ms capacity every 10ms).
        let params = SchedDeadlineParams::new(sched_ms(10), sched_ms(10));

        let mut critical_profile = BaseProfile::new_deadline(params, false);
        critical_profile.critical = true;

        let mut normal_profile = BaseProfile::new_deadline(params, false);
        normal_profile.critical = false;

        let critical_control = SpinControl::new();
        let normal_control = SpinControl::new();

        // SAFETY: `oversubscription_spin_thread` is a valid thread entry point and
        // `critical_control` and `normal_control` outlive both threads (joined below).
        let (critical_thread, normal_thread) = unsafe {
            let critical_thread = unwrap_ok!(create_with_priority(
                c"critical-thread".as_ptr(),
                oversubscription_spin_thread,
                ptr::from_ref(&critical_control).cast_mut().cast(),
                DEFAULT_PRIORITY,
            ));
            let normal_thread = unwrap_ok!(create_with_priority(
                c"normal-thread".as_ptr(),
                oversubscription_spin_thread,
                ptr::from_ref(&normal_control).cast_mut().cast(),
                DEFAULT_PRIORITY,
            ));

            let _ = critical_thread.set_cpu_affinity(target_mask);
            let _ = normal_thread.set_cpu_affinity(target_mask);

            critical_thread.as_ref().set_base_profile(&critical_profile);
            normal_thread.as_ref().set_base_profile(&normal_profile);

            // Start the threads.
            critical_thread.resume();
            normal_thread.resume();

            (critical_thread, normal_thread)
        };

        // Let them run for a short duration while oversubscribing the CPU.
        let _ = sleep_relative(DurationMono::from_millis(250));

        // Signal them to stop.
        critical_control.done.store(true, Ordering::SeqCst);
        normal_control.done.store(true, Ordering::SeqCst);

        // SAFETY: `critical_thread` and `normal_thread` are valid and have not been joined yet.
        unsafe {
            let _ = critical_thread.join(InstantMono::INFINITE);
            let _ = normal_thread.join(InstantMono::INFINITE);
        }

        // SAFETY: Both worker threads have exited and been joined, so no concurrent access to
        // `count` can occur.
        let (critical_count, normal_count) =
            unsafe { (*critical_control.count.get(), *normal_control.count.get()) };

        // The critical thread should have received vastly more CPU time. In a perfect
        // system, normal_count would be 0, but due to context switching and kernel
        // behavior it might get a few cycles.
        expect_gt!(
            critical_count,
            normal_count * 10,
            "Critical thread did not receive expected CPU precedence"
        );
    }
}
