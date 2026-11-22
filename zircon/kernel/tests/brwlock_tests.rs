// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// brwlock tests
#[cfg(ktest)]
#[unittest::suite(name = "brwlock")]
mod brwlock_tests {
    use core::convert::Infallible;
    use core::ffi::{c_int, c_void};
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use kprint::kprint;
    use pin_init::{PinInit, pin_init, stack_pin_init};
    use rand::rngs::SmallRng;
    use rand::{RngExt, SeedableRng};
    use unittest::{assert_true, expect_eq, expect_le, expect_ok, expect_true, subtest};
    use zx_status::Status;

    use crate::crypto::global_prng;
    use crate::kernel::deadline::{Deadline, TimerSlack};
    use crate::kernel::mp::get_online_mask;
    use crate::kernel::scheduler::BaseProfile;
    use crate::kernel::thread::{
        self, DEFAULT_PRIORITY, HIGH_PRIORITY, ThreadPtr, create_with_priority,
    };
    use crate::kernel::types::{cpu_mask_t, cpu_num_to_mask, highest_cpu_set, lowest_cpu_set};
    use crate::ksync_kernel::{TrackedBrwLockNoPi, TrackedBrwLockPi};
    use crate::platform_rs::timer::{DurationMono, InstantMono, current_mono_time};

    /// Use a delay spinner to create fine grained delays between 0 and 1msec
    fn rand_delay(rng: &mut SmallRng) {
        let end = current_mono_time()
            + DurationMono::from_nanos(
                rng.random_range(0..DurationMono::from_millis(1).into_nanos()),
            );
        loop {
            thread::r#yield();
            if current_mono_time() >= end {
                break;
            }
        }
    }

    /// Use a helper class for running tests so that worker threads and main thread all
    /// have easy access to shared state.
    #[ksync::guarded]
    struct BrwLockTest<LockType: ksync::RawSharedLock>
    where
        LockType::DefaultPolicy: ksync::ExclusiveLockPolicy<LockType, AcquireArgs = ()>,
    {
        #[brwlock]
        lock: ksync::BrwLockPi<LockType>,
        state: AtomicU32,
        kill: AtomicBool,
    }

    impl<LockType: ksync::RawSharedLock> BrwLockTest<LockType>
    where
        LockType::DefaultPolicy: ksync::ExclusiveLockPolicy<LockType, AcquireArgs = ()>,
    {
        fn new() -> impl PinInit<Self, Infallible> {
            pin_init!(Self {
                lock <- ksync::BrwLockPi::init(),
                state: AtomicU32::new(0),
                kill: AtomicBool::new(false),
            })
        }

        fn run_test<const READERS: usize, const WRITERS: usize>() -> bool {
            (subtest!(|| {
                stack_pin_init!(let test = Self::new());
                let test: &Self = test.into_ref().get_ref();
                let mut reader_threads: [Option<ThreadPtr>; READERS] = [None; READERS];
                let mut writer_threads: [Option<ThreadPtr>; WRITERS] = [None; WRITERS];

                // SAFETY: Kernel unit tests run after threading has been initialized, and the
                // current thread remains valid and alive for the duration of `run_test`.
                let (current, current_thread) = unsafe {
                    let current = ThreadPtr::current();
                    (current, current.as_ref())
                };
                let old_bp: BaseProfile = current_thread.snapshot_base_profile();

                // Run at high priority so that we can be validating what the other threads are
                // doing. Unless we are a uniprocessor, in which case we will just have to live
                // with poor testing. If we do boost priority then we need to make sure worker
                // threads don't ever get scheduled on our core, since we will never block and so
                // they will starve. This is currently a known starvation problem with the
                // scheduler.
                let mut worker_mask: cpu_mask_t = get_online_mask();
                if lowest_cpu_set(worker_mask) != highest_cpu_set(worker_mask) {
                    let _ = get_online_mask();
                    current_thread.set_base_profile(&BaseProfile::new_fair(HIGH_PRIORITY));
                    let pin_mask: cpu_mask_t = cpu_num_to_mask(lowest_cpu_set(worker_mask));
                    // SAFETY: `current` is the currently running thread, which is alive.
                    let _ = unsafe { current.set_cpu_affinity(pin_mask) };
                    worker_mask -= pin_mask;
                } else {
                    current_thread.set_base_profile(&BaseProfile::new_fair(DEFAULT_PRIORITY));
                }

                // Start threads
                for t in &mut reader_threads {
                    extern "C" fn reader_trampoline<LockType: ksync::RawSharedLock>(
                        arg: *mut c_void,
                    ) -> c_int
                    where
                        LockType::DefaultPolicy:
                            ksync::ExclusiveLockPolicy<LockType, AcquireArgs = ()>,
                    {
                        // SAFETY: `arg` points to the stack-pinned `test` instance, which outlives
                        // all reader threads (joined before `run_test` returns).
                        unsafe { (&*arg.cast::<BrwLockTest<LockType>>()).reader_worker() };
                        0
                    }
                    // SAFETY: `reader_trampoline` is a valid entry point, and `test` outlives the
                    // thread, which is joined below.
                    let thread = unsafe {
                        create_with_priority(
                            c"reader worker".as_ptr(),
                            reader_trampoline::<LockType>,
                            core::ptr::from_ref(test).cast_mut().cast(),
                            DEFAULT_PRIORITY,
                        )
                    }
                    .unwrap();
                    // SAFETY: `thread` was just created and has not been joined yet.
                    unsafe {
                        let _ = thread.set_cpu_affinity(worker_mask);
                        thread.resume();
                    }
                    *t = Some(thread);
                }
                for t in &mut writer_threads {
                    extern "C" fn writer_trampoline<LockType: ksync::RawSharedLock>(
                        arg: *mut c_void,
                    ) -> c_int
                    where
                        LockType::DefaultPolicy:
                            ksync::ExclusiveLockPolicy<LockType, AcquireArgs = ()>,
                    {
                        // SAFETY: `arg` points to the stack-pinned `test` instance, which outlives
                        // all writer threads (joined before `run_test` returns).
                        unsafe { (&*arg.cast::<BrwLockTest<LockType>>()).writer_worker() };
                        0
                    }
                    // SAFETY: `writer_trampoline` is a valid entry point, and `test` outlives the
                    // thread, which is joined below.
                    let thread = unsafe {
                        create_with_priority(
                            c"writer worker".as_ptr(),
                            writer_trampoline::<LockType>,
                            core::ptr::from_ref(test).cast_mut().cast(),
                            DEFAULT_PRIORITY,
                        )
                    }
                    .unwrap();
                    // SAFETY: `thread` was just created and has not been joined yet.
                    unsafe {
                        let _ = thread.set_cpu_affinity(worker_mask);
                        thread.resume();
                    }
                    *t = Some(thread);
                }

                let start = current_mono_time();
                let duration = DurationMono::from_millis(300);
                while current_mono_time() < start + duration {
                    // `Relaxed` is sufficient because only the single `state` word is inspected
                    // to check reader/writer invariants.
                    let local_state: u32 = test.state.load(Ordering::Relaxed);
                    let num_readers: u32 = local_state & 0xffff;
                    let num_writers: u32 = local_state >> 16;
                    expect_le!(num_readers, READERS as u32, "Too many readers");
                    expect_true!(num_writers == 0 || num_writers == 1, "Too many writers");
                    expect_true!(num_readers == 0 || num_writers == 0, "Readers and writers");
                    thread::r#yield();
                }

                // Like `ThreadPtr::join` with an infinite timeout except that it will
                // periodically print a message that includes `description` so that it's
                // easier to debug hangs.
                //
                // Why an infinite timeout?  Because tests often run in virtualized/emulated
                // environments where any reasonable fixed timeout could be exceeded.
                let join = |t: ThreadPtr, description: &str| -> Result<(), Status> {
                    const TIMEOUT_MS: i64 = 5000;
                    loop {
                        let deadline = Deadline::after_mono(
                            DurationMono::from_millis(TIMEOUT_MS),
                            TimerSlack::none(),
                        );
                        // SAFETY: `t` points to a non-detached thread created above and not yet
                        // joined.
                        let status = unsafe { t.join(InstantMono(deadline.when().0)) }.map(|_| ());
                        if status != Err(Status::TIMED_OUT) {
                            return status;
                        }
                        kprint!(
                            "failed to join {:s} after {} msec, retrying\n",
                            description,
                            TIMEOUT_MS
                        );
                    }
                };

                // Shutdown all the threads. Validating they can shutdown is important
                // to ensure they didn't get stuck on the waitqueue and never woken up.
                test.kill.store(true, Ordering::SeqCst);
                for t in reader_threads {
                    let status = join(t.unwrap(), "Reader");
                    expect_ok!(status, "Reader failed to join");
                }
                for t in writer_threads {
                    let status = join(t.unwrap(), "Writer");
                    expect_ok!(status, "Writer failed to join");
                }

                expect_eq!(test.state.load(Ordering::SeqCst), 0u32, "Threads still holding lock");

                // Restore original base profile.
                current_thread.set_base_profile(&old_bp);
            }))()
        }

        fn reader_worker(&self) {
            let mut seed = [0u8; 32];
            global_prng::draw(&mut seed);
            let mut rng = SmallRng::from_seed(seed);

            // `Relaxed` is sufficient for polling the one-way `kill` flag in a loop and for
            // updating `state` while serialized by `self.lock` (where the lock's acquire/release
            // operations provide synchronization between lock holders).
            while !self.kill.load(Ordering::Relaxed) {
                {
                    ksync::lock!(self.read_lock());
                    let _ = self.state.fetch_add(1, Ordering::Relaxed);
                    thread::r#yield();
                    let _ = self.state.fetch_sub(1, Ordering::Relaxed);
                }
                rand_delay(&mut rng);
            }
        }

        fn writer_worker(&self) {
            let mut seed = [0u8; 32];
            global_prng::draw(&mut seed);
            let mut rng = SmallRng::from_seed(seed);

            // `Relaxed` is sufficient for polling the one-way `kill` flag in a loop and for
            // updating `state` while serialized by `self.lock` (where the lock's acquire/release
            // operations provide synchronization between lock holders).
            while !self.kill.load(Ordering::Relaxed) {
                {
                    ksync::lock!(self.write_lock());
                    let _ = self.state.fetch_add(0x10000, Ordering::Relaxed);
                    thread::r#yield();
                    let _ = self.state.fetch_sub(0x10000, Ordering::Relaxed);
                }
                rand_delay(&mut rng);
            }
        }
    }

    // The number of threads to use for readers, writers and upgraders was chosen by manual
    // instrumentation of the brwlock to see if all the different code paths were being hit.

    /// parallel readers(PI)
    #[test]
    fn parallel_readers_pi() {
        assert_true!(BrwLockTest::<TrackedBrwLockPi>::run_test::<8, 0>());
    }

    /// single writer(PI)
    #[test]
    fn single_writer_pi() {
        assert_true!(BrwLockTest::<TrackedBrwLockPi>::run_test::<0, 4>());
    }

    /// readers and writer(PI)
    #[test]
    fn readers_and_writer_pi() {
        assert_true!(BrwLockTest::<TrackedBrwLockPi>::run_test::<4, 2>());
    }

    /// parallel readers(No PI)
    #[test]
    fn parallel_readers_no_pi() {
        assert_true!(BrwLockTest::<TrackedBrwLockNoPi>::run_test::<8, 0>());
    }

    /// single writer(No PI)
    #[test]
    fn single_writer_no_pi() {
        assert_true!(BrwLockTest::<TrackedBrwLockNoPi>::run_test::<0, 4>());
    }

    /// readers and writer(No PI)
    #[test]
    fn readers_and_writer_no_pi() {
        assert_true!(BrwLockTest::<TrackedBrwLockNoPi>::run_test::<4, 2>());
    }
}
