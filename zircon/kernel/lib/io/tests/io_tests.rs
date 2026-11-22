// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// io test
#[unittest::suite(name = "io_tests")]
mod tests {
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicBool, Ordering};

    use kprint::kprint;
    use unittest::{assert_gt, expect_eq, expect_true};

    use crate::kernel::auto_preempt_disabler::AutoPreemptDisabler;
    use crate::kernel::deadline::Deadline;
    use crate::kernel::percpu::PerCpu;
    use crate::kernel::thread::{Thread, current_get};
    use crate::kernel::timer::{Timer, ZX_CLOCK_MONOTONIC};
    use crate::platform_rs::timer::{DurationMono, current_mono_time, zx_sec};

    /// Timer callback that prints a line from interrupt context and sets the `AtomicBool` flag at
    /// `arg`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `arg` is non-null, properly aligned, and points to an
    /// initialized `AtomicBool` valid for shared access for the duration of this callback.
    unsafe extern "C" fn interrupt_printf_timer_callback(
        _timer: *mut Timer,
        _now: i64,
        arg: *mut c_void,
    ) {
        kprint!("io_tests: interrupt line\n");
        // SAFETY: By this function's `# Safety` precondition, `arg` is non-null, properly aligned,
        // and points to an initialized `AtomicBool` valid for shared access for the duration of
        // this call.
        unsafe { &*arg.cast::<AtomicBool>() }.store(true, Ordering::SeqCst);
    }

    // Call `serial_write` while holding the thread lock to establish a lock ordering between the
    // thread lock and the uart_serial lock. By establishing the lock ordering, lockdep may be able
    // to detect violations of this ordering. This is a regression test for
    // https://fxbug.dev/42155881.
    /// Verify lock ordering between the thread lock and `serial_write`.
    #[test]
    fn serial_write_holding_thread_lock() {
        // TODO(johngro): Do we still need this test, or something similar, now that the thread lock
        // is gone?
        // Disabled during thread lock breakup:
        // ksync::lock!(let _thread_lock_guard = ThreadLock::get().lock_with(ksync::source_tag!()));
        // serial_write(b"this is a test message from serial_write_holding_thread_lock\n");
    }

    // Check that a `printf` from interrupt context leaves the interrupted thread's partial line
    // alone and that the flush nesting depth returns to zero. This is a regression test for
    // https://fxbug.dev/581353423.
    /// Verify interrupt-context `printf` preserves the interrupted thread's linebuffer.
    #[test]
    fn interrupt_printf_leaves_thread_linebuffer() {
        kprint!("io_tests: thread partial line,");

        // Keep this thread on the CPU so that the timer interrupts it.
        let _preempt_disable = AutoPreemptDisabler::new();

        let lb = Thread::linebuffer_ptr(current_get());
        // SAFETY: `current_get()` is the running test thread with blocking allowed.
        // Interrupt-context prints use the per-CPU `irq_linebuffer` and preemption is disabled, so
        // nothing else writes this thread's linebuffer during this read.
        let pos_before = unsafe { (*lb).pos };
        assert_gt!(pos_before, 0usize);
        // SAFETY: As above.
        let buffer_before = unsafe { (*lb).buffer };

        let timer_fired = AtomicBool::new(false);
        pin_init::stack_pin_init!(let timer = Timer::init(ZX_CLOCK_MONOTONIC));
        // SAFETY: `timer` is pinned on the stack and cancelled below before either `timer` or
        // `timer_fired` goes out of scope. `arg` is a non-null, properly aligned pointer to the
        // initialized `AtomicBool` `timer_fired`, which remains valid for shared access until
        // `timer.cancel()` returns, discharging `interrupt_printf_timer_callback`'s `# Safety`
        // precondition.
        unsafe {
            timer.as_ref().set_deadline(
                &Deadline::no_slack(current_mono_time().into()),
                interrupt_printf_timer_callback,
                core::ptr::from_ref(&timer_fired).cast_mut().cast::<c_void>(),
            );
        }
        let deadline = current_mono_time() + DurationMono(zx_sec(10));
        while !timer_fired.load(Ordering::SeqCst) && current_mono_time() < deadline {
            libarch::r#yield();
        }
        timer.cancel();

        expect_true!(timer_fired.load(Ordering::SeqCst));
        // SAFETY: `lb` is the current thread's linebuffer and the timer has been cancelled.
        let (pos_after_irq, buffer_after_irq) = unsafe { ((*lb).pos, (*lb).buffer) };
        expect_eq!(pos_before, pos_after_irq);
        expect_true!(buffer_before[..pos_before] == buffer_after_irq[..pos_before]);
        expect_eq!(0usize, PerCpu::get_current().stdout_state.flush_depth());

        kprint!(" end\n");
        // SAFETY: `lb` is the current thread's linebuffer after `kprint!` has returned.
        expect_eq!(0usize, unsafe { (*lb).pos });
        expect_eq!(0usize, PerCpu::get_current().stdout_state.flush_depth());
    }
}
