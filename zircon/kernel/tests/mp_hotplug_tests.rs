// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// Tests for unplugging and hotplugging cores
#[cfg(ktest)]
#[unittest::suite(name = "hotplug")]
mod mp_hotplug_tests {
    use core::cell::Cell;
    use core::ffi::c_void;
    use core::marker::{PhantomData, PhantomPinned};
    use core::ptr::NonNull;
    use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

    use kprint::kprintln;
    use pin_init::{pin_data, pin_init, pin_init_array_from_fn, stack_pin_init};
    use unittest::{
        assert_eq, assert_false, assert_ok, expect_eq, expect_ge, expect_gt, expect_ne,
        expect_true, unwrap_ok, unwrap_some,
    };
    use zx_status::Status;
    use zx_types::{ZX_OK, ZX_TIME_INFINITE};

    use crate::arch_rs::{InterruptDisableGuard, curr_cpu_num};
    use crate::kconcurrent::{
        ChainLockProof, ChainLockTransaction, ChainLocked, IRQ_SAVE_OPTION, clt_tag,
        single_chain_lock_guard_owned,
    };
    use crate::kernel::deadline::{Deadline, TimerSlack};
    use crate::kernel::dpc::{Dpc, DpcRunner, QueueType};
    use crate::kernel::event::{AutounsignalEvent, Event};
    use crate::kernel::mp::{
        self, CPU_MASK_ALL, INVALID_CPU, MpIpiTarget, SMP_MAX_CPUS, cpu_mask_t, cpu_num_t,
        cpu_num_to_mask,
    };
    use crate::kernel::scheduler;
    use crate::kernel::scheduler_state::{HIGHEST_PRIORITY, LOWEST_PRIORITY, ThreadStateKind};
    use crate::kernel::thread::{
        self, DEFAULT_PRIORITY, LOW_PRIORITY, MigrateFn, MigrateStage, Thread, ThreadPtr,
        sleep_relative,
    };
    use crate::kernel::types::{BOOT_CPU_ID, highest_cpu_set, lowest_cpu_set};
    use crate::pdev_power::PowerCpuState;
    use crate::platform_rs::power::platform_get_cpu_state;
    use crate::platform_rs::timer::{DurationMono, InstantMono, current_mono_time};

    mod send_const_ptr {
        use core::ptr::NonNull;

        /// `Send` wrapper for shared non-null, aligned pointers captured by `'static` `MigrateFn`
        /// closures.
        #[derive(Debug)]
        pub struct SendConstPtr<T> {
            // Safety invariant: properly aligned for `T` and derived from a valid `&T` with
            // shared-read provenance.
            ptr: NonNull<T>,
        }

        impl<T: Sync> Clone for SendConstPtr<T> {
            fn clone(&self) -> Self {
                *self
            }
        }

        impl<T: Sync> Copy for SendConstPtr<T> {}

        impl<T: Sync> SendConstPtr<T> {
            /// Wraps `reference` as a `SendConstPtr<T>`, preserving its non-null and alignment
            /// guarantees.
            pub const fn from_ref(reference: &T) -> Self {
                Self { ptr: NonNull::from_ref(reference) }
            }

            /// Returns a shared reference to the pointee.
            ///
            /// # Safety
            ///
            /// The caller must ensure that from the `from_ref` call through the end of `'a`, the
            /// pointed-to `T` remains valid and initialized in the same allocation and is neither
            /// mutably borrowed (`&mut T`) nor mutated outside `UnsafeCell`.
            #[inline]
            pub unsafe fn as_ref<'a>(self) -> &'a T {
                // SAFETY: By `SendConstPtr`'s field invariant (`from_ref`), `self.ptr` is properly
                // aligned for `T`.
                unsafe {
                    core::hint::assert_unchecked(self.ptr.as_ptr().is_aligned());
                }
                // SAFETY: `self.ptr` is non-null and aligned per `from_ref`'s field invariant;
                // validity, initialization, and absence of mutable aliasing or non-`UnsafeCell`
                // mutation from `from_ref` through `'a` are guaranteed by the caller, and `T: Sync`
                // permits shared `&T` access across threads.
                unsafe { self.ptr.as_ref() }
            }
        }

        // SAFETY: `SendConstPtr` encapsulates `ptr` (constructible only from `&T` via `from_ref`)
        // and only exposes `as_ref<'a>(self) -> &'a T` (which cannot mutate `T` and requires its
        // own `unsafe` liveness and aliasing proof), and `T: Sync` ensures shared `&T` access
        // across threads is sound.
        unsafe impl<T: Sync> Send for SendConstPtr<T> {}
    }

    use send_const_ptr::SendConstPtr;

    /// Writes the current CPU number to `*arg.cast::<cpu_num_t>()`.
    ///
    /// # Safety
    ///
    /// `arg` must be non-null, properly aligned for `cpu_num_t`, and valid for exclusive writes
    /// of a `cpu_num_t` for the duration of this call.
    unsafe extern "C" fn resume_cpu_test_thread(arg: *mut c_void) -> i32 {
        // SAFETY: Guaranteed by the caller.
        unsafe {
            *arg.cast::<cpu_num_t>() = curr_cpu_num();
        }
        0
    }

    /// "Unplug" online secondary (non-BOOT) cores
    fn unplug_all_cores() -> Result<(), Status> {
        let cpumask: cpu_mask_t = mp::get_online_mask() & !cpu_num_to_mask(BOOT_CPU_ID);
        mp::unplug_cpu_mask(cpumask, ZX_TIME_INFINITE)
    }

    fn hotplug_core(i: cpu_num_t) -> Result<(), Status> {
        let cpumask: cpu_mask_t = cpu_num_to_mask(i);
        mp::hotplug_cpu_mask(cpumask)
    }

    fn get_num_cpus_online() -> u32 {
        let mut count = 0;
        let mut online: cpu_mask_t = mp::get_online_mask();
        while online != 0 {
            online >>= 1;
            count += 1;
        }
        count
    }

    fn wait_for_cpu_offline(i: cpu_num_t) -> Result<(), Status> {
        let mut print_time: InstantMono = current_mono_time() + DurationMono::from_seconds(5);
        loop {
            let res = platform_get_cpu_state(i);
            if let Err(err) = res {
                if err == Status::NOT_SUPPORTED {
                    // x86 does not implement `platform_get_cpu_state`, so return OK if the call
                    // returns `Status::NOT_SUPPORTED`.
                    return Ok(());
                }
                return Err(err);
            } else if matches!(res, Ok(PowerCpuState::Off | PowerCpuState::Stopped)) {
                return Ok(());
            }
            if current_mono_time() > print_time {
                print_time = current_mono_time() + DurationMono::from_seconds(5);
                kprintln!("Still waiting for CPU {i} to go offline, waiting 5 more seconds");
            }
            let _ = sleep_relative(DurationMono::from_micros(200));
        }
    }

    fn wait_for_cpu_active(i: cpu_num_t) {
        let mut print_time: InstantMono = current_mono_time() + DurationMono::from_seconds(5);
        loop {
            if scheduler::peek_is_active(i) {
                return;
            }
            if current_mono_time() > print_time {
                print_time = current_mono_time() + DurationMono::from_seconds(5);
                kprintln!("Still waiting for CPU {i} to become active, waiting 5 more seconds");
            }
            let _ = sleep_relative(DurationMono::from_micros(200));
        }
    }

    /// Wait until `i`'s `DpcRunner` is running, by queuing a `Dpc` on each of `i`'s `Dpc` queues
    /// and waiting for them to execute.
    ///
    /// A hotplugged core becomes active (see `wait_for_cpu_active`) before it has created its `Dpc`
    /// threads, and a core must not be unplugged before then (`mp::unplug_cpu_mask` shuts down its
    /// `Dpc` threads). The general queue's thread is created (and may start executing `Dpc`s)
    /// before the low latency queue's thread, so both queues must be checked.
    ///
    /// # Safety
    ///
    /// The caller must ensure that this function is called after `LK_INIT_LEVEL_THREADING` from a
    /// normal thread context where blocking is permitted (with interrupts enabled and no spinlocks
    /// held), and without holding `Thread::get_list_lock()`, any scheduler queue lock, or an active
    /// `ChainLockTransaction`.
    unsafe fn wait_for_cpu_dpc_runner(i: cpu_num_t) -> Result<(), Status> {
        /// Enqueues and waits for a DPC on both DPC queues of the current CPU.
        ///
        /// # Safety
        ///
        /// Must run in normal thread context after `LK_INIT_LEVEL_THREADING` where blocking is
        /// permitted.
        unsafe extern "C" fn entry(_arg: *mut c_void) -> i32 {
            extern "C" fn signal_event(dpc: &Dpc) {
                let event_ptr = dpc.arg::<Event>().unwrap().as_ptr();
                // SAFETY: `event_ptr` points to the stack-pinned `event` in `entry`, which stays
                // live until `wait_infinite` returns after `signal_worker` releases `wait.lock()`
                // (calling `signal_worker` on `*const Event` avoids holding `&Event` once the woken
                // waiter may drop `event`), and no `ChainLockTransaction` is active in DPC context.
                unsafe { Event::signal_worker(event_ptr, Ok(()), None) };
            }

            const QUEUE_TYPES: [QueueType; 2] = [QueueType::General, QueueType::LowLatency];
            for queue_type in QUEUE_TYPES {
                stack_pin_init!(let event = Event::init_unsignaled());
                let event_ref: &Event = event.as_ref().get_ref();
                let dpc = Dpc::new(signal_event, Some(NonNull::from_ref(event_ref)));
                // SAFETY: `dpc` and `event` remain valid and unmoved on this thread's stack until
                // the DPC is dequeued and `event_ref.wait_infinite()` returns below;
                // `signal_event` only signals `event`.
                let status = unsafe { DpcRunner::enqueue(&dpc, queue_type) };
                if let Err(err) = status {
                    return err.into_raw();
                }
                // Wait non-interruptibly, since `dpc` and `event` live on this thread's stack.
                let _ = event_ref.wait_infinite();
            }
            ZX_OK
        }

        // SAFETY: A null `t` allocates the `Thread`, `entry` runs in a newly created kernel thread
        // after `LK_INIT_LEVEL_THREADING` and ignores its argument, and the caller guarantees
        // blocking is permitted with no active `ChainLockTransaction`; the thread is joined before
        // returning.
        let waiter_thread = unsafe {
            thread::create_etc(
                core::ptr::null_mut(),
                b"wait-for-dpc-runner",
                Some(entry),
                core::ptr::null_mut(),
                &scheduler::BaseProfile::new_fair(DEFAULT_PRIORITY),
                None,
            )
        }?;
        // SAFETY: `waiter_thread` was just created and has not been resumed, destroyed, or joined
        // yet, and the caller guarantees normal thread context with no spinlocks, scheduler queue
        // locks, or active `ChainLockTransaction` held.
        unsafe {
            let _ = waiter_thread.set_cpu_affinity(cpu_num_to_mask(i));
            waiter_thread.resume();
        }
        // SAFETY: `waiter_thread` was created and resumed above, has not been joined or detached,
        // this thread is not `waiter_thread`, no `&Thread` or lock is held, and the caller
        // guarantees `LK_INIT_LEVEL_THREADING` has completed and blocking is permitted with no
        // active `ChainLockTransaction`.
        let retcode = unsafe { waiter_thread.join(InstantMono::INFINITE) }?;
        Status::ok(retcode)
    }

    // Unplug all cores (except for Boot core), then hotplug
    // the cores one by one and make sure that we can schedule
    // tasks on that core.
    /// test unplug and hotplug cores one by one
    #[test]
    fn mp_hotplug_test() {
        // TODO(https://fxbug.dev/42086046): Re-enable test on RISC-V.
        if cfg!(target_arch = "riscv64") {
            kprintln!("skipping test mp_hotplug, hotplug only supported on x64 and arm64");
            return true;
        }

        let num_cores: u32 = get_num_cpus_online();
        if num_cores < 2 {
            kprintln!("skipping test mp_hotplug, not enough online cpus");
            return true;
        }
        // SAFETY: Kernel unit tests run after `LK_INIT_LEVEL_THREADING` in normal thread context
        // with no thread lock, scheduler queue lock, or `ChainLockTransaction` held.
        let previous_affinity =
            unsafe { ThreadPtr::current().set_cpu_affinity(cpu_num_to_mask(BOOT_CPU_ID)) };
        let _restore_affinity = zr::defer(move || {
            // SAFETY: Runs after `LK_INIT_LEVEL_THREADING` on the current test thread before
            // returning, with no thread lock, scheduler queue lock, or `ChainLockTransaction` held.
            let _ = unsafe { ThreadPtr::current().set_cpu_affinity(previous_affinity) };
        });
        let _restore_cores = zr::defer(move || {
            for i in 0..num_cores {
                if i != BOOT_CPU_ID && !mp::is_cpu_online(i) {
                    let status = hotplug_core(i);
                    assert!(
                        status.is_ok(),
                        "hotplugging core {} failed: {}",
                        i,
                        Status::result_into_raw(status),
                    );
                    wait_for_cpu_active(i);
                    // SAFETY: `_restore_cores` runs at test exit after `LK_INIT_LEVEL_THREADING` in
                    // normal thread context where blocking is permitted with interrupts enabled and
                    // no spinlocks, thread list lock, scheduler queue locks, or active
                    // `ChainLockTransaction` held.
                    let dpc_status = unsafe { wait_for_cpu_dpc_runner(i) };
                    assert!(
                        dpc_status.is_ok(),
                        "waiting for core {}'s dpc runner failed: {}",
                        i,
                        Status::result_into_raw(dpc_status),
                    );
                }
            }
        });
        // "Unplug" online secondary (non-BOOT) cores
        assert_ok!(unplug_all_cores(), "unplugging all cores failed");
        for i in 0..num_cores {
            if i == BOOT_CPU_ID {
                continue;
            }
            // Wait until this core is fully offline.
            assert_ok!(wait_for_cpu_offline(i), "waiting for core to go offline failed");
            // Hotplug this core.
            assert_ok!(hotplug_core(i), "hotplugging core failed");
            // Wait until the core is active.
            wait_for_cpu_active(i);
            // Unplug shuts down DPC threads, which start after the core becomes active.
            assert_ok!(
                // SAFETY: Runs after `LK_INIT_LEVEL_THREADING` in normal thread context where
                // blocking is permitted with interrupts enabled and no spinlocks, thread list lock,
                // scheduler queue locks, or active `ChainLockTransaction` held.
                unsafe { wait_for_cpu_dpc_runner(i) },
                "waiting for core's dpc runner failed"
            );
            // Create a thread, affine it to the core just hotplugged
            // and make sure the thread does get scheduled there.
            let mut running_core: cpu_num_t = INVALID_CPU;
            let new_thread = unwrap_ok!(
                // SAFETY: A null `t` allocates the `Thread`, `resume_cpu_test_thread` writes
                // aligned stack-local `running_core` once before exiting, blocking is permitted, no
                // `ChainLockTransaction` is active, and `new_thread` is joined below (aborting on
                // error) before `running_core` is read or goes out of scope.
                unsafe {
                    thread::create_etc(
                        core::ptr::null_mut(),
                        b"resume-test-thread",
                        Some(resume_cpu_test_thread),
                        (&raw mut running_core).cast::<c_void>(),
                        &scheduler::BaseProfile::new_fair(DEFAULT_PRIORITY),
                        None,
                    )
                },
                "Thread create failed"
            );
            // SAFETY: `thread_lists_init` has completed; `new_thread` was just created and has not
            // been resumed, erased, destroyed, or joined yet; and no locks, spinlocks, scheduler
            // queue locks, or `ChainLockTransaction` are held.
            unsafe {
                let _ = new_thread.set_cpu_affinity(cpu_num_to_mask(i));
                new_thread.set_migrate_fn(Some(MigrateFn::new(|_: &Thread, _: MigrateStage| {})));
                new_thread.resume();
            }
            // SAFETY: `new_thread` was created and resumed above, has not been joined or detached,
            // this thread is not `new_thread`, no `&Thread` or lock is held, and blocking is
            // permitted with no active `ChainLockTransaction`.
            unsafe { new_thread.join(InstantMono::INFINITE) }.expect("thread join failed");
            assert_eq!(i, running_core, "Thread not running on hotplugged core");
        }
    }

    // Repeatedly unplug and hotplug a core while another thread repeatedly calls `mp::sync_exec`
    // targeting it, and make sure that every `mp::sync_exec` call returns.
    //
    // A task may be queued for the core after it has disabled interrupts to go offline, in which
    // case the task is drained when the core is marked offline rather than executed. Otherwise,
    // the task is executed by the core before it goes offline. Either way, `mp::sync_exec` must not
    // wait indefinitely for the core, and must not return while its task is still queued (which
    // `mp::sync_exec` checks with a `debug_assert!`).
    /// test mp_sync_exec while unplugging and hotplugging a core
    #[test]
    fn mp_hotplug_sync_exec_test() {
        // TODO(https://fxbug.dev/42086046): Re-enable test on RISC-V.
        if cfg!(target_arch = "riscv64") {
            kprintln!(
                "skipping test mp_hotplug_sync_exec, hotplug only supported on x64 and arm64"
            );
            return true;
        }

        let online_secondaries: cpu_mask_t = mp::get_online_mask() & !cpu_num_to_mask(BOOT_CPU_ID);
        if online_secondaries == 0 {
            kprintln!("skipping test mp_hotplug_sync_exec, not enough online cpus");
            return true;
        }
        let target_cpu: cpu_num_t = highest_cpu_set(online_secondaries);

        // SAFETY: Kernel unit tests run after `LK_INIT_LEVEL_THREADING` in normal thread context
        // with no thread lock, scheduler queue lock, or `ChainLockTransaction` held.
        let previous_affinity =
            unsafe { ThreadPtr::current().set_cpu_affinity(cpu_num_to_mask(BOOT_CPU_ID)) };
        let _restore_affinity = zr::defer(move || {
            // SAFETY: Runs after `LK_INIT_LEVEL_THREADING` on the current test thread before
            // returning, with no thread lock, scheduler queue lock, or `ChainLockTransaction` held.
            let _ = unsafe { ThreadPtr::current().set_cpu_affinity(previous_affinity) };
        });

        // If an assertion below fails while `target_cpu` is unplugged, hotplug it again so that
        // subsequent tests run with the same set of cores.
        let _restore_target_cpu = zr::defer(move || {
            if !mp::is_cpu_online(target_cpu) {
                let status = hotplug_core(target_cpu);
                assert!(
                    status.is_ok(),
                    "hotplugging core {} failed: {}",
                    target_cpu,
                    Status::result_into_raw(status),
                );
                wait_for_cpu_active(target_cpu);
                // Unplug shuts down DPC threads, which start after the core becomes active.
                // SAFETY: `_restore_target_cpu` runs at test exit after `LK_INIT_LEVEL_THREADING`
                // in normal thread context where blocking is permitted with interrupts enabled and
                // no spinlocks, thread list lock, scheduler queue locks, or active
                // `ChainLockTransaction` held.
                let dpc_status = unsafe { wait_for_cpu_dpc_runner(target_cpu) };
                assert!(
                    dpc_status.is_ok(),
                    "waiting for core {}'s dpc runner failed: {}",
                    target_cpu,
                    Status::result_into_raw(dpc_status),
                );
            }
        });

        struct State {
            target_cpu: cpu_num_t,
            stop: AtomicBool,
            calls: AtomicU64,
        }
        let state = State { target_cpu, stop: AtomicBool::new(false), calls: AtomicU64::new(0) };

        /// The sender may run on any online core other than the one being unplugged and hotplugged.
        ///
        /// # Safety
        ///
        /// `arg` must point to a valid, shared `State` that outlives this thread.
        unsafe extern "C" fn sender_entry(arg: *mut c_void) -> i32 {
            // SAFETY: Guaranteed by the caller (`stop_sender` joins `sender` before `state` drops).
            let state = unsafe { &*arg.cast::<State>() };
            while !state.stop.load(Ordering::Relaxed) {
                // Alternate between targeting just the core being unplugged and hotplugged, and
                // all other cores.
                let calls = state.calls.load(Ordering::Relaxed);
                if calls.is_multiple_of(2) {
                    // SAFETY: `MpIpiTarget::Mask` does not require interrupts to be disabled.
                    unsafe {
                        mp::sync_exec(MpIpiTarget::Mask, cpu_num_to_mask(state.target_cpu), || {});
                    }
                } else {
                    // Targeting all but the local core requires interrupts to be disabled.
                    let _irq_disable_guard = InterruptDisableGuard::new();
                    // SAFETY: Interrupts are disabled by `_irq_disable_guard` as required for
                    // `MpIpiTarget::AllButLocal`.
                    unsafe {
                        mp::sync_exec(MpIpiTarget::AllButLocal, 0, || {});
                    }
                }
                state.calls.store(calls + 1, Ordering::Relaxed);
                // While the target core is offline, every other call returns immediately, so yield
                // to avoid monopolizing this core.
                thread::r#yield();
            }
            0
        }

        let state_ptr: *mut c_void = core::ptr::from_ref(&state).cast_mut().cast();
        let sender = unwrap_ok!(
            // SAFETY: A null `t` allocates the `Thread`, `sender_entry` only accesses `state` via
            // shared reference, blocking is permitted, no `ChainLockTransaction` is active, and
            // `stop_sender` joins `sender` (aborting on error) before `state` is dropped.
            unsafe {
                thread::create_etc(
                    core::ptr::null_mut(),
                    b"mp-hotplug-sync-exec-sender",
                    Some(sender_entry),
                    state_ptr,
                    &scheduler::BaseProfile::new_fair(DEFAULT_PRIORITY),
                    None,
                )
            },
            "Thread create failed"
        );
        // SAFETY: `sender` was just created and has not been resumed, destroyed, or joined yet, and
        // no spinlocks, scheduler queue locks, or `ChainLockTransaction` are held.
        unsafe {
            let _ = sender.set_cpu_affinity(mp::get_online_mask() & !cpu_num_to_mask(target_cpu));
            sender.resume();
        }

        // Stop the sender, and wait for it to finish, before `state` goes out of scope, even if an
        // assertion below fails. If any `mp::sync_exec` call fails to return, the sender never
        // observes `stop`, and this test hangs.
        let mut stop_sender = zr::defer(|| {
            state.stop.store(true, Ordering::Relaxed);
            // SAFETY: `sender` was created and resumed above, `stop_sender` runs at most once,
            // this thread is not `sender`, no `&Thread` or lock is held, and blocking is permitted
            // with no active `ChainLockTransaction`.
            unsafe { sender.join(InstantMono::INFINITE) }.expect("joining sender failed");
        });

        const ITERATIONS: i32 = 20;
        for _ in 0..ITERATIONS {
            assert_ok!(
                mp::unplug_cpu_mask(cpu_num_to_mask(target_cpu), ZX_TIME_INFINITE),
                "unplugging core failed"
            );
            assert_ok!(wait_for_cpu_offline(target_cpu), "waiting for core to go offline failed");
            assert_ok!(hotplug_core(target_cpu), "hotplugging core failed");
            wait_for_cpu_active(target_cpu);
            // Unplug shuts down DPC threads, which start after the core becomes active.
            assert_ok!(
                // SAFETY: Runs after `LK_INIT_LEVEL_THREADING` in normal thread context where
                // blocking is permitted with interrupts enabled and no spinlocks, thread list
                // lock, scheduler queue locks, or active `ChainLockTransaction` held.
                unsafe { wait_for_cpu_dpc_runner(target_cpu) },
                "waiting for core's dpc runner failed"
            );
        }

        stop_sender.call();
        expect_gt!(state.calls.load(Ordering::Relaxed), 0u64);
    }

    /// Polls until `thread` enters `expected_state`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `thread` points to a valid, live [`Thread`] that is not joined
    /// or destroyed while this function runs, and that this function is called from normal thread
    /// context where blocking is permitted with no locks or `ChainLockTransaction` held.
    unsafe fn wait_for_thread_state(thread: ThreadPtr, expected_state: ThreadStateKind) {
        let mut print_time: InstantMono = current_mono_time() + DurationMono::from_seconds(5);
        loop {
            {
                // SAFETY: Caller guarantees `thread` remains live across this function, and
                // `thread_ref` is scoped inside this lock block and dropped before
                // `sleep_relative`.
                let thread_ref = unsafe { thread.as_ref() };
                single_chain_lock_guard_owned!(
                    guard,
                    thread_ref,
                    IRQ_SAVE_OPTION,
                    clt_tag!("mp_hotplug_wait_for_thread_state")
                );
                if guard.proof().state() == expected_state {
                    return;
                }
            }
            if current_mono_time() > print_time {
                print_time = current_mono_time() + DurationMono::from_seconds(5);
                kprintln!(
                    "Still waiting for thread to enter state {}, waiting 5 more seconds",
                    expected_state as i32
                );
            }
            let _ = sleep_relative(DurationMono::from_micros(100));
        }
    }

    // Regression test for https://fxbug.dev/578020457:
    // Verify that threads hard-pinned to a single secondary CPU can be woken (both from an
    // `OwnedWaitQueue` via `KMutex` release and from a `WaitQueue` via `Event::signal`) while their
    // pinned CPU is offline or remain `ThreadStateKind::Ready` across unplug (running
    // `MigrateStage::Save` before power-off), stay queued on that inactive CPU's scheduler without
    // panicking or dereferencing `INVALID_CPU`, and execute on that CPU once it is hotplugged back
    // online.
    /// test unblocking pinned threads while their core is offline
    #[test]
    fn mp_hotplug_unblock_pinned_while_offline_test() {
        // TODO(https://fxbug.dev/42086046): Re-enable test on RISC-V.
        if cfg!(target_arch = "riscv64") {
            kprintln!(
                "skipping test mp_hotplug_unblock_pinned_while_offline, hotplug only supported on \
                 x64 and arm64"
            );
            return true;
        }

        let online_secondaries: cpu_mask_t = mp::get_online_mask() & !cpu_num_to_mask(BOOT_CPU_ID);
        if online_secondaries == 0 {
            kprintln!(
                "skipping test mp_hotplug_unblock_pinned_while_offline, not enough online cpus"
            );
            return true;
        }
        let target_cpu: cpu_num_t = highest_cpu_set(online_secondaries);

        // SAFETY: Kernel unit tests run after `LK_INIT_LEVEL_THREADING` in normal thread context
        // with no thread lock, scheduler queue lock, or `ChainLockTransaction` held.
        let previous_affinity =
            unsafe { ThreadPtr::current().set_cpu_affinity(cpu_num_to_mask(BOOT_CPU_ID)) };
        let _restore_affinity = zr::defer(move || {
            // SAFETY: Runs after `LK_INIT_LEVEL_THREADING` on the current test thread before
            // returning, with no thread lock, scheduler queue lock, or `ChainLockTransaction` held.
            let _ = unsafe { ThreadPtr::current().set_cpu_affinity(previous_affinity) };
        });

        /// Relaxed atomic loads/stores on `State` are synchronized by `Event::wait_infinite()`,
        /// `wait_for_thread_state()` / `wait_for_cpu_offline()` lock acquisitions, and
        /// `ThreadPtr::join()`. Heap-allocate `State` to keep this function's kernel stack frame
        /// small.
        #[ksync::guarded]
        #[pin_data]
        struct State {
            #[mutex]
            lock: ksync::KMutex<crate::ksync_kernel::TrackedMutex>,
            #[pin]
            holder_locked_event: Event,
            #[pin]
            release_lock_event: Event,
            #[pin]
            holder_released_event: Event,
            #[pin]
            wake_event: Event,
            #[pin]
            ready_pinned_started_event: Event,
            observe_post_hotplug_cpu: AtomicBool,
            stop_ready_pinned: AtomicBool,
            mutex_waiter_before_cpu: AtomicU32,
            mutex_waiter_after_cpu: AtomicU32,
            event_waiter_before_cpu: AtomicU32,
            event_waiter_after_cpu: AtomicU32,
            ready_pinned_before_cpu: AtomicU32,
            ready_pinned_after_cpu: AtomicU32,
            migrate_save_calls: AtomicU32,
            migrate_restore_calls: AtomicU32,
            ready_migrate_save_calls: AtomicU32,
            ready_migrate_restore_calls: AtomicU32,
        }

        let state = unwrap_ok!(
            kalloc::Box::try_pin_init(pin_init!(State {
                lock <- ksync::KMutex::init(),
                holder_locked_event <- Event::init_unsignaled(),
                release_lock_event <- Event::init_unsignaled(),
                holder_released_event <- Event::init_unsignaled(),
                wake_event <- Event::init_unsignaled(),
                ready_pinned_started_event <- Event::init_unsignaled(),
                observe_post_hotplug_cpu: AtomicBool::new(false),
                stop_ready_pinned: AtomicBool::new(false),
                mutex_waiter_before_cpu: AtomicU32::new(INVALID_CPU),
                mutex_waiter_after_cpu: AtomicU32::new(INVALID_CPU),
                event_waiter_before_cpu: AtomicU32::new(INVALID_CPU),
                event_waiter_after_cpu: AtomicU32::new(INVALID_CPU),
                ready_pinned_before_cpu: AtomicU32::new(INVALID_CPU),
                ready_pinned_after_cpu: AtomicU32::new(INVALID_CPU),
                migrate_save_calls: AtomicU32::new(0),
                migrate_restore_calls: AtomicU32::new(0),
                ready_migrate_save_calls: AtomicU32::new(0),
                ready_migrate_restore_calls: AtomicU32::new(0),
            })),
            "failed to allocate test state"
        );
        let state_ref: &State = state.as_ref().get_ref();
        let state_ptr: *mut c_void = core::ptr::from_ref(state_ref).cast_mut().cast();
        let state_const = SendConstPtr::from_ref(state_ref);

        let mutex_holder: Cell<Option<ThreadPtr>> = Cell::new(None);
        let mutex_waiter: Cell<Option<ThreadPtr>> = Cell::new(None);
        let event_waiter: Cell<Option<ThreadPtr>> = Cell::new(None);
        let ready_pinned: Cell<Option<ThreadPtr>> = Cell::new(None);

        let _cleanup_threads = zr::defer(|| {
            state_ref.stop_ready_pinned.store(true, Ordering::Relaxed);
            state_ref.release_lock_event.signal();
            state_ref.wake_event.signal();
            if let Some(thread) = mutex_holder.take() {
                // SAFETY: `thread` was created and resumed below, `take()` ensures it is joined at
                // most once, this thread is not `thread`, no `&Thread` or lock is held, and
                // blocking is permitted with no active `ChainLockTransaction`.
                unsafe { thread.join(InstantMono::INFINITE) }.expect("joining mutex_holder failed");
            }
            if let Some(thread) = mutex_waiter.take() {
                // SAFETY: `thread` was created and resumed below, `take()` ensures it is joined at
                // most once, this thread is not `thread`, no `&Thread` or lock is held, and
                // blocking is permitted with no active `ChainLockTransaction`.
                unsafe { thread.join(InstantMono::INFINITE) }.expect("joining mutex_waiter failed");
            }
            if let Some(thread) = event_waiter.take() {
                // SAFETY: `thread` was created and resumed below, `take()` ensures it is joined at
                // most once, this thread is not `thread`, no `&Thread` or lock is held, and
                // blocking is permitted with no active `ChainLockTransaction`.
                unsafe { thread.join(InstantMono::INFINITE) }.expect("joining event_waiter failed");
            }
            if let Some(thread) = ready_pinned.take() {
                // SAFETY: `thread` was created and resumed below, `take()` ensures it is joined at
                // most once, this thread is not `thread`, no `&Thread` or lock is held, and
                // blocking is permitted with no active `ChainLockTransaction`.
                unsafe { thread.join(InstantMono::INFINITE) }.expect("joining ready_pinned failed");
            }
        });

        // Declared after `_cleanup_threads` so `target_cpu` is brought back online first during
        // unwinding, allowing any threads pinned to `target_cpu` to run and exit when
        // `_cleanup_threads` joins them.
        let _restore_target_cpu = zr::defer(move || {
            if !mp::is_cpu_online(target_cpu) {
                let status = hotplug_core(target_cpu);
                assert!(
                    status.is_ok(),
                    "hotplugging core {} failed: {}",
                    target_cpu,
                    Status::result_into_raw(status),
                );
                wait_for_cpu_active(target_cpu);
                // Unplug shuts down DPC threads, which start after the core becomes active.
                // SAFETY: `_restore_target_cpu` runs at test exit after `LK_INIT_LEVEL_THREADING`
                // in normal thread context where blocking is permitted with interrupts enabled and
                // no spinlocks, thread list lock, scheduler queue locks, or active
                // `ChainLockTransaction` held.
                let dpc_status = unsafe { wait_for_cpu_dpc_runner(target_cpu) };
                assert!(
                    dpc_status.is_ok(),
                    "waiting for core {}'s dpc runner failed: {}",
                    target_cpu,
                    Status::result_into_raw(dpc_status),
                );
            }
        });

        /// Acquires `state.lock`, signals `holder_locked_event`, and holds the lock until
        /// `release_lock_event` is signaled.
        ///
        /// # Safety
        ///
        /// `arg` must point to a valid, shared `State` that outlives this thread.
        unsafe extern "C" fn mutex_holder_entry(arg: *mut c_void) -> i32 {
            // SAFETY: Guaranteed by the caller (`_cleanup_threads` joins before `state` drops).
            let state = unsafe { &*arg.cast::<State>() };
            {
                ksync::lock!(let _guard = state.lock_lock());
                state.holder_locked_event.signal();
                let _ = state.release_lock_event.wait_infinite();
            }
            state.holder_released_event.signal();
            0
        }

        let holder_thread = unwrap_ok!(
            // SAFETY: A null `t` allocates the `Thread`, `mutex_holder_entry` only accesses `state`
            // via shared reference, blocking is permitted, no `ChainLockTransaction` is active, and
            // `_cleanup_threads` joins the thread (aborting on error) before `state` is dropped.
            unsafe {
                thread::create_etc(
                    core::ptr::null_mut(),
                    b"pinned-offline-mutex-holder",
                    Some(mutex_holder_entry),
                    state_ptr,
                    &scheduler::BaseProfile::new_fair(DEFAULT_PRIORITY),
                    None,
                )
            },
            "mutex_holder create failed"
        );
        // SAFETY: `holder_thread` was just created and has not been resumed, destroyed, or joined
        // yet, and no spinlocks, scheduler queue locks, or `ChainLockTransaction` are held.
        unsafe {
            let _ = holder_thread.set_cpu_affinity(cpu_num_to_mask(BOOT_CPU_ID));
            holder_thread.resume();
        }
        mutex_holder.set(Some(holder_thread));
        assert_ok!(
            state_ref.holder_locked_event.wait_infinite(),
            "waiting for holder_locked_event failed"
        );

        /// Blocks on `state.lock` while pinned to `target_cpu` and records the CPU before and
        /// after acquiring the lock.
        ///
        /// # Safety
        ///
        /// `arg` must point to a valid, shared `State` that outlives this thread.
        unsafe extern "C" fn mutex_waiter_entry(arg: *mut c_void) -> i32 {
            // SAFETY: Guaranteed by the caller (`_cleanup_threads` joins before `state` drops).
            let state = unsafe { &*arg.cast::<State>() };
            state.mutex_waiter_before_cpu.store(curr_cpu_num(), Ordering::Relaxed);
            ksync::lock!(let _guard = state.lock_lock());
            state.mutex_waiter_after_cpu.store(curr_cpu_num(), Ordering::Relaxed);
            0
        }

        let mutex_waiter_thread = unwrap_ok!(
            // SAFETY: A null `t` allocates the `Thread`, `mutex_waiter_entry` only accesses `state`
            // via shared reference, blocking is permitted, no `ChainLockTransaction` is active, and
            // `_cleanup_threads` joins the thread (aborting on error) before `state` is dropped.
            unsafe {
                thread::create_etc(
                    core::ptr::null_mut(),
                    b"pinned-offline-mutex-waiter",
                    Some(mutex_waiter_entry),
                    state_ptr,
                    &scheduler::BaseProfile::new_fair(DEFAULT_PRIORITY),
                    None,
                )
            },
            "mutex_waiter create failed"
        );
        // SAFETY: `mutex_waiter_thread` was just created and has not been resumed, destroyed, or
        // joined yet, and no spinlocks, scheduler queue locks, or `ChainLockTransaction` are held.
        unsafe {
            let _ = mutex_waiter_thread.set_cpu_affinity(cpu_num_to_mask(target_cpu));
            mutex_waiter_thread.resume();
        }
        mutex_waiter.set(Some(mutex_waiter_thread));

        /// Blocks on `state.wake_event` while pinned to `target_cpu` and records the CPU before
        /// and after waking.
        ///
        /// # Safety
        ///
        /// `arg` must point to a valid, shared `State` that outlives this thread.
        unsafe extern "C" fn event_waiter_entry(arg: *mut c_void) -> i32 {
            // SAFETY: Guaranteed by the caller (`_cleanup_threads` joins before `state` drops).
            let state = unsafe { &*arg.cast::<State>() };
            state.event_waiter_before_cpu.store(curr_cpu_num(), Ordering::Relaxed);
            let _ = state.wake_event.wait_infinite();
            state.event_waiter_after_cpu.store(curr_cpu_num(), Ordering::Relaxed);
            0
        }

        let event_waiter_thread = unwrap_ok!(
            // SAFETY: A null `t` allocates the `Thread`, `event_waiter_entry` only accesses `state`
            // via shared reference, blocking is permitted, no `ChainLockTransaction` is active, and
            // `_cleanup_threads` joins the thread (aborting on error) before `state` is dropped.
            unsafe {
                thread::create_etc(
                    core::ptr::null_mut(),
                    b"pinned-offline-event-waiter",
                    Some(event_waiter_entry),
                    state_ptr,
                    &scheduler::BaseProfile::new_fair(DEFAULT_PRIORITY),
                    None,
                )
            },
            "event_waiter create failed"
        );
        let event_waiter_migrate_fn =
            MigrateFn::new(move |_thread: &Thread, stage: MigrateStage| {
                // SAFETY: `state` (`Pin<Box<State>>`) remains valid and initialized in the same
                // allocation, and is never mutably borrowed or mutated outside `UnsafeCell`
                // (`Atomic*`/`Event`/`KMutex`), until `event_waiter_thread` is joined before
                // `state` is dropped.
                let state = unsafe { state_const.as_ref() };
                if stage == MigrateStage::Save {
                    state.migrate_save_calls.fetch_add(1, Ordering::Relaxed);
                } else if stage == MigrateStage::Restore {
                    state.migrate_restore_calls.fetch_add(1, Ordering::Relaxed);
                }
            });
        // SAFETY: `thread_lists_init` has completed; `event_waiter_thread` was just created and has
        // not been resumed, erased, destroyed, or joined yet; and no locks, spinlocks, scheduler
        // queue locks, or `ChainLockTransaction` are held.
        unsafe {
            let _ = event_waiter_thread.set_cpu_affinity(cpu_num_to_mask(target_cpu));
            event_waiter_thread.set_migrate_fn(Some(event_waiter_migrate_fn));
            event_waiter_thread.resume();
        }
        event_waiter.set(Some(event_waiter_thread));

        /// Runs ready on `target_cpu` across unplug/hotplug and records the post-hotplug CPU.
        ///
        /// # Safety
        ///
        /// `arg` must point to a valid, shared `State` that outlives this thread.
        unsafe extern "C" fn ready_pinned_entry(arg: *mut c_void) -> i32 {
            // SAFETY: Guaranteed by the caller (`_cleanup_threads` joins before `state` drops).
            let state = unsafe { &*arg.cast::<State>() };
            state.ready_pinned_before_cpu.store(curr_cpu_num(), Ordering::Relaxed);
            state.ready_pinned_started_event.signal();
            while !state.stop_ready_pinned.load(Ordering::Relaxed) {
                if state.observe_post_hotplug_cpu.load(Ordering::Relaxed) {
                    state.ready_pinned_after_cpu.store(curr_cpu_num(), Ordering::Relaxed);
                    break;
                }
                thread::r#yield();
            }
            0
        }

        let ready_pinned_thread = unwrap_ok!(
            // SAFETY: A null `t` allocates the `Thread`, `ready_pinned_entry` only accesses `state`
            // via shared reference, blocking is permitted, no `ChainLockTransaction` is active, and
            // `_cleanup_threads` joins the thread (aborting on error) before `state` is dropped.
            unsafe {
                thread::create_etc(
                    core::ptr::null_mut(),
                    b"pinned-offline-ready-worker",
                    Some(ready_pinned_entry),
                    state_ptr,
                    &scheduler::BaseProfile::new_fair(LOW_PRIORITY),
                    None,
                )
            },
            "ready_pinned create failed"
        );
        let ready_pinned_migrate_fn =
            MigrateFn::new(move |_thread: &Thread, stage: MigrateStage| {
                // SAFETY: `state` (`Pin<Box<State>>`) remains valid and initialized in the same
                // allocation, and is never mutably borrowed or mutated outside `UnsafeCell`
                // (`Atomic*`/`Event`/`KMutex`), until `ready_pinned_thread` is joined before
                // `state` is dropped.
                let state = unsafe { state_const.as_ref() };
                if stage == MigrateStage::Save {
                    state.ready_migrate_save_calls.fetch_add(1, Ordering::Relaxed);
                } else if stage == MigrateStage::Restore {
                    state.ready_migrate_restore_calls.fetch_add(1, Ordering::Relaxed);
                }
            });
        // SAFETY: `thread_lists_init` has completed; `ready_pinned_thread` was just created and has
        // not been resumed, erased, destroyed, or joined yet; and no locks, spinlocks, scheduler
        // queue locks, or `ChainLockTransaction` are held.
        unsafe {
            let _ = ready_pinned_thread.set_cpu_affinity(cpu_num_to_mask(target_cpu));
            ready_pinned_thread.set_migrate_fn(Some(ready_pinned_migrate_fn));
            ready_pinned_thread.resume();
        }
        ready_pinned.set(Some(ready_pinned_thread));

        // Wait until both waiters have run on `target_cpu` and entered `ThreadStateKind::Blocked`,
        // and `ready_pinned` has started executing on `target_cpu`.
        // SAFETY: `mutex_waiter_thread` has not been joined yet so it stays live, blocking is
        // permitted, and no locks or `ChainLockTransaction` are held.
        unsafe { wait_for_thread_state(mutex_waiter_thread, ThreadStateKind::Blocked) };
        // SAFETY: `event_waiter_thread` has not been joined yet so it stays live, blocking is
        // permitted, and no locks or `ChainLockTransaction` are held.
        unsafe { wait_for_thread_state(event_waiter_thread, ThreadStateKind::Blocked) };
        assert_ok!(
            state_ref.ready_pinned_started_event.wait_infinite(),
            "waiting for ready_pinned_started_event failed"
        );
        expect_eq!(target_cpu, state_ref.mutex_waiter_before_cpu.load(Ordering::Relaxed));
        expect_eq!(target_cpu, state_ref.event_waiter_before_cpu.load(Ordering::Relaxed));
        expect_eq!(target_cpu, state_ref.ready_pinned_before_cpu.load(Ordering::Relaxed));

        // Take `target_cpu` offline while the waiters are blocked and `ready_pinned` is runnable
        // on `target_cpu` (preempted into `ThreadStateKind::Ready` during unplug).
        assert_ok!(
            mp::unplug_cpu_mask(cpu_num_to_mask(target_cpu), ZX_TIME_INFINITE),
            "unplugging core failed"
        );
        assert_ok!(wait_for_cpu_offline(target_cpu), "waiting for core to go offline failed");
        assert_false!(scheduler::peek_is_active(target_cpu));
        // SAFETY: `ready_pinned_thread` has not been joined yet so it stays live, blocking is
        // permitted, and no locks or `ChainLockTransaction` are held.
        unsafe { wait_for_thread_state(ready_pinned_thread, ThreadStateKind::Ready) };
        expect_eq!(1u32, state_ref.ready_migrate_save_calls.load(Ordering::Relaxed));
        expect_eq!(0u32, state_ref.ready_migrate_restore_calls.load(Ordering::Relaxed));
        state_ref.observe_post_hotplug_cpu.store(true, Ordering::Relaxed);

        // Unblock both pinned waiters while `target_cpu` is offline:
        // 1. Releasing `state.lock` wakes `mutex_waiter` via `OwnedWaitQueue::wake_threads_locked`.
        // 2. Signaling `state.wake_event` wakes `event_waiter` via `WaitQueue::wake_one`.
        state_ref.release_lock_event.signal();
        assert_ok!(
            state_ref.holder_released_event.wait_infinite(),
            "waiting for holder_released_event failed"
        );
        state_ref.wake_event.signal();

        // All three pinned threads should now be queued in `ThreadStateKind::Ready` on inactive
        // `target_cpu`, and must not have run on any other CPU while `target_cpu` is offline.
        // SAFETY: `mutex_waiter_thread` has not been joined yet so it stays live, blocking is
        // permitted, and no locks or `ChainLockTransaction` are held.
        unsafe { wait_for_thread_state(mutex_waiter_thread, ThreadStateKind::Ready) };
        // SAFETY: `event_waiter_thread` has not been joined yet so it stays live, blocking is
        // permitted, and no locks or `ChainLockTransaction` are held.
        unsafe { wait_for_thread_state(event_waiter_thread, ThreadStateKind::Ready) };
        expect_eq!(INVALID_CPU, state_ref.mutex_waiter_after_cpu.load(Ordering::Relaxed));
        expect_eq!(INVALID_CPU, state_ref.event_waiter_after_cpu.load(Ordering::Relaxed));
        expect_eq!(INVALID_CPU, state_ref.ready_pinned_after_cpu.load(Ordering::Relaxed));

        // Hotplug `target_cpu` back online and verify that all queued pinned threads execute on it.
        assert_ok!(hotplug_core(target_cpu), "hotplugging core failed");
        wait_for_cpu_active(target_cpu);
        // Unplug shuts down DPC threads, which start after the core becomes active.
        assert_ok!(
            // SAFETY: Runs after `LK_INIT_LEVEL_THREADING` in normal thread context where
            // blocking is permitted with interrupts enabled and no spinlocks, thread list lock,
            // scheduler queue locks, or active `ChainLockTransaction` held.
            unsafe { wait_for_cpu_dpc_runner(target_cpu) },
            "waiting for core's dpc runner failed"
        );

        let holder_thread = unwrap_some!(mutex_holder.get());
        assert_ok!(
            // SAFETY: `holder_thread` was created and resumed above and has not been joined or
            // detached yet (clearing `mutex_holder` only after `join` succeeds ensures
            // `_cleanup_threads` would still join on a hypothetical `join` error), this thread is
            // not `holder_thread`, no `&Thread` or lock is held, and blocking is permitted with no
            // active `ChainLockTransaction`.
            unsafe { holder_thread.join(InstantMono::INFINITE) }.map(drop),
            "joining mutex_holder failed"
        );
        mutex_holder.set(None);
        let mutex_waiter_thread = unwrap_some!(mutex_waiter.get());
        assert_ok!(
            // SAFETY: `mutex_waiter_thread` was created and resumed above and has not been joined
            // or detached yet (clearing `mutex_waiter` only after `join` succeeds ensures
            // `_cleanup_threads` would still join on a hypothetical `join` error), this thread is
            // not `mutex_waiter_thread`, no `&Thread` or lock is held, and blocking is permitted
            // with no active `ChainLockTransaction`.
            unsafe { mutex_waiter_thread.join(InstantMono::INFINITE) }.map(drop),
            "joining mutex_waiter failed"
        );
        mutex_waiter.set(None);
        let event_waiter_thread = unwrap_some!(event_waiter.get());
        assert_ok!(
            // SAFETY: `event_waiter_thread` was created and resumed above and has not been joined
            // or detached yet (clearing `event_waiter` only after `join` succeeds ensures
            // `_cleanup_threads` would still join on a hypothetical `join` error), this thread is
            // not `event_waiter_thread`, no `&Thread` or lock is held, and blocking is permitted
            // with no active `ChainLockTransaction`.
            unsafe { event_waiter_thread.join(InstantMono::INFINITE) }.map(drop),
            "joining event_waiter failed"
        );
        event_waiter.set(None);
        let ready_pinned_thread = unwrap_some!(ready_pinned.get());
        assert_ok!(
            // SAFETY: `ready_pinned_thread` was created and resumed above and has not been joined
            // or detached yet (clearing `ready_pinned` only after `join` succeeds ensures
            // `_cleanup_threads` would still join on a hypothetical `join` error), this thread is
            // not `ready_pinned_thread`, no `&Thread` or lock is held, and blocking is permitted
            // with no active `ChainLockTransaction`.
            unsafe { ready_pinned_thread.join(InstantMono::INFINITE) }.map(drop),
            "joining ready_pinned failed"
        );
        ready_pinned.set(None);

        expect_eq!(target_cpu, state_ref.mutex_waiter_after_cpu.load(Ordering::Relaxed));
        expect_eq!(target_cpu, state_ref.event_waiter_after_cpu.load(Ordering::Relaxed));
        expect_eq!(target_cpu, state_ref.ready_pinned_after_cpu.load(Ordering::Relaxed));
        expect_eq!(1u32, state_ref.migrate_save_calls.load(Ordering::Relaxed));
        expect_eq!(1u32, state_ref.migrate_restore_calls.load(Ordering::Relaxed));
        expect_eq!(1u32, state_ref.ready_migrate_save_calls.load(Ordering::Relaxed));
        expect_eq!(1u32, state_ref.ready_migrate_restore_calls.load(Ordering::Relaxed));
    }

    // Regression test for https://fxbug.dev/578020457:
    // Verify that unpinned threads with migration functions neither livelock nor spin-deadlock when
    // woken (or unpinned via `set_cpu_affinity`) while their `last_cpu` is offline or deactivating:
    // 1. When `last_cpu` is already offline (`!is_active && !cpu_deactivating`) and a migration
    //    function is installed after `Thread::call_migrate_fn_for_cpu` has already completed,
    //    waking a blocked thread or widening an offline-enqueued thread's affinity places it
    //    directly onto an active CPU without hanging or running `MigrateStage::Save` on the wrong
    //    CPU.
    // 2. When `last_cpu` is deactivating in `Scheduler::migrate_unpinned_threads`, concurrent
    //    wakeups from other CPUs queue onto `last_cpu`'s `save_state_list_` without deadlocking
    //    against `Thread::call_migrate_fn_for_cpu` and preserve strict Save-on-`last_cpu` / Restore
    //    ordering.
    /// test unblocking unpinned migrating threads while their last core is offline
    #[test]
    fn mp_hotplug_unblock_unpinned_migrate_while_offline_test() {
        // TODO(https://fxbug.dev/42086046): Re-enable test on RISC-V.
        if cfg!(target_arch = "riscv64") {
            kprintln!(
                "skipping test mp_hotplug_unblock_unpinned_migrate_while_offline, hotplug only \
                 supported on x64 and arm64"
            );
            return true;
        }

        let online_secondaries: cpu_mask_t = mp::get_online_mask() & !cpu_num_to_mask(BOOT_CPU_ID);
        if online_secondaries == 0 {
            kprintln!(
                "skipping test mp_hotplug_unblock_unpinned_migrate_while_offline, not enough \
                 online cpus"
            );
            return true;
        }
        let target_cpu: cpu_num_t = highest_cpu_set(online_secondaries);

        // SAFETY: Kernel unit tests run after `LK_INIT_LEVEL_THREADING` in normal thread context
        // with no thread lock, scheduler queue lock, or `ChainLockTransaction` held.
        let previous_affinity =
            unsafe { ThreadPtr::current().set_cpu_affinity(cpu_num_to_mask(BOOT_CPU_ID)) };
        let _restore_affinity = zr::defer(move || {
            // SAFETY: Runs after `LK_INIT_LEVEL_THREADING` on the current test thread before
            // returning, with no thread lock, scheduler queue lock, or `ChainLockTransaction` held.
            let _ = unsafe { ThreadPtr::current().set_cpu_affinity(previous_affinity) };
        });

        /// `thread`, `state`, `save_pending`, and `exited` use `Atomic*` / `zr::AtomicConstPtr` for
        /// `Sync` interior mutability because `cleanup_threads` and already-resumed workers hold
        /// shared `&State` / `&WorkerSlot` references while subsequent slots are initialized and
        /// while `MigrateFn` mutates `save_pending` and `exited` across worker blocking points.
        /// `Ordering::Relaxed` suffices: `thread` is accessed only on the test runner thread,
        /// `state` is stored before `worker.resume()`, and `save_pending` and `exited` are mutated
        /// under each worker's `ChainLock` and read after `join()`.
        #[pin_data]
        struct WorkerSlot {
            thread: AtomicPtr<Thread>,
            state: zr::AtomicConstPtr<State>,
            #[pin]
            started_event: Event,
            #[pin]
            wake_event: AutounsignalEvent,
            wakeups: AtomicU64,
            save_calls: AtomicU64,
            restore_calls: AtomicU64,
            migrate_order_ok: AtomicBool,
            save_pending: AtomicBool,
            exited: AtomicBool,
        }

        const NUM_WORKERS: usize = 4;

        /// Relaxed atomic loads/stores on `State` are synchronized by `Event::wait_infinite()`,
        /// `wait_for_thread_state()` / `wait_for_cpu_offline()` lock acquisitions, and
        /// `ThreadPtr::join()`; per-worker `save_pending` and `exited` fields are accessed
        /// exclusively under each worker thread's `ChainLock` inside
        /// `Thread::call_migrate_fn_locked_ptr`. Heap-allocate `State` to keep this function's
        /// kernel stack frame small.
        #[pin_data]
        struct State {
            #[pin]
            offline_waiter_started_event: Event,
            #[pin]
            offline_waiter_wake_event: Event,
            #[pin]
            pinned_enqueued_started_event: Event,
            unpinned_observe_cpu: AtomicBool,
            stop_pinned_enqueued: AtomicBool,
            stop_workers: AtomicBool,
            offline_waiter_before_cpu: AtomicU32,
            offline_waiter_after_cpu: AtomicU32,
            pinned_enqueued_before_cpu: AtomicU32,
            pinned_enqueued_after_cpu: AtomicU32,
            offline_waiter_save_calls: AtomicU32,
            offline_waiter_restore_calls: AtomicU32,
            offline_waiter_exiting_calls: AtomicU32,
            offline_waiter_migrate_ok: AtomicBool,
            pinned_enqueued_save_calls: AtomicU32,
            pinned_enqueued_restore_calls: AtomicU32,
            pinned_enqueued_exiting_calls: AtomicU32,
            pinned_enqueued_migrate_ok: AtomicBool,
            #[pin]
            workers: [WorkerSlot; NUM_WORKERS],
        }

        let state = unwrap_ok!(
            kalloc::Box::try_pin_init(pin_init!(State {
                offline_waiter_started_event <- Event::init_unsignaled(),
                offline_waiter_wake_event <- Event::init_unsignaled(),
                pinned_enqueued_started_event <- Event::init_unsignaled(),
                unpinned_observe_cpu: AtomicBool::new(false),
                stop_pinned_enqueued: AtomicBool::new(false),
                stop_workers: AtomicBool::new(false),
                offline_waiter_before_cpu: AtomicU32::new(INVALID_CPU),
                offline_waiter_after_cpu: AtomicU32::new(INVALID_CPU),
                pinned_enqueued_before_cpu: AtomicU32::new(INVALID_CPU),
                pinned_enqueued_after_cpu: AtomicU32::new(INVALID_CPU),
                offline_waiter_save_calls: AtomicU32::new(0),
                offline_waiter_restore_calls: AtomicU32::new(0),
                offline_waiter_exiting_calls: AtomicU32::new(0),
                offline_waiter_migrate_ok: AtomicBool::new(true),
                pinned_enqueued_save_calls: AtomicU32::new(0),
                pinned_enqueued_restore_calls: AtomicU32::new(0),
                pinned_enqueued_exiting_calls: AtomicU32::new(0),
                pinned_enqueued_migrate_ok: AtomicBool::new(true),
                workers <- pin_init_array_from_fn::<_, NUM_WORKERS, _, _>(|_| {
                    pin_init!(WorkerSlot {
                        thread: AtomicPtr::new(core::ptr::null_mut()),
                        state: zr::AtomicConstPtr::new(core::ptr::null()),
                        started_event <- Event::init_unsignaled(),
                        wake_event <- AutounsignalEvent::init_unsignaled(),
                        wakeups: AtomicU64::new(0),
                        save_calls: AtomicU64::new(0),
                        restore_calls: AtomicU64::new(0),
                        migrate_order_ok: AtomicBool::new(true),
                        save_pending: AtomicBool::new(false),
                        exited: AtomicBool::new(false),
                    })
                }),
            })),
            "failed to allocate test state"
        );
        let state_ref: &State = state.as_ref().get_ref();
        let state_ptr: *mut c_void = core::ptr::from_ref(state_ref).cast_mut().cast();
        let state_const = SendConstPtr::from_ref(state_ref);

        let offline_waiter: Cell<Option<ThreadPtr>> = Cell::new(None);
        let pinned_enqueued: Cell<Option<ThreadPtr>> = Cell::new(None);
        let waker: Cell<Option<ThreadPtr>> = Cell::new(None);

        let mut cleanup_threads = zr::defer(|| {
            state_ref.stop_pinned_enqueued.store(true, Ordering::Relaxed);
            state_ref.stop_workers.store(true, Ordering::Relaxed);
            state_ref.offline_waiter_wake_event.signal();
            for slot in &state_ref.workers {
                slot.wake_event.signal();
            }
            if let Some(thread) = waker.take() {
                // SAFETY: `thread` was created and resumed below, `take()` ensures it is joined at
                // most once, this thread is not `thread`, no `&Thread` or lock is held, and
                // blocking is permitted with no active `ChainLockTransaction`.
                unsafe { thread.join(InstantMono::INFINITE) }.expect("joining waker failed");
            }
            for slot in &state_ref.workers {
                let raw = slot.thread.swap(core::ptr::null_mut(), Ordering::Relaxed);
                if !raw.is_null() {
                    slot.wake_event.signal();
                    // SAFETY: `raw` is non-null and was stored from a created and resumed
                    // `ThreadPtr`, and `swap` with null ensures it is joined at most once.
                    let thread = unsafe { ThreadPtr::from_raw(raw) }.unwrap();
                    // SAFETY: `thread` was resumed and taken exclusively above, this thread is not
                    // `thread`, no `&Thread` or lock is held, and blocking is permitted with no
                    // active `ChainLockTransaction`.
                    unsafe { thread.join(InstantMono::INFINITE) }.expect("joining worker failed");
                }
            }
            if let Some(thread) = offline_waiter.take() {
                // SAFETY: `thread` was created and resumed below, `take()` ensures it is joined at
                // most once, this thread is not `thread`, no `&Thread` or lock is held, and
                // blocking is permitted with no active `ChainLockTransaction`.
                unsafe { thread.join(InstantMono::INFINITE) }
                    .expect("joining offline_waiter failed");
            }
            if let Some(thread) = pinned_enqueued.take() {
                // SAFETY: `thread` was created and resumed below, `take()` ensures it is joined at
                // most once, this thread is not `thread`, no `&Thread` or lock is held, and
                // blocking is permitted with no active `ChainLockTransaction`.
                unsafe { thread.join(InstantMono::INFINITE) }
                    .expect("joining pinned_enqueued failed");
            }
        });

        // Declared after `cleanup_threads` so `target_cpu` is brought back online first during
        // unwinding.
        let _restore_target_cpu = zr::defer(move || {
            if !mp::is_cpu_online(target_cpu) {
                let status = hotplug_core(target_cpu);
                assert!(
                    status.is_ok(),
                    "hotplugging core {} failed: {}",
                    target_cpu,
                    Status::result_into_raw(status),
                );
                wait_for_cpu_active(target_cpu);
                // Unplug shuts down DPC threads, which start after the core becomes active.
                // SAFETY: `_restore_target_cpu` runs at test exit after `LK_INIT_LEVEL_THREADING`
                // in normal thread context where blocking is permitted with interrupts enabled and
                // no spinlocks, thread list lock, scheduler queue locks, or active
                // `ChainLockTransaction` held.
                let dpc_status = unsafe { wait_for_cpu_dpc_runner(target_cpu) };
                assert!(
                    dpc_status.is_ok(),
                    "waiting for core {}'s dpc runner failed: {}",
                    target_cpu,
                    Status::result_into_raw(dpc_status),
                );
            }
        });

        // Phase 1: Wake an unpinned thread (and unpin an offline-enqueued thread) whose `last_cpu`
        // is already offline and had a migration function installed after
        // `Thread::call_migrate_fn_for_cpu` ran.
        /// Blocks on `offline_waiter_wake_event` after running on `target_cpu` and records its
        /// post-wakeup CPU.
        ///
        /// # Safety
        ///
        /// `arg` must point to a valid, shared `State` that outlives this thread.
        unsafe extern "C" fn offline_waiter_entry(arg: *mut c_void) -> i32 {
            // SAFETY: Guaranteed by the caller (`cleanup_threads` joins before `state` drops).
            let state = unsafe { &*arg.cast::<State>() };
            state.offline_waiter_before_cpu.store(curr_cpu_num(), Ordering::Relaxed);
            state.offline_waiter_started_event.signal();
            let _ = state.offline_waiter_wake_event.wait_infinite();
            state.offline_waiter_after_cpu.store(curr_cpu_num(), Ordering::Relaxed);
            0
        }

        let offline_waiter_thread = unwrap_ok!(
            // SAFETY: A null `t` allocates the `Thread`, `offline_waiter_entry` only accesses
            // `state` via shared reference, blocking is permitted, no `ChainLockTransaction` is
            // active, and `cleanup_threads` joins the thread (aborting on error) before `state` is
            // dropped.
            unsafe {
                thread::create_etc(
                    core::ptr::null_mut(),
                    b"unpinned-offline-migrate-waiter",
                    Some(offline_waiter_entry),
                    state_ptr,
                    &scheduler::BaseProfile::new_fair(DEFAULT_PRIORITY),
                    None,
                )
            },
            "offline_waiter create failed"
        );
        // SAFETY: `offline_waiter_thread` was just created and has not been resumed, destroyed, or
        // joined yet, and no spinlocks, scheduler queue locks, or `ChainLockTransaction` are held.
        unsafe {
            let _ = offline_waiter_thread.set_soft_cpu_affinity(cpu_num_to_mask(target_cpu));
            offline_waiter_thread.resume();
        }
        offline_waiter.set(Some(offline_waiter_thread));

        /// Spins at `LOW_PRIORITY` on `target_cpu` until unpinned while `target_cpu` is offline.
        ///
        /// # Safety
        ///
        /// `arg` must point to a valid, shared `State` that outlives this thread.
        unsafe extern "C" fn pinned_enqueued_entry(arg: *mut c_void) -> i32 {
            // SAFETY: Guaranteed by the caller (`cleanup_threads` joins before `state` drops).
            let state = unsafe { &*arg.cast::<State>() };
            state.pinned_enqueued_before_cpu.store(curr_cpu_num(), Ordering::Relaxed);
            state.pinned_enqueued_started_event.signal();
            while !state.stop_pinned_enqueued.load(Ordering::Relaxed) {
                if state.unpinned_observe_cpu.load(Ordering::Relaxed) {
                    state.pinned_enqueued_after_cpu.store(curr_cpu_num(), Ordering::Relaxed);
                    break;
                }
                thread::r#yield();
            }
            0
        }

        let pinned_enqueued_thread = unwrap_ok!(
            // SAFETY: A null `t` allocates the `Thread`, `pinned_enqueued_entry` only accesses
            // `state` via shared reference, blocking is permitted, no `ChainLockTransaction` is
            // active, and `cleanup_threads` joins the thread (aborting on error) before `state` is
            // dropped.
            unsafe {
                thread::create_etc(
                    core::ptr::null_mut(),
                    b"unpin-while-offline-migrate-worker",
                    Some(pinned_enqueued_entry),
                    state_ptr,
                    &scheduler::BaseProfile::new_fair(LOW_PRIORITY),
                    None,
                )
            },
            "pinned_enqueued create failed"
        );
        // SAFETY: `pinned_enqueued_thread` was just created and has not been resumed, destroyed, or
        // joined yet, and no spinlocks, scheduler queue locks, or `ChainLockTransaction` are held.
        unsafe {
            let _ = pinned_enqueued_thread.set_cpu_affinity(cpu_num_to_mask(target_cpu));
            pinned_enqueued_thread.resume();
        }
        pinned_enqueued.set(Some(pinned_enqueued_thread));

        assert_ok!(
            state_ref.offline_waiter_started_event.wait_infinite(),
            "waiting for offline_waiter_started_event failed"
        );
        // SAFETY: `offline_waiter_thread` has not been joined yet so it stays live, blocking is
        // permitted, and no locks or `ChainLockTransaction` are held.
        unsafe { wait_for_thread_state(offline_waiter_thread, ThreadStateKind::Blocked) };
        assert_ok!(
            state_ref.pinned_enqueued_started_event.wait_infinite(),
            "waiting for pinned_enqueued_started_event failed"
        );
        expect_eq!(target_cpu, state_ref.offline_waiter_before_cpu.load(Ordering::Relaxed));
        expect_eq!(target_cpu, state_ref.pinned_enqueued_before_cpu.load(Ordering::Relaxed));

        assert_ok!(
            mp::unplug_cpu_mask(cpu_num_to_mask(target_cpu), ZX_TIME_INFINITE),
            "unplugging core failed"
        );
        assert_ok!(wait_for_cpu_offline(target_cpu), "waiting for core to go offline failed");
        assert_false!(scheduler::peek_is_active(target_cpu));
        // SAFETY: `pinned_enqueued_thread` has not been joined yet so it stays live, blocking is
        // permitted, and no locks or `ChainLockTransaction` are held.
        unsafe { wait_for_thread_state(pinned_enqueued_thread, ThreadStateKind::Ready) };

        // Install migration functions while `target_cpu` is offline so `last_cpu == target_cpu`
        // with `!migrate_pending`.
        let offline_waiter_migrate_fn =
            MigrateFn::new(move |thread: &Thread, stage: MigrateStage| {
                // SAFETY: `state` (`Pin<Box<State>>`) remains valid and initialized in the same
                // allocation, and is never mutably borrowed or mutated outside `UnsafeCell`
                // (`Atomic*`/`Event`), until `offline_waiter_thread` is joined before `state` is
                // dropped.
                let state = unsafe { state_const.as_ref() };
                // SAFETY: Per `ThreadPtr::set_migrate_fn`'s callback contract, migration callbacks
                // are invoked with an active `ChainLockTransaction`; `token` is callback-local and
                // does not outlive the active transaction.
                let token = unsafe { ChainLockTransaction::assert_active() };
                thread.lock().assert_held(&token);
                match stage {
                    MigrateStage::Save => {
                        // SAFETY: Per `ThreadPtr::set_migrate_fn` and
                        // `Thread::call_migrate_fn_locked_slow`, `thread`'s lock is held
                        // exclusively across this callback (preventing concurrent writes to
                        // `last_cpu`), the caller only borrows `thread.migrate_fn`, and this
                        // temporary proof is used solely for the shared `.last_cpu()` read
                        // accessor.
                        let last_cpu =
                            unsafe { ChainLockProof::assume_synchronized_read_only(thread) }
                                .last_cpu();
                        if curr_cpu_num() != last_cpu {
                            state.offline_waiter_migrate_ok.store(false, Ordering::Relaxed);
                        }
                        state.offline_waiter_save_calls.fetch_add(1, Ordering::Relaxed);
                    }
                    MigrateStage::Restore => {
                        state.offline_waiter_restore_calls.fetch_add(1, Ordering::Relaxed);
                        state.offline_waiter_migrate_ok.store(false, Ordering::Relaxed);
                    }
                    MigrateStage::Exiting => {
                        state.offline_waiter_exiting_calls.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        // SAFETY: `thread_lists_init` has completed; `offline_waiter_thread` is alive (`Blocked`
        // on `offline_waiter_wake_event`) and not erased; and no thread lock, list lock, scheduler
        // queue lock, or `ChainLockTransaction` is held.
        unsafe {
            offline_waiter_thread.set_migrate_fn(Some(offline_waiter_migrate_fn));
        }

        state_ref.offline_waiter_wake_event.signal();
        let offline_waiter_thread = unwrap_some!(offline_waiter.get());
        assert_ok!(
            // SAFETY: `offline_waiter_thread` was created and resumed above and has not been joined
            // or detached yet (clearing `offline_waiter` only after `join` succeeds ensures
            // `cleanup_threads` would still join on a hypothetical `join` error), this thread is
            // not `offline_waiter_thread`, no `&Thread` or lock is held, and blocking is permitted
            // with no active `ChainLockTransaction`.
            unsafe { offline_waiter_thread.join(InstantMono::INFINITE) }.map(drop),
            "joining offline_waiter failed"
        );
        offline_waiter.set(None);

        let waiter_after_cpu: cpu_num_t =
            state_ref.offline_waiter_after_cpu.load(Ordering::Relaxed);
        expect_ne!(INVALID_CPU, waiter_after_cpu);
        expect_ne!(target_cpu, waiter_after_cpu);
        expect_eq!(0u32, state_ref.offline_waiter_save_calls.load(Ordering::Relaxed));
        expect_eq!(0u32, state_ref.offline_waiter_restore_calls.load(Ordering::Relaxed));
        expect_eq!(1u32, state_ref.offline_waiter_exiting_calls.load(Ordering::Relaxed));
        expect_true!(state_ref.offline_waiter_migrate_ok.load(Ordering::Relaxed));

        let pinned_enqueued_migrate_fn =
            MigrateFn::new(move |thread: &Thread, stage: MigrateStage| {
                // SAFETY: `state` (`Pin<Box<State>>`) remains valid and initialized in the same
                // allocation, and is never mutably borrowed or mutated outside `UnsafeCell`
                // (`Atomic*`/`Event`), until `pinned_enqueued_thread` is joined before `state` is
                // dropped.
                let state = unsafe { state_const.as_ref() };
                // SAFETY: Per `ThreadPtr::set_migrate_fn`'s callback contract, migration callbacks
                // are invoked with an active `ChainLockTransaction`; `token` is callback-local and
                // does not outlive the active transaction.
                let token = unsafe { ChainLockTransaction::assert_active() };
                thread.lock().assert_held(&token);
                match stage {
                    MigrateStage::Save => {
                        // SAFETY: Per `ThreadPtr::set_migrate_fn` and
                        // `Thread::call_migrate_fn_locked_slow`, `thread`'s lock is held
                        // exclusively across this callback (preventing concurrent writes to
                        // `last_cpu`), the caller only borrows `thread.migrate_fn`, and this
                        // temporary proof is used solely for the shared `.last_cpu()` read
                        // accessor.
                        let last_cpu =
                            unsafe { ChainLockProof::assume_synchronized_read_only(thread) }
                                .last_cpu();
                        if curr_cpu_num() != last_cpu {
                            state.pinned_enqueued_migrate_ok.store(false, Ordering::Relaxed);
                        }
                        state.pinned_enqueued_save_calls.fetch_add(1, Ordering::Relaxed);
                    }
                    MigrateStage::Restore => {
                        state.pinned_enqueued_restore_calls.fetch_add(1, Ordering::Relaxed);
                        state.pinned_enqueued_migrate_ok.store(false, Ordering::Relaxed);
                    }
                    MigrateStage::Exiting => {
                        state.pinned_enqueued_exiting_calls.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        // SAFETY: `thread_lists_init` has completed; `pinned_enqueued_thread` is alive (`Ready` on
        // offline `target_cpu`) and not erased; and no thread lock, list lock, scheduler queue
        // lock, or `ChainLockTransaction` is held.
        unsafe {
            pinned_enqueued_thread.set_migrate_fn(Some(pinned_enqueued_migrate_fn));
        }

        state_ref.unpinned_observe_cpu.store(true, Ordering::Relaxed);
        // SAFETY: `pinned_enqueued_thread` is alive and no thread lock, scheduler queue lock, or
        // `ChainLockTransaction` is held.
        unsafe {
            let _ = pinned_enqueued_thread.set_cpu_affinity(CPU_MASK_ALL);
        }
        let pinned_enqueued_thread = unwrap_some!(pinned_enqueued.get());
        assert_ok!(
            // SAFETY: `pinned_enqueued_thread` was created and resumed above and has not been
            // joined or detached yet (clearing `pinned_enqueued` only after `join` succeeds ensures
            // `cleanup_threads` would still join on a hypothetical `join` error), this thread is
            // not `pinned_enqueued_thread`, no `&Thread` or lock is held, and blocking is permitted
            // with no active `ChainLockTransaction`.
            unsafe { pinned_enqueued_thread.join(InstantMono::INFINITE) }.map(drop),
            "joining pinned_enqueued failed"
        );
        pinned_enqueued.set(None);

        let enqueued_after_cpu: cpu_num_t =
            state_ref.pinned_enqueued_after_cpu.load(Ordering::Relaxed);
        expect_ne!(INVALID_CPU, enqueued_after_cpu);
        expect_ne!(target_cpu, enqueued_after_cpu);
        expect_eq!(0u32, state_ref.pinned_enqueued_save_calls.load(Ordering::Relaxed));
        expect_eq!(0u32, state_ref.pinned_enqueued_restore_calls.load(Ordering::Relaxed));
        expect_eq!(1u32, state_ref.pinned_enqueued_exiting_calls.load(Ordering::Relaxed));
        expect_true!(state_ref.pinned_enqueued_migrate_ok.load(Ordering::Relaxed));

        assert_ok!(hotplug_core(target_cpu), "hotplugging core failed");
        wait_for_cpu_active(target_cpu);
        // Unplug shuts down DPC threads, which start after the core becomes active.
        assert_ok!(
            // SAFETY: Runs after `LK_INIT_LEVEL_THREADING` in normal thread context where
            // blocking is permitted with interrupts enabled and no spinlocks, thread list lock,
            // scheduler queue locks, or active `ChainLockTransaction` held.
            unsafe { wait_for_cpu_dpc_runner(target_cpu) },
            "waiting for core's dpc runner failed"
        );

        // Phase 2: Repeatedly unplug and hotplug `target_cpu` while unpinned workers with
        // migration functions block on `target_cpu` and are woken concurrently from `BOOT_CPU_ID`.
        for slot in &state_ref.workers {
            slot.state.store(core::ptr::from_ref(state_ref), Ordering::Relaxed);
            /// Alternates waiting on `slot.wake_event` and sleeping until `stop_workers` is set.
            ///
            /// # Safety
            ///
            /// `arg` must point to a valid, shared `WorkerSlot` inside `state` (with `slot.state`
            /// initialized to `state`) that outlives this thread.
            unsafe extern "C" fn worker_entry(arg: *mut c_void) -> i32 {
                // SAFETY: Guaranteed by the caller (`cleanup_threads` joins before `state` drops).
                let slot = unsafe { &*arg.cast::<WorkerSlot>() };
                // SAFETY: `slot.state` was stored with `state_ref` before `worker.resume()`
                // (whose release/acquire synchronization ensures this `Relaxed` load sees non-null
                // `state_ref`), and `state` outlives all workers via `cleanup_threads`.
                let state = unsafe { &*slot.state.load(Ordering::Relaxed) };
                slot.started_event.signal();
                let mut iteration: u64 = 0;
                while !state.stop_workers.load(Ordering::Relaxed) {
                    iteration += 1;
                    if iteration.is_multiple_of(2) {
                        let _ = slot.wake_event.wait(&Deadline::after_mono(
                            DurationMono::from_micros(150),
                            TimerSlack::none(),
                        ));
                    } else {
                        let _ = sleep_relative(DurationMono::from_micros(50));
                    }
                    slot.wakeups.fetch_add(1, Ordering::Relaxed);
                }
                0
            }

            let slot_ptr: *mut c_void = core::ptr::from_ref(slot).cast_mut().cast();
            let worker = unwrap_ok!(
                // SAFETY: A null `t` allocates the `Thread`, `worker_entry` only accesses `slot`
                // and `state` via shared references, blocking is permitted, no
                // `ChainLockTransaction` is active, and `cleanup_threads` joins the worker
                // (aborting on error) before `state` is dropped.
                unsafe {
                    thread::create_etc(
                        core::ptr::null_mut(),
                        b"unpinned-deactivating-migrate-worker",
                        Some(worker_entry),
                        slot_ptr,
                        &scheduler::BaseProfile::new_fair(DEFAULT_PRIORITY),
                        None,
                    )
                },
                "worker create failed"
            );
            let slot_const = SendConstPtr::from_ref(slot);
            let worker_migrate_fn = MigrateFn::new(move |thread: &Thread, stage: MigrateStage| {
                // SAFETY: `state` (`Pin<Box<State>>`) remains valid and initialized in the
                // same allocation, and `WorkerSlot` is never mutably borrowed or mutated
                // outside `UnsafeCell` (`Atomic*`/`Event`), until all workers are joined
                // before `state` is dropped.
                let slot = unsafe { slot_const.as_ref() };
                // SAFETY: Per `ThreadPtr::set_migrate_fn`'s callback contract, migration
                // callbacks are invoked with an active `ChainLockTransaction`; `token` is
                // callback-local and does not outlive the active transaction.
                let token = unsafe { ChainLockTransaction::assert_active() };
                thread.lock().assert_held(&token);
                match stage {
                    MigrateStage::Save => {
                        // SAFETY: Per `ThreadPtr::set_migrate_fn` and
                        // `Thread::call_migrate_fn_locked_slow`, `thread`'s lock is held
                        // exclusively across this callback (preventing concurrent writes to
                        // `last_cpu`), the caller only borrows `thread.migrate_fn`, and this
                        // temporary proof is used solely for the shared `.last_cpu()` read
                        // accessor.
                        let last_cpu =
                            unsafe { ChainLockProof::assume_synchronized_read_only(thread) }
                                .last_cpu();
                        if slot.exited.load(Ordering::Relaxed)
                            || slot.save_pending.load(Ordering::Relaxed)
                            || curr_cpu_num() != last_cpu
                        {
                            slot.migrate_order_ok.store(false, Ordering::Relaxed);
                        }
                        slot.save_pending.store(true, Ordering::Relaxed);
                        slot.save_calls.fetch_add(1, Ordering::Relaxed);
                    }
                    MigrateStage::Restore => {
                        if slot.exited.load(Ordering::Relaxed)
                            || !slot.save_pending.load(Ordering::Relaxed)
                        {
                            slot.migrate_order_ok.store(false, Ordering::Relaxed);
                        }
                        slot.save_pending.store(false, Ordering::Relaxed);
                        slot.restore_calls.fetch_add(1, Ordering::Relaxed);
                    }
                    MigrateStage::Exiting => {
                        if slot.exited.load(Ordering::Relaxed)
                            || slot.save_pending.load(Ordering::Relaxed)
                        {
                            slot.migrate_order_ok.store(false, Ordering::Relaxed);
                        }
                        slot.exited.store(true, Ordering::Relaxed);
                    }
                }
            });
            // SAFETY: `thread_lists_init` has completed; `worker` was just created and has not been
            // resumed, erased, destroyed, or joined yet; and no locks, spinlocks, scheduler queue
            // locks, or `ChainLockTransaction` are held.
            unsafe {
                let _ = worker.set_cpu_affinity(cpu_num_to_mask(target_cpu));
                worker.set_migrate_fn(Some(worker_migrate_fn));
                worker.resume();
            }
            slot.thread.store(worker.as_raw(), Ordering::Relaxed);
        }

        for slot in &state_ref.workers {
            assert_ok!(
                slot.started_event.wait_infinite(),
                "waiting for worker to start on target_cpu failed"
            );
            let raw = slot.thread.load(Ordering::Relaxed);
            let worker = unwrap_some!(
                if raw.is_null() {
                    None
                } else {
                    // SAFETY: `raw` is non-null and was populated with a valid resumed `ThreadPtr`
                    // in the loop above, and has not been joined yet.
                    unsafe { ThreadPtr::from_raw(raw) }
                },
                "worker slot thread pointer should be populated"
            );
            // SAFETY: `worker` is alive and no thread lock, scheduler queue lock, or
            // `ChainLockTransaction` is held.
            unsafe {
                let _ = worker.set_cpu_affinity(CPU_MASK_ALL);
                let _ = worker.set_soft_cpu_affinity(cpu_num_to_mask(target_cpu));
            }
        }

        /// Signals every worker's `wake_event` from `BOOT_CPU_ID` until `stop_workers` is set.
        ///
        /// # Safety
        ///
        /// `arg` must point to a valid, shared `State` that outlives this thread.
        unsafe extern "C" fn waker_entry(arg: *mut c_void) -> i32 {
            // SAFETY: Guaranteed by the caller (`cleanup_threads` joins before `state` drops).
            let state = unsafe { &*arg.cast::<State>() };
            while !state.stop_workers.load(Ordering::Relaxed) {
                for slot in &state.workers {
                    slot.wake_event.signal();
                }
                let _ = sleep_relative(DurationMono::from_micros(25));
            }
            0
        }

        let waker_thread = unwrap_ok!(
            // SAFETY: A null `t` allocates the `Thread`, `waker_entry` only accesses `state` via
            // shared reference, blocking is permitted, no `ChainLockTransaction` is active, and
            // `cleanup_threads` joins `waker` (aborting on error) before `state` is dropped.
            unsafe {
                thread::create_etc(
                    core::ptr::null_mut(),
                    b"unpinned-deactivating-migrate-waker",
                    Some(waker_entry),
                    state_ptr,
                    &scheduler::BaseProfile::new_fair(DEFAULT_PRIORITY),
                    None,
                )
            },
            "waker create failed"
        );
        // SAFETY: `waker_thread` was just created and has not been resumed, destroyed, or joined
        // yet, and no spinlocks, scheduler queue locks, or `ChainLockTransaction` are held.
        unsafe {
            let _ = waker_thread.set_cpu_affinity(cpu_num_to_mask(BOOT_CPU_ID));
            waker_thread.resume();
        }
        waker.set(Some(waker_thread));

        const UNPLUG_CYCLES: i32 = 20;
        for _ in 0..UNPLUG_CYCLES {
            let _ = sleep_relative(DurationMono::from_micros(250));
            assert_ok!(
                mp::unplug_cpu_mask(cpu_num_to_mask(target_cpu), ZX_TIME_INFINITE),
                "unplugging core failed"
            );
            assert_ok!(wait_for_cpu_offline(target_cpu), "waiting for core to go offline failed");
            assert_ok!(hotplug_core(target_cpu), "hotplugging core failed");
            wait_for_cpu_active(target_cpu);
            // Unplug shuts down DPC threads, which start after the core becomes active.
            assert_ok!(
                // SAFETY: Runs after `LK_INIT_LEVEL_THREADING` in normal thread context where
                // blocking is permitted with interrupts enabled and no spinlocks, thread list
                // lock, scheduler queue locks, or active `ChainLockTransaction` held.
                unsafe { wait_for_cpu_dpc_runner(target_cpu) },
                "waiting for core's dpc runner failed"
            );
        }

        cleanup_threads.call();

        let mut total_wakeups: u64 = 0;
        let mut total_save_calls: u64 = 0;
        let mut total_restore_calls: u64 = 0;
        for slot in &state_ref.workers {
            total_wakeups += slot.wakeups.load(Ordering::Relaxed);
            total_save_calls += slot.save_calls.load(Ordering::Relaxed);
            total_restore_calls += slot.restore_calls.load(Ordering::Relaxed);
            expect_true!(slot.migrate_order_ok.load(Ordering::Relaxed));
        }
        expect_ge!(total_wakeups, NUM_WORKERS as u64);
        expect_ge!(total_save_calls, NUM_WORKERS as u64);
        expect_eq!(total_save_calls, total_restore_calls);
    }

    // Regression test for https://fxbug.dev/578020457 and https://fxbug.dev/448121736:
    // Repeatedly unplug and hotplug a secondary CPU while unpinned fair threads of widely
    // differing weights (driving `fair_affine_transform.slope < 1.0`) actively run on and migrate
    // off that CPU. Verifies that `Scheduler::migrate_unpinned_threads` converts fair start/finish
    // times from variable to monotonic time only once rather than corrupting the fair timeline
    // across unplug cycles.
    /// test fair timeline integrity across repeated core unplug migrations
    #[test]
    fn mp_hotplug_migrate_fair_timeline_test() {
        // TODO(https://fxbug.dev/42086046): Re-enable test on RISC-V.
        if cfg!(target_arch = "riscv64") {
            kprintln!(
                "skipping test mp_hotplug_migrate_fair_timeline, hotplug only supported on x64 \
                 and arm64"
            );
            return true;
        }

        let online_secondaries: cpu_mask_t = mp::get_online_mask() & !cpu_num_to_mask(BOOT_CPU_ID);
        if online_secondaries == 0 {
            kprintln!("skipping test mp_hotplug_migrate_fair_timeline, not enough online cpus");
            return true;
        }
        let target_cpu: cpu_num_t = highest_cpu_set(online_secondaries);

        // SAFETY: Kernel unit tests run after `LK_INIT_LEVEL_THREADING` in normal thread context
        // with no thread lock, scheduler queue lock, or `ChainLockTransaction` held.
        let previous_affinity =
            unsafe { ThreadPtr::current().set_cpu_affinity(cpu_num_to_mask(BOOT_CPU_ID)) };
        let _restore_affinity = zr::defer(move || {
            // SAFETY: Runs after `LK_INIT_LEVEL_THREADING` on the current test thread before
            // returning, with no thread lock, scheduler queue lock, or `ChainLockTransaction` held.
            let _ = unsafe { ThreadPtr::current().set_cpu_affinity(previous_affinity) };
        });

        /// `stop` is a monotonic cancellation flag and `iterations` is read after
        /// `ThreadPtr::join()`.
        struct State {
            stop: AtomicBool,
            iterations: AtomicU64,
        }
        let state = State { stop: AtomicBool::new(false), iterations: AtomicU64::new(0) };
        let state_ptr: *mut c_void = core::ptr::from_ref(&state).cast_mut().cast();

        /// Combining `LOWEST_PRIORITY` and `HIGHEST_PRIORITY` fair threads with soft affinity
        /// preferring `target_cpu` guarantees `min_weight < weight_total * K_MIN_FAIR_UTILIZATION`
        /// on `target_cpu`, forcing `fair_affine_transform.slope` well below 1.0 while keeping
        /// `hard_affinity` unpinned so `migrate_unpinned_threads` migrates them on each unplug
        /// cycle.
        const PRIORITIES: [i32; 5] = [
            LOWEST_PRIORITY,
            LOWEST_PRIORITY,
            DEFAULT_PRIORITY,
            HIGHEST_PRIORITY,
            HIGHEST_PRIORITY,
        ];
        let workers: [Cell<Option<ThreadPtr>>; PRIORITIES.len()] =
            [const { Cell::new(None) }; PRIORITIES.len()];

        let mut stop_workers = zr::defer(|| {
            state.stop.store(true, Ordering::Relaxed);
            for worker in &workers {
                if let Some(thread) = worker.take() {
                    // SAFETY: `thread` was created and resumed below, `take()` ensures it is joined
                    // at most once, this thread is not `thread`, no `&Thread` or lock is held, and
                    // blocking is permitted with no active `ChainLockTransaction`.
                    unsafe { thread.join(InstantMono::INFINITE) }.expect("joining worker failed");
                }
            }
        });

        let _restore_target_cpu = zr::defer(move || {
            if !mp::is_cpu_online(target_cpu) {
                let status = hotplug_core(target_cpu);
                assert!(
                    status.is_ok(),
                    "hotplugging core {} failed: {}",
                    target_cpu,
                    Status::result_into_raw(status),
                );
                wait_for_cpu_active(target_cpu);
                // Unplug shuts down DPC threads, which start after the core becomes active.
                // SAFETY: `_restore_target_cpu` runs at test exit after `LK_INIT_LEVEL_THREADING`
                // in normal thread context where blocking is permitted with interrupts enabled and
                // no spinlocks, thread list lock, scheduler queue locks, or active
                // `ChainLockTransaction` held.
                let dpc_status = unsafe { wait_for_cpu_dpc_runner(target_cpu) };
                assert!(
                    dpc_status.is_ok(),
                    "waiting for core {}'s dpc runner failed: {}",
                    target_cpu,
                    Status::result_into_raw(dpc_status),
                );
            }
        });

        for (worker_slot, &priority) in workers.iter().zip(&PRIORITIES) {
            /// Yields in a loop while counting iterations until `state.stop` is set.
            ///
            /// # Safety
            ///
            /// `arg` must point to a valid, shared `State` that outlives this thread.
            unsafe extern "C" fn worker_entry(arg: *mut c_void) -> i32 {
                // SAFETY: Guaranteed by the caller (`stop_workers` joins before `state` drops).
                let state = unsafe { &*arg.cast::<State>() };
                while !state.stop.load(Ordering::Relaxed) {
                    state.iterations.fetch_add(1, Ordering::Relaxed);
                    thread::r#yield();
                }
                0
            }

            let worker = unwrap_ok!(
                // SAFETY: A null `t` allocates the `Thread`, `worker_entry` only accesses `state`
                // via shared reference, blocking is permitted, no `ChainLockTransaction` is active,
                // and `stop_workers` joins each worker (aborting on error) before `state` goes out
                // of scope.
                unsafe {
                    thread::create_etc(
                        core::ptr::null_mut(),
                        b"mp-hotplug-fair-timeline-worker",
                        Some(worker_entry),
                        state_ptr,
                        &scheduler::BaseProfile::new_fair(priority),
                        None,
                    )
                },
                "worker create failed"
            );
            // SAFETY: `worker` was just created and has not been resumed, destroyed, or joined yet,
            // and no spinlocks, scheduler queue locks, or `ChainLockTransaction` are held.
            unsafe {
                let _ = worker.set_soft_cpu_affinity(cpu_num_to_mask(target_cpu));
                worker.resume();
            }
            worker_slot.set(Some(worker));
        }

        const UNPLUG_CYCLES: i32 = 25;
        for _ in 0..UNPLUG_CYCLES {
            let _ = sleep_relative(DurationMono::from_millis(1));
            assert_ok!(
                mp::unplug_cpu_mask(cpu_num_to_mask(target_cpu), ZX_TIME_INFINITE),
                "unplugging core failed"
            );
            assert_ok!(wait_for_cpu_offline(target_cpu), "waiting for core to go offline failed");
            assert_ok!(hotplug_core(target_cpu), "hotplugging core failed");
            wait_for_cpu_active(target_cpu);
            // Unplug shuts down DPC threads, which start after the core becomes active.
            assert_ok!(
                // SAFETY: Runs after `LK_INIT_LEVEL_THREADING` in normal thread context where
                // blocking is permitted with interrupts enabled and no spinlocks, thread list
                // lock, scheduler queue locks, or active `ChainLockTransaction` held.
                unsafe { wait_for_cpu_dpc_runner(target_cpu) },
                "waiting for core's dpc runner failed"
            );
        }

        stop_workers.call();
        expect_gt!(state.iterations.load(Ordering::Relaxed), 0u64);
    }

    // Regression test for https://fxbug.dev/578020457:
    // Repeatedly unplug and hotplug `target_cpu` while worker threads on other active CPUs
    // transition their own soft affinity to `target_cpu` and yield. If `target_cpu` deactivates
    // after `Scheduler::needs_migration_local` initiates migration away from the current CPU,
    // target selection falls back to hard affinity and may re-select the current CPU; verifies that
    // `Scheduler::reschedule_common` does not assert, invoke a spurious `MigrateStage::Save`
    // without leaving the CPU, or lose the local reschedule.
    /// test soft affinity migration fallback during core unplug
    #[test]
    fn mp_hotplug_soft_affinity_migration_fallback_test() {
        // TODO(https://fxbug.dev/42086046): Re-enable test on RISC-V.
        if cfg!(target_arch = "riscv64") {
            kprintln!(
                "skipping test mp_hotplug_soft_affinity_migration_fallback, hotplug only \
                 supported on x64 and arm64"
            );
            return true;
        }

        let online_secondaries: cpu_mask_t = mp::get_online_mask() & !cpu_num_to_mask(BOOT_CPU_ID);
        if online_secondaries == 0 {
            kprintln!(
                "skipping test mp_hotplug_soft_affinity_migration_fallback, not enough online cpus"
            );
            return true;
        }
        let target_cpu: cpu_num_t = highest_cpu_set(online_secondaries);

        // SAFETY: Kernel unit tests run after `LK_INIT_LEVEL_THREADING` in normal thread context
        // with no thread lock, scheduler queue lock, or `ChainLockTransaction` held.
        let previous_affinity =
            unsafe { ThreadPtr::current().set_cpu_affinity(cpu_num_to_mask(BOOT_CPU_ID)) };
        let _restore_affinity = zr::defer(move || {
            // SAFETY: Runs after `LK_INIT_LEVEL_THREADING` on the current test thread before
            // returning, with no thread lock, scheduler queue lock, or `ChainLockTransaction` held.
            let _ = unsafe { ThreadPtr::current().set_cpu_affinity(previous_affinity) };
        });

        /// `thread`, `state`, `home_cpu`, `save_pending`, and `exited` use `Atomic*` /
        /// `zr::AtomicConstPtr` for `Sync` interior mutability because `stop_workers` and
        /// already-resumed workers hold shared `&State` / `&WorkerSlot` references while subsequent
        /// slots are initialized and while `MigrateFn` mutates `save_pending` and `exited` across
        /// worker reschedules. `Ordering::Relaxed` suffices: `thread` is accessed only on the test
        /// runner thread, `state` and `home_cpu` are stored before `worker.resume()`, and
        /// `save_pending` and `exited` are mutated under each worker's `ChainLock` and read after
        /// `join()`.
        #[pin_data]
        struct WorkerSlot {
            thread: AtomicPtr<Thread>,
            state: zr::AtomicConstPtr<State>,
            home_cpu: AtomicU32,
            migrate_save_calls: AtomicU64,
            migrate_order_ok: AtomicBool,
            save_pending: AtomicBool,
            exited: AtomicBool,
            #[pin]
            _pin: PhantomData<PhantomPinned>,
        }

        /// `stop` is a monotonic cancellation flag, `probe_target` is an advisory toggle carrying
        /// no dependent state, and `iterations`, `migrate_save_calls`, and `migrate_order_ok` are
        /// read after `ThreadPtr::join()`. Heap-allocate `State` to keep this function's kernel
        /// stack frame small.
        #[pin_data]
        struct State {
            stop: AtomicBool,
            probe_target: AtomicBool,
            iterations: AtomicU64,
            #[pin]
            workers: [WorkerSlot; SMP_MAX_CPUS],
            target_cpu: cpu_num_t,
        }

        let state = unwrap_ok!(
            kalloc::Box::try_pin_init(pin_init!(State {
                stop: AtomicBool::new(false),
                probe_target: AtomicBool::new(false),
                iterations: AtomicU64::new(0),
                workers <- pin_init_array_from_fn::<_, SMP_MAX_CPUS, _, _>(|_| {
                    pin_init!(WorkerSlot {
                        thread: AtomicPtr::new(core::ptr::null_mut()),
                        state: zr::AtomicConstPtr::new(core::ptr::null()),
                        home_cpu: AtomicU32::new(INVALID_CPU),
                        migrate_save_calls: AtomicU64::new(0),
                        migrate_order_ok: AtomicBool::new(true),
                        save_pending: AtomicBool::new(false),
                        exited: AtomicBool::new(false),
                        _pin: PhantomData,
                    })
                }),
                target_cpu,
            })),
            "failed to allocate test state"
        );
        let state_ref: &State = state.as_ref().get_ref();

        let mut stop_workers = zr::defer(|| {
            state_ref.stop.store(true, Ordering::Relaxed);
            for slot in &state_ref.workers {
                let raw = slot.thread.swap(core::ptr::null_mut(), Ordering::Relaxed);
                if !raw.is_null() {
                    // SAFETY: `raw` is non-null and was stored from a created and resumed
                    // `ThreadPtr`, and `swap` with null ensures it is joined at most once.
                    let thread = unsafe { ThreadPtr::from_raw(raw) }.unwrap();
                    // SAFETY: `thread` was resumed and taken exclusively above, this thread is not
                    // `thread`, no `&Thread` or lock is held, and blocking is permitted with no
                    // active `ChainLockTransaction`.
                    unsafe { thread.join(InstantMono::INFINITE) }.expect("joining worker failed");
                }
            }
        });

        let _restore_target_cpu = zr::defer(move || {
            if !mp::is_cpu_online(target_cpu) {
                let status = hotplug_core(target_cpu);
                assert!(
                    status.is_ok(),
                    "hotplugging core {} failed: {}",
                    target_cpu,
                    Status::result_into_raw(status),
                );
                wait_for_cpu_active(target_cpu);
                // Unplug shuts down DPC threads, which start after the core becomes active.
                // SAFETY: `_restore_target_cpu` runs at test exit after `LK_INIT_LEVEL_THREADING`
                // in normal thread context where blocking is permitted with interrupts enabled and
                // no spinlocks, thread list lock, scheduler queue locks, or active
                // `ChainLockTransaction` held.
                let dpc_status = unsafe { wait_for_cpu_dpc_runner(target_cpu) };
                assert!(
                    dpc_status.is_ok(),
                    "waiting for core {}'s dpc runner failed: {}",
                    target_cpu,
                    Status::result_into_raw(dpc_status),
                );
            }
        });

        let mut remaining_cpus: cpu_mask_t = mp::get_online_mask() & !cpu_num_to_mask(target_cpu);
        let mut worker_index: usize = 0;
        while remaining_cpus != 0 {
            let home_cpu: cpu_num_t = lowest_cpu_set(remaining_cpus);
            remaining_cpus &= !cpu_num_to_mask(home_cpu);

            let slot: &WorkerSlot = &state_ref.workers[worker_index];
            slot.state.store(core::ptr::from_ref(state_ref), Ordering::Relaxed);
            slot.home_cpu.store(home_cpu, Ordering::Relaxed);
            /// Alternates soft affinity between `home_cpu` and `target_cpu` while yielding.
            ///
            /// # Safety
            ///
            /// Must run in normal thread context after `LK_INIT_LEVEL_THREADING`, and `arg` must
            /// point to a valid, shared `WorkerSlot` inside `state` (with `slot.state` and
            /// `slot.home_cpu` initialized) that outlives this thread.
            unsafe extern "C" fn worker_entry(arg: *mut c_void) -> i32 {
                // SAFETY: Guaranteed by the caller (`stop_workers` joins before `state` drops).
                let slot = unsafe { &*arg.cast::<WorkerSlot>() };
                // SAFETY: `slot.state` was stored with `state_ref` before `worker.resume()`
                // (whose release/acquire synchronization ensures this `Relaxed` load sees non-null
                // `state_ref`), and `state` outlives all workers via `stop_workers`.
                let state = unsafe { &*slot.state.load(Ordering::Relaxed) };
                let home_mask: cpu_mask_t = cpu_num_to_mask(slot.home_cpu.load(Ordering::Relaxed));
                let target_mask: cpu_mask_t = cpu_num_to_mask(state.target_cpu);
                // SAFETY: Guaranteed by the caller (`worker_entry` runs in normal thread context).
                let self_thread = unsafe { ThreadPtr::current() };
                let mut step: u64 = 0;
                while !state.stop.load(Ordering::Relaxed) {
                    if state.probe_target.load(Ordering::Relaxed) && {
                        step += 1;
                        step.is_multiple_of(2)
                    } {
                        // SAFETY: `self_thread` is the running current thread, and no thread lock,
                        // scheduler queue lock, or `ChainLockTransaction` is held.
                        let _ = unsafe { self_thread.set_soft_cpu_affinity(target_mask) };
                    } else {
                        // SAFETY: `self_thread` is the running current thread, and no thread lock,
                        // scheduler queue lock, or `ChainLockTransaction` is held.
                        let _ = unsafe { self_thread.set_soft_cpu_affinity(home_mask) };
                    }
                    state.iterations.fetch_add(1, Ordering::Relaxed);
                    thread::r#yield();
                }
                0
            }

            let slot_ptr: *mut c_void = core::ptr::from_ref(slot).cast_mut().cast();
            let worker = unwrap_ok!(
                // SAFETY: A null `t` allocates the `Thread`, `worker_entry` runs in normal thread
                // context and only accesses `slot` and `state` via shared references, blocking is
                // permitted, no `ChainLockTransaction` is active, and `stop_workers` joins each
                // worker (aborting on error) before `state` is dropped.
                unsafe {
                    thread::create_etc(
                        core::ptr::null_mut(),
                        b"mp-hotplug-soft-affinity-worker",
                        Some(worker_entry),
                        slot_ptr,
                        &scheduler::BaseProfile::new_fair(DEFAULT_PRIORITY),
                        None,
                    )
                },
                "worker create failed"
            );
            // SAFETY: `worker` was just created and has not been resumed, destroyed, or joined
            // yet, and no thread lock, scheduler queue lock, or `ChainLockTransaction` is held.
            unsafe {
                let _ = worker
                    .set_cpu_affinity(cpu_num_to_mask(home_cpu) | cpu_num_to_mask(target_cpu));
                let _ = worker.set_soft_cpu_affinity(cpu_num_to_mask(home_cpu));
            }
            if worker_index.is_multiple_of(2) {
                let slot_const = SendConstPtr::from_ref(slot);
                let worker_migrate_fn =
                    MigrateFn::new(move |_thread: &Thread, stage: MigrateStage| {
                        // SAFETY: `state` (`Pin<Box<State>>`) remains valid and initialized in
                        // the same allocation, and `WorkerSlot` is never mutably borrowed or
                        // mutated outside `UnsafeCell` (`Atomic*`, accessed exclusively under
                        // each worker's `ChainLock` for `save_pending`/`exited`), until all
                        // workers are joined before `state` is dropped.
                        let slot = unsafe { slot_const.as_ref() };
                        match stage {
                            MigrateStage::Save => {
                                if slot.exited.load(Ordering::Relaxed)
                                    || slot.save_pending.load(Ordering::Relaxed)
                                {
                                    slot.migrate_order_ok.store(false, Ordering::Relaxed);
                                }
                                slot.save_pending.store(true, Ordering::Relaxed);
                                slot.migrate_save_calls.fetch_add(1, Ordering::Relaxed);
                            }
                            MigrateStage::Restore => {
                                if slot.exited.load(Ordering::Relaxed)
                                    || !slot.save_pending.load(Ordering::Relaxed)
                                {
                                    slot.migrate_order_ok.store(false, Ordering::Relaxed);
                                }
                                slot.save_pending.store(false, Ordering::Relaxed);
                            }
                            MigrateStage::Exiting => {
                                if slot.exited.load(Ordering::Relaxed) {
                                    slot.migrate_order_ok.store(false, Ordering::Relaxed);
                                }
                                slot.exited.store(true, Ordering::Relaxed);
                            }
                        }
                    });
                // SAFETY: `thread_lists_init` has completed; `worker` was just created and is not
                // erased from lists; and no thread lock, list lock, scheduler queue lock, or
                // `ChainLockTransaction` is held.
                unsafe {
                    worker.set_migrate_fn(Some(worker_migrate_fn));
                }
            }
            // SAFETY: `worker` is non-idle, has not been joined or destroyed, and no spinlocks or
            // `ChainLockTransaction` are held.
            unsafe { worker.resume() };
            slot.thread.store(worker.as_raw(), Ordering::Relaxed);
            worker_index += 1;
        }

        const UNPLUG_CYCLES: i32 = 30;
        for _ in 0..UNPLUG_CYCLES {
            state_ref.probe_target.store(false, Ordering::Relaxed);
            let _ = sleep_relative(DurationMono::from_micros(250));
            state_ref.probe_target.store(true, Ordering::Relaxed);
            assert_ok!(
                mp::unplug_cpu_mask(cpu_num_to_mask(target_cpu), ZX_TIME_INFINITE),
                "unplugging core failed"
            );
            state_ref.probe_target.store(false, Ordering::Relaxed);
            assert_ok!(wait_for_cpu_offline(target_cpu), "waiting for core to go offline failed");
            assert_ok!(hotplug_core(target_cpu), "hotplugging core failed");
            wait_for_cpu_active(target_cpu);
            // Unplug shuts down DPC threads, which start after the core becomes active.
            assert_ok!(
                // SAFETY: Runs after `LK_INIT_LEVEL_THREADING` in normal thread context where
                // blocking is permitted with interrupts enabled and no spinlocks, thread list
                // lock, scheduler queue locks, or active `ChainLockTransaction` held.
                unsafe { wait_for_cpu_dpc_runner(target_cpu) },
                "waiting for core's dpc runner failed"
            );
        }

        stop_workers.call();
        expect_gt!(state_ref.iterations.load(Ordering::Relaxed), 0u64);
        let mut total_migrate_save_calls: u64 = 0;
        for slot in &state_ref.workers {
            total_migrate_save_calls += slot.migrate_save_calls.load(Ordering::Relaxed);
            expect_true!(slot.migrate_order_ok.load(Ordering::Relaxed));
        }
        expect_gt!(total_migrate_save_calls, 0u64);
    }

    // Regression test for https://fxbug.dev/578020457:
    // Concurrently mutate soft and hard CPU affinity on runnable worker threads queued on
    // `target_cpu` while repeatedly unplugging and hotplugging `target_cpu`. Verifies that
    // `Scheduler::migrate_unpinned_threads` and `Scheduler::set_cpu_affinity` do not race on
    // run-queue removal, `unblock_list_node`, or thread migration state when `queue_lock` is
    // released.
    /// test concurrent SetCpuAffinity during core unplug migrations
    #[test]
    fn mp_hotplug_set_affinity_during_unplug_test() {
        // TODO(https://fxbug.dev/42086046): Re-enable test on RISC-V.
        if cfg!(target_arch = "riscv64") {
            kprintln!(
                "skipping test mp_hotplug_set_affinity_during_unplug, hotplug only supported on \
                 x64 and arm64"
            );
            return true;
        }

        let online_secondaries: cpu_mask_t = mp::get_online_mask() & !cpu_num_to_mask(BOOT_CPU_ID);
        if online_secondaries == 0 {
            kprintln!(
                "skipping test mp_hotplug_set_affinity_during_unplug, not enough online cpus"
            );
            return true;
        }
        let target_cpu: cpu_num_t = highest_cpu_set(online_secondaries);

        // SAFETY: Kernel unit tests run after `LK_INIT_LEVEL_THREADING` in normal thread context
        // with no thread lock, scheduler queue lock, or `ChainLockTransaction` held.
        let previous_affinity =
            unsafe { ThreadPtr::current().set_cpu_affinity(cpu_num_to_mask(BOOT_CPU_ID)) };
        let _restore_affinity = zr::defer(move || {
            // SAFETY: Runs after `LK_INIT_LEVEL_THREADING` on the current test thread before
            // returning, with no thread lock, scheduler queue lock, or `ChainLockTransaction` held.
            let _ = unsafe { ThreadPtr::current().set_cpu_affinity(previous_affinity) };
        });

        /// `stop` is a monotonic cancellation flag; counters are checked after `ThreadPtr::join()`.
        /// `workers` uses `AtomicPtr<Thread>` for `Sync` interior mutability because
        /// `cleanup_threads` and already-resumed workers hold `&State` while subsequent worker
        /// slots are initialized; `Ordering::Relaxed` suffices because all slots are populated
        /// before `mutator_thread.resume()` and swapped/joined only after `mutator.join()`.
        struct State {
            stop: AtomicBool,
            worker_iterations: AtomicU64,
            affinity_updates: AtomicU64,
            workers: [AtomicPtr<Thread>; 4],
            target_cpu: cpu_num_t,
        }
        let state = State {
            stop: AtomicBool::new(false),
            worker_iterations: AtomicU64::new(0),
            affinity_updates: AtomicU64::new(0),
            workers: [const { AtomicPtr::new(core::ptr::null_mut()) }; 4],
            target_cpu,
        };
        let state_ptr: *mut c_void = core::ptr::from_ref(&state).cast_mut().cast();

        let mutator: Cell<Option<ThreadPtr>> = Cell::new(None);
        let mut cleanup_threads = zr::defer(|| {
            state.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = mutator.take() {
                // SAFETY: `thread` was created and resumed below, `take()` ensures it is joined at
                // most once, this thread is not `thread`, no `&Thread` or lock is held, and
                // blocking is permitted with no active `ChainLockTransaction`.
                unsafe { thread.join(InstantMono::INFINITE) }.expect("joining mutator failed");
            }
            for worker in &state.workers {
                let raw = worker.swap(core::ptr::null_mut(), Ordering::Relaxed);
                if !raw.is_null() {
                    // SAFETY: `raw` is non-null and was stored from a created and resumed
                    // `ThreadPtr`, and `swap` with null ensures it is joined at most once.
                    let thread = unsafe { ThreadPtr::from_raw(raw) }.unwrap();
                    // SAFETY: `thread` was resumed and taken exclusively above, this thread is not
                    // `thread`, no `&Thread` or lock is held, and blocking is permitted with no
                    // active `ChainLockTransaction`.
                    unsafe { thread.join(InstantMono::INFINITE) }.expect("joining worker failed");
                }
            }
        });

        let _restore_target_cpu = zr::defer(move || {
            if !mp::is_cpu_online(target_cpu) {
                let status = hotplug_core(target_cpu);
                assert!(
                    status.is_ok(),
                    "hotplugging core {} failed: {}",
                    target_cpu,
                    Status::result_into_raw(status),
                );
                wait_for_cpu_active(target_cpu);
                // Unplug shuts down DPC threads, which start after the core becomes active.
                // SAFETY: `_restore_target_cpu` runs at test exit after `LK_INIT_LEVEL_THREADING`
                // in normal thread context where blocking is permitted with interrupts enabled and
                // no spinlocks, thread list lock, scheduler queue locks, or active
                // `ChainLockTransaction` held.
                let dpc_status = unsafe { wait_for_cpu_dpc_runner(target_cpu) };
                assert!(
                    dpc_status.is_ok(),
                    "waiting for core {}'s dpc runner failed: {}",
                    target_cpu,
                    Status::result_into_raw(dpc_status),
                );
            }
        });

        for slot in &state.workers {
            /// Yields in a loop while counting iterations until `state.stop` is set.
            ///
            /// # Safety
            ///
            /// `arg` must point to a valid, shared `State` that outlives this thread.
            unsafe extern "C" fn worker_entry(arg: *mut c_void) -> i32 {
                // SAFETY: Guaranteed by the caller (`cleanup_threads` joins before `state` drops).
                let state = unsafe { &*arg.cast::<State>() };
                while !state.stop.load(Ordering::Relaxed) {
                    state.worker_iterations.fetch_add(1, Ordering::Relaxed);
                    thread::r#yield();
                }
                0
            }

            let worker = unwrap_ok!(
                // SAFETY: A null `t` allocates the `Thread`, `worker_entry` only accesses `state`
                // via shared reference, blocking is permitted, no `ChainLockTransaction` is active,
                // and `cleanup_threads` joins each worker (aborting on error) before `state` goes
                // out of scope.
                unsafe {
                    thread::create_etc(
                        core::ptr::null_mut(),
                        b"mp-hotplug-affinity-worker",
                        Some(worker_entry),
                        state_ptr,
                        &scheduler::BaseProfile::new_fair(DEFAULT_PRIORITY),
                        None,
                    )
                },
                "worker create failed"
            );
            // SAFETY: `worker` was just created and has not been resumed, destroyed, or joined yet,
            // and no spinlocks, scheduler queue locks, or `ChainLockTransaction` are held.
            unsafe {
                let _ = worker.set_soft_cpu_affinity(cpu_num_to_mask(target_cpu));
                worker.resume();
            }
            slot.store(worker.as_raw(), Ordering::Relaxed);
        }

        /// Toggles soft and hard CPU affinity masks across `state.workers` until `state.stop` is
        /// set.
        ///
        /// # Safety
        ///
        /// `arg` must point to a valid, shared `State` whose `workers` array is populated with
        /// live `ThreadPtr`s that remain unjoined until after this thread exits.
        unsafe extern "C" fn mutator_entry(arg: *mut c_void) -> i32 {
            // SAFETY: Guaranteed by the caller (`cleanup_threads` joins `mutator` first).
            let state = unsafe { &*arg.cast::<State>() };
            let target_mask: cpu_mask_t = cpu_num_to_mask(state.target_cpu);
            let boot_or_target: cpu_mask_t = cpu_num_to_mask(BOOT_CPU_ID) | target_mask;
            let mut step: u64 = 0;
            while !state.stop.load(Ordering::Relaxed) {
                for worker in &state.workers {
                    let raw = worker.load(Ordering::Relaxed);
                    assert!(!raw.is_null());
                    // SAFETY: `raw` is verified non-null above; all `state.workers` slots were
                    // populated before `mutator_thread.resume()` (whose release/acquire
                    // synchronization ensures this `Relaxed` load sees the stored `ThreadPtr`) and
                    // remain alive until `cleanup_threads` joins `mutator` before swapping or
                    // joining any worker.
                    let worker_thread = unsafe { ThreadPtr::from_raw(raw) }.unwrap();
                    let soft_mask = if (step & 1) != 0 { target_mask } else { CPU_MASK_ALL };
                    let hard_mask = if (step & 2) != 0 { boot_or_target } else { CPU_MASK_ALL };
                    // SAFETY: `worker_thread` remains alive until `mutator` is joined, and no
                    // thread lock, scheduler queue lock, or `ChainLockTransaction` is held.
                    unsafe {
                        let _ = worker_thread.set_soft_cpu_affinity(soft_mask);
                        let _ = worker_thread.set_cpu_affinity(hard_mask);
                    }
                    state.affinity_updates.fetch_add(1, Ordering::Relaxed);
                }
                step += 1;
                thread::r#yield();
            }
            0
        }

        let mutator_thread = unwrap_ok!(
            // SAFETY: A null `t` allocates the `Thread`, `mutator_entry` only accesses `state` via
            // shared reference after all `state.workers` slots are populated, blocking is
            // permitted, no `ChainLockTransaction` is active, and `cleanup_threads` joins
            // `mutator` (aborting on error) before `state.workers` are joined or `state` drops.
            unsafe {
                thread::create_etc(
                    core::ptr::null_mut(),
                    b"mp-hotplug-affinity-mutator",
                    Some(mutator_entry),
                    state_ptr,
                    &scheduler::BaseProfile::new_fair(DEFAULT_PRIORITY),
                    None,
                )
            },
            "mutator create failed"
        );
        // SAFETY: `mutator_thread` was just created and has not been resumed, destroyed, or joined
        // yet, and no spinlocks, scheduler queue locks, or `ChainLockTransaction` are held.
        unsafe {
            let _ = mutator_thread.set_cpu_affinity(cpu_num_to_mask(BOOT_CPU_ID));
            mutator_thread.resume();
        }
        mutator.set(Some(mutator_thread));

        const UNPLUG_CYCLES: i32 = 20;
        for _ in 0..UNPLUG_CYCLES {
            let _ = sleep_relative(DurationMono::from_millis(1));
            assert_ok!(
                mp::unplug_cpu_mask(cpu_num_to_mask(target_cpu), ZX_TIME_INFINITE),
                "unplugging core failed"
            );
            assert_ok!(wait_for_cpu_offline(target_cpu), "waiting for core to go offline failed");
            assert_ok!(hotplug_core(target_cpu), "hotplugging core failed");
            wait_for_cpu_active(target_cpu);
            // Unplug shuts down DPC threads, which start after the core becomes active.
            assert_ok!(
                // SAFETY: Runs after `LK_INIT_LEVEL_THREADING` in normal thread context where
                // blocking is permitted with interrupts enabled and no spinlocks, thread list
                // lock, scheduler queue locks, or active `ChainLockTransaction` held.
                unsafe { wait_for_cpu_dpc_runner(target_cpu) },
                "waiting for core's dpc runner failed"
            );
        }

        cleanup_threads.call();
        expect_gt!(state.worker_iterations.load(Ordering::Relaxed), 0u64);
        expect_gt!(state.affinity_updates.load(Ordering::Relaxed), 0u64);
    }
}
