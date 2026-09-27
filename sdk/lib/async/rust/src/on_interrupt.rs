// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::callback_state::CallbackSharedState;
use core::fmt;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering};
use core::task::{Context, Poll};
use futures::Stream;
use futures::task::AtomicWaker;
use libasync_dispatcher::{DetectDispatcher, GetAsyncDispatcher};
use libasync_sys::{
    async_bind_irq, async_dispatcher_t, async_irq, async_irq_t, async_state_t, async_unbind_irq,
};
use std::sync::Arc;
use zx::sys::{ZX_ERR_CANCELED, ZX_OK, zx_packet_interrupt_t, zx_status_t};
use zx::{
    AsHandleRef, BootTimeline, Instant, Interrupt, InterruptKind, RealInterruptKind, Status,
    Timeline,
};

/// Internal state managed for an active IRQ binding.
struct IrqState {
    async_dispatcher: NonNull<async_dispatcher_t>,
    waker: AtomicWaker,
    status: AtomicI32,
    timestamp: AtomicI64,
    raw_ptr_released: AtomicBool,
}

// SAFETY: async_dispatcher_t is thread-safe per libasync API specification.
unsafe impl Send for IrqState {}
unsafe impl Sync for IrqState {}

type SharedState = CallbackSharedState<async_irq, IrqState>;

impl IrqState {
    unsafe extern "C" fn call(
        dispatcher: *mut async_dispatcher_t,
        irq: *mut async_irq_t,
        status: zx_status_t,
        signal: *const zx_packet_interrupt_t,
    ) {
        // SAFETY: irq points to the async_irq at offset 0 of CallbackSharedState.
        // Increment strong count for the duration of this call to ensure the shared
        // state remains valid even if unbind runs concurrently on another thread.
        unsafe { Arc::increment_strong_count(irq as *const SharedState) };
        let state = unsafe { Arc::from_raw(irq as *const SharedState) };

        debug_assert!(
            dispatcher == state.async_dispatcher.as_ptr(),
            "dispatcher pointer mismatch in irq callback"
        );

        if status == ZX_OK {
            debug_assert!(!signal.is_null(), "signal must not be null when status is ZX_OK");
            // SAFETY: signal is non-null and valid when status is ZX_OK per async_irq_handler_t contract.
            let ts = unsafe { (*signal).timestamp };
            state.timestamp.store(ts, Ordering::Relaxed);
            state.status.store(ZX_OK, Ordering::Release);
            state.waker.wake();
        } else if status == ZX_ERR_CANCELED {
            state.status.store(status, Ordering::Release);
            state.waker.wake();

            if state
                .raw_ptr_released
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // SAFETY: The dispatcher will never invoke this callback again after ZX_ERR_CANCELED.
                unsafe { SharedState::release_raw_ptr(irq) };
            }
        }
    }
}

/// A stream that yields notifications whenever a Zircon interrupt fires on a libasync dispatcher.
pub struct OnInterrupt<K: InterruptKind = RealInterruptKind, T: Timeline = BootTimeline> {
    dispatcher: DetectDispatcher,
    interrupt: Option<Interrupt<K, T>>,
    state: Option<Arc<SharedState>>,
}

impl<K: InterruptKind, T: Timeline> OnInterrupt<K, T> {
    /// Creates a new `OnInterrupt` stream bound to the current thread's active dispatcher.
    pub fn new(interrupt: Interrupt<K, T>) -> Self {
        Self { dispatcher: DetectDispatcher::default(), interrupt: Some(interrupt), state: None }
    }

    /// Creates a new `OnInterrupt` stream bound to the specified dispatcher.
    pub fn new_on(dispatcher: impl GetAsyncDispatcher, interrupt: Interrupt<K, T>) -> Self {
        Self {
            dispatcher: DetectDispatcher::new(dispatcher.get_async_dispatcher()),
            interrupt: Some(interrupt),
            state: None,
        }
    }

    /// Acknowledges the interrupt so that it can be triggered again.
    ///
    /// In Zircon, interrupts remain masked after firing until `ack()` is invoked.
    pub fn ack(&self) -> Result<(), Status> {
        self.interrupt.as_ref().ok_or(Status::BAD_STATE)?.ack()
    }

    /// Returns a reference to the underlying `Interrupt`, or `None` if it was taken.
    pub fn interrupt(&self) -> Option<&Interrupt<K, T>> {
        self.interrupt.as_ref()
    }

    /// Cancels interrupt listening and returns the underlying `Interrupt` object.
    pub fn take_interrupt(&mut self) -> Option<Interrupt<K, T>> {
        self.unbind();
        self.interrupt.take()
    }

    fn bind(&mut self) -> Result<(), Status> {
        let interrupt = self.interrupt.as_ref().ok_or(Status::BAD_STATE)?;
        let dispatcher = self.dispatcher.get_or_detect()?;
        let async_dispatcher = dispatcher.as_ptr();

        let base = async_irq {
            state: async_state_t::default(),
            handler: Some(IrqState::call),
            object: interrupt.raw_handle(),
        };

        let inner = IrqState {
            async_dispatcher,
            waker: AtomicWaker::new(),
            status: AtomicI32::new(Status::SHOULD_WAIT.into_raw()),
            timestamp: AtomicI64::new(0),
            raw_ptr_released: AtomicBool::new(false),
        };

        let shared_state = SharedState::new(base, inner);
        let raw_ptr = SharedState::make_raw_ptr(shared_state.clone());
        // SAFETY: async_bind_irq is thread safe per libasync C API doc.
        let status = unsafe { async_bind_irq(async_dispatcher.as_ptr(), raw_ptr) };
        if let Err(err) = Status::ok(status) {
            // SAFETY: Binding failed; callback will never run. Decrement raw ref.
            unsafe { SharedState::release_raw_ptr(raw_ptr) };
            return Err(err);
        }

        self.state = Some(shared_state);
        Ok(())
    }

    fn unbind(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };

        let raw_ptr = SharedState::as_raw_ptr(&state);
        // SAFETY: async_unbind_irq is thread-safe per libasync C API doc.
        let status = unsafe { async_unbind_irq(state.async_dispatcher.as_ptr(), raw_ptr) };

        if Status::ok(status).is_ok()
            && state
                .raw_ptr_released
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            // SAFETY: Successfully unbound. C dispatcher will never invoke the callback again.
            unsafe { SharedState::release_raw_ptr(raw_ptr) };
        }
        // If status == ZX_ERR_BAD_STATE, dispatcher is shutting down; the callback will receive
        // ZX_ERR_CANCELED and decref there, or raw_ptr_released deduplicates if it already ran.
    }
}

impl<K: InterruptKind, T: Timeline> Stream for OnInterrupt<K, T> {
    type Item = Result<Instant<T>, Status>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if this.interrupt.is_none() {
            return Poll::Ready(None);
        }

        if this.state.is_none()
            && let Err(err) = this.bind()
        {
            return Poll::Ready(Some(Err(err)));
        }

        let state = this.state.as_ref().unwrap();
        state.waker.register(cx.waker());

        match state.status.load(Ordering::Acquire) {
            ZX_OK => {
                // Reset status to SHOULD_WAIT for the next event.
                state.status.store(Status::SHOULD_WAIT.into_raw(), Ordering::Release);
                let ts = state.timestamp.load(Ordering::Relaxed);
                Poll::Ready(Some(Ok(Instant::from_nanos(ts))))
            }
            ZX_ERR_CANCELED => {
                this.unbind();
                this.interrupt = None;
                Poll::Ready(Some(Err(Status::CANCELED)))
            }
            s if s == Status::SHOULD_WAIT.into_raw() => Poll::Pending,
            s => Poll::Ready(Some(Err(Status::err_from_raw(s)))),
        }
    }
}

impl<K: InterruptKind, T: Timeline> Drop for OnInterrupt<K, T> {
    fn drop(&mut self) {
        self.unbind();
    }
}

impl<K: InterruptKind, T: Timeline> AsHandleRef for OnInterrupt<K, T> {
    fn as_handle_ref(&self) -> zx::HandleRef<'_> {
        self.as_ref().as_handle_ref()
    }
}

impl<K: InterruptKind, T: Timeline> AsRef<Interrupt<K, T>> for OnInterrupt<K, T> {
    fn as_ref(&self) -> &Interrupt<K, T> {
        self.interrupt.as_ref().expect("OnInterrupt dereferenced after interrupt taken")
    }
}

impl<K: InterruptKind, T: Timeline> Unpin for OnInterrupt<K, T> {}

impl<K: InterruptKind, T: Timeline> fmt::Debug for OnInterrupt<K, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnInterrupt")
            .field("interrupt", &self.interrupt.as_ref().map(|i| i.as_handle_ref()))
            .finish()
    }
}

/// Extension trait adding interrupt listening capabilities to types providing an async dispatcher.
pub trait DispatcherInterruptExt {
    /// Returns an `OnInterrupt` stream bound to this dispatcher.
    fn on_interrupt<K: InterruptKind, T: Timeline>(
        &self,
        interrupt: Interrupt<K, T>,
    ) -> OnInterrupt<K, T>;

    /// Returns an `OnInterrupt` stream bound to this dispatcher, or `None` if no dispatcher is present.
    fn try_on_interrupt<K: InterruptKind, T: Timeline>(
        &self,
        interrupt: Interrupt<K, T>,
    ) -> Option<OnInterrupt<K, T>>;
}

impl<D: GetAsyncDispatcher> DispatcherInterruptExt for D {
    fn on_interrupt<K: InterruptKind, T: Timeline>(
        &self,
        interrupt: Interrupt<K, T>,
    ) -> OnInterrupt<K, T> {
        self.try_on_interrupt(interrupt).expect("No current dispatcher")
    }

    fn try_on_interrupt<K: InterruptKind, T: Timeline>(
        &self,
        interrupt: Interrupt<K, T>,
    ) -> Option<OnInterrupt<K, T>> {
        let dispatcher = self.try_get_async_dispatcher()?;
        Some(OnInterrupt::new_on(dispatcher, interrupt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdf_env::test::spawn_in_driver;
    use futures::{StreamExt, poll};
    use libasync_dispatcher::CurrentDispatcher;
    use std::sync::mpsc;
    use std::task::Waker;
    use std::thread::sleep;
    use std::time::Duration;
    use zx::{
        BootInstant, MonotonicInstant, MonotonicTimeline, VirtualInterrupt, VirtualInterruptKind,
    };

    #[test]
    fn test_bind_and_receive_virtual_interrupt() {
        spawn_in_driver("testing irq wait", async move {
            let interrupt = VirtualInterrupt::create_virtual().unwrap();
            let mut stream = CurrentDispatcher.on_interrupt(interrupt);
            assert_eq!(poll!(stream.next()), Poll::Pending);

            let trigger_time = BootInstant::from_nanos(12345);
            stream.interrupt().unwrap().trigger(trigger_time).unwrap();

            let res = stream.next().await;
            assert_eq!(res, Some(Ok(trigger_time)));

            stream.ack().unwrap();
        });
    }

    #[test]
    fn test_multiple_interrupts_sequential() {
        spawn_in_driver("testing irq multi-shot", async move {
            let interrupt = VirtualInterrupt::create_virtual().unwrap();
            let mut stream = CurrentDispatcher.on_interrupt(interrupt);
            assert_eq!(poll!(stream.next()), Poll::Pending);

            for i in 1..=5 {
                let trigger_time = BootInstant::from_nanos(i * 1000);
                stream.interrupt().unwrap().trigger(trigger_time).unwrap();

                let res = stream.next().await;
                assert_eq!(res, Some(Ok(trigger_time)));

                stream.ack().unwrap();
            }
        });
    }

    #[test]
    fn test_take_interrupt() {
        spawn_in_driver("testing take_interrupt", async move {
            let interrupt = VirtualInterrupt::create_virtual().unwrap();
            let mut stream = CurrentDispatcher.on_interrupt(interrupt);
            assert_eq!(poll!(stream.next()), Poll::Pending);

            let reclaimed_irq = stream.take_interrupt().unwrap();
            assert_eq!(poll!(stream.next()), Poll::Ready(None));

            let trigger_time = BootInstant::from_nanos(54321);
            reclaimed_irq.trigger(trigger_time).unwrap();
            assert_eq!(reclaimed_irq.wait().unwrap(), trigger_time);
        });
    }

    #[test]
    fn test_drop_while_bound() {
        spawn_in_driver("testing drop while bound", async move {
            let interrupt = VirtualInterrupt::create_virtual().unwrap();
            let mut stream = CurrentDispatcher.on_interrupt(interrupt);
            assert_eq!(poll!(stream.next()), Poll::Pending);
            drop(stream);
        });
    }

    #[test]
    fn test_monotonic_timeline_interrupt() {
        spawn_in_driver("testing monotonic timeline irq", async move {
            let interrupt =
                Interrupt::<VirtualInterruptKind, MonotonicTimeline>::create_virtual().unwrap();
            let mut stream = CurrentDispatcher.on_interrupt(interrupt);
            assert_eq!(poll!(stream.next()), Poll::Pending);

            let trigger_time = MonotonicInstant::from_nanos(99999);
            stream.interrupt().unwrap().trigger(trigger_time).unwrap();

            let res = stream.next().await;
            assert_eq!(res, Some(Ok(trigger_time)));

            stream.ack().unwrap();
        });
    }

    #[test]
    fn test_dispatcher_shutdown_cancel() {
        let (stream_tx, stream_rx) = mpsc::channel();
        spawn_in_driver("testing irq shutdown", async move {
            let interrupt = VirtualInterrupt::create_virtual().unwrap();
            let mut stream = CurrentDispatcher.on_interrupt(interrupt);
            assert_eq!(poll!(stream.next()), Poll::Pending);
            stream_tx.send(stream).unwrap();
        });

        let mut stream = stream_rx.recv().unwrap();
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        loop {
            let Poll::Ready(res) = stream.poll_next_unpin(&mut context) else {
                sleep(Duration::from_millis(10));
                continue;
            };
            assert_eq!(res, Some(Err(Status::CANCELED)));
            break;
        }

        // Subsequent poll should yield None
        assert_eq!(stream.poll_next_unpin(&mut context), Poll::Ready(None));
    }
}
